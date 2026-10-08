//! 传输层解密/解压：`battle_proxy::battle::transport_crypto`。
//!
//! 复刻样本中的 `src/battle/transport_crypto.rs`。UE 自带三套可选保护，三角洲手游
//! 的 UDP 通道上都能碰到，且**可逐包嗅探判定**（不同包可能只有其中一两种）：
//!
//! 1. **UE 原生异或 + AES-256-ECB**（`FAES::EncryptData` / `FEncryptionHandler`）：
//!    每 16 字节块先与 AES Key 前 16 字节异或，再做一次 AES-ECB 块加密。
//!    反向：先 AES-ECB 解密，再异或回 Key。
//! 2. **LZ4 块压缩**（`FCompression::COMPRESS_LZ4`）：游戏把大包压缩后再加密，
//!    压缩前长度在包头里；样本链进 `lz4_flex` 正是为此。
//! 3. **纯 XOR 流**（部分版本/部分通道使用的轻量混淆，密钥为滚动 32 位）。
//!
//! 设计原则（与样本一致）：**逐包嗅探 + 失败保留原始字节**。解密失败绝不丢包，
//! 解析器仍会拿到原始数据的只读视图（`DecodeOutcome::Raw`），因为位移等关键字段
//! 常常仍在明文里。

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use aes::Aes256;
use serde::{Deserialize, Serialize};

/// AES 块长度。
pub const AES_BLOCK: usize = 16;

/// 传输保护组合（按嗅探结果记录，用于诊断与 `selftest`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// 明文（未加密、未压缩）—— 部分握手/心跳包。
    Plain,
    /// 仅 UE AES-ECB-XOR。
    AesEcbXor,
    /// AES + LZ4。
    AesEcbXorLz4,
    /// 仅 LZ4。
    Lz4,
    /// 仅滚动 XOR。
    XorStream,
    /// 识别不出，按原始字节处理。
    Unknown,
}

/// 密钥材料。样本里卡密/授权与传输密钥是分开的两套东西；这里只管传输。
#[derive(Debug, Clone, Default)]
pub struct TransportKeys {
    /// UE `FEncryptionHandler` 使用的 32 字节 AES Key。
    pub aes_key: Option<[u8; 32]>,
    /// 滚动 XOR 的种子（32 位）。
    pub xor_seed: u32,
    /// 该连接是否使用 "XOR 前置" 变体（部分小版本把异或放到 AES 之后）。
    pub xor_after_aes: bool,
}

/// 解码结果。`bytes` 始终有效：失败时等于原始输入。
#[derive(Debug, Clone)]
pub struct Decoded {
    pub kind: TransportKind,
    pub bytes: Vec<u8>,
    /// 是否发生了实际变换（用于统计 `udp_invalid_packets`）。
    pub transformed: bool,
}

impl Decoded {
    pub fn raw(input: &[u8]) -> Self {
        Self { kind: TransportKind::Plain, bytes: input.to_vec(), transformed: false }
    }
}

/// UE `FAES::EncryptData`：每块先异或 Key 前 16 字节，再 AES-ECB 加密。
pub fn aes_encrypt_data(key: &[u8; 32], data: &mut [u8]) {
    let cipher = Aes256::new(GenericArray::from_slice(key));
    for chunk in data.chunks_mut(AES_BLOCK) {
        if chunk.len() < AES_BLOCK {
            break;
        }
        for (i, b) in chunk.iter_mut().enumerate() {
            *b ^= key[i];
        }
        let block = GenericArray::from_mut_slice(chunk);
        cipher.encrypt_block(block);
    }
}

/// UE `FAES::DecryptData`：先 AES-ECB 解密，再异或 Key 前 16 字节。
pub fn aes_decrypt_data(key: &[u8; 32], data: &mut [u8]) {
    let cipher = Aes256::new(GenericArray::from_slice(key));
    for chunk in data.chunks_mut(AES_BLOCK) {
        if chunk.len() < AES_BLOCK {
            break;
        }
        let block = GenericArray::from_mut_slice(chunk);
        cipher.decrypt_block(block);
        for (i, b) in chunk.iter_mut().enumerate() {
            *b ^= key[i];
        }
    }
}

/// 滚动 XOR：`state = state * 0x41C64E6D + 0x3039`，按字节取高位。
pub struct XorStream {
    state: u32,
}

impl XorStream {
    pub fn new(seed: u32) -> Self {
        Self { state: if seed == 0 { 0x9E37_79B9 } else { seed } }
    }
    #[inline]
    pub fn next_byte(&mut self) -> u8 {
        self.state = self.state.wrapping_mul(0x41C6_4E6D).wrapping_add(0x3039);
        (self.state >> 24) as u8
    }
    pub fn apply(&mut self, data: &mut [u8]) {
        for b in data.iter_mut() {
            *b ^= self.next_byte();
        }
    }
}

/// LZ4 解压：样本通过 `lz4_flex` 做块解压，长度由包头给出。
pub fn lz4_decompress_block(input: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    if expected_len == 0 || expected_len > 4 * 1024 * 1024 {
        return None;
    }
    lz4_flex::block::decompress(input, expected_len).ok()
}

/// 试探性判断一段数据是否为 LZ4 块（UE 的压缩包首字节高 4 位是字面量长度）。
pub fn looks_like_lz4(input: &[u8], expected_len: usize) -> bool {
    if input.len() < 4 || expected_len == 0 || expected_len > 4 * 1024 * 1024 {
        return false;
    }
    lz4_flex::block::decompress(input, expected_len).is_ok()
}

/// 逐包解码：按 `Plain → LZ4 → AES → AES+LZ4 → XOR` 顺序嗅探，取第一个"结构成立"的结果。
///
/// `validate` 是调用方注入的结构校验（例如 UE 包头的 `PacketId`/`AckPacketId` 合理、
/// 对齐后能读出合法 bunch 头）。这是样本里 `server_decode_gate` 的实际语义：
/// **没有通过校验就不进入解析队列**，但仍然计入转发字节数。
pub fn decode_packet<F>(
    input: &[u8],
    keys: &TransportKeys,
    expected_uncompressed: Option<usize>,
    validate: F,
) -> Decoded
where
    F: Fn(&[u8]) -> bool,
{
    if input.is_empty() {
        return Decoded::raw(input);
    }

    // 1) 明文优先：绝大多数小包（移动/心跳）是明文或仅压缩。
    if validate(input) {
        return Decoded { kind: TransportKind::Plain, bytes: input.to_vec(), transformed: false };
    }

    // 2) 仅 LZ4。
    if let Some(len) = expected_uncompressed {
        if looks_like_lz4(input, len) {
            if let Some(out) = lz4_decompress_block(input, len) {
                if validate(&out) {
                    return Decoded { kind: TransportKind::Lz4, bytes: out, transformed: true };
                }
            }
        }
    }

    // 3) 仅 AES。
    if let Some(key) = keys.aes_key.as_ref() {
        if input.len() >= AES_BLOCK {
            let mut buf = input.to_vec();
            aes_decrypt_data(key, &mut buf);
            if keys.xor_after_aes {
                let mut s = XorStream::new(keys.xor_seed);
                s.apply(&mut buf);
            }
            if validate(&buf) {
                return Decoded { kind: TransportKind::AesEcbXor, bytes: buf, transformed: true };
            }
            // 3b) AES + LZ4。
            if let Some(len) = expected_uncompressed {
                if let Some(out) = lz4_decompress_block(&buf, len) {
                    if validate(&out) {
                        return Decoded {
                            kind: TransportKind::AesEcbXorLz4,
                            bytes: out,
                            transformed: true,
                        };
                    }
                }
            }
        }
    }

    // 4) 滚动 XOR（末位兜底，样本里 `unresolved_bunch_header_variant` 的常见来源）。
    if keys.xor_seed != 0 {
        let mut buf = input.to_vec();
        let mut s = XorStream::new(keys.xor_seed);
        s.apply(&mut buf);
        if validate(&buf) {
            return Decoded { kind: TransportKind::XorStream, bytes: buf, transformed: true };
        }
    }

    // 全部失败：保留原始字节，由上层计入 `udp_invalid_packets`。
    Decoded { kind: TransportKind::Unknown, bytes: input.to_vec(), transformed: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        k
    }

    #[test]
    fn aes_ue_roundtrip() {
        let k = key();
        let orig: Vec<u8> = (0..64u8).collect();
        let mut buf = orig.clone();
        aes_encrypt_data(&k, &mut buf);
        assert_ne!(buf, orig);
        aes_decrypt_data(&k, &mut buf);
        assert_eq!(buf, orig);
    }

    #[test]
    fn aes_is_block_aligned_and_ignores_tail() {
        let k = key();
        let mut buf = vec![0u8; 20];
        let tail = [buf[16], buf[17], buf[18], buf[19]];
        aes_encrypt_data(&k, &mut buf);
        assert_eq!(&buf[16..20], &tail, "partial tail must be untouched");
    }

    #[test]
    fn xor_stream_is_deterministic_and_seed_dependent() {
        let mut a = XorStream::new(0x1234_5678);
        let mut b = XorStream::new(0x1234_5678);
        let mut c = XorStream::new(0x8765_4321);
        let x = a.next_byte();
        assert_eq!(x, b.next_byte());
        assert_ne!(x, c.next_byte());
    }

    #[test]
    fn decode_prefers_plaintext_when_validator_accepts() {
        let keys = TransportKeys { aes_key: Some(key()), xor_seed: 0, xor_after_aes: false };
        let d = decode_packet(b"hello", &keys, None, |_| true);
        assert_eq!(d.kind, TransportKind::Plain);
        assert!(!d.transformed);
    }

    #[test]
    fn decode_falls_back_to_raw_on_unrecognised_input() {
        let keys = TransportKeys::default();
        let d = decode_packet(&[1, 2, 3, 4], &keys, None, |_| false);
        assert_eq!(d.kind, TransportKind::Unknown);
        assert_eq!(d.bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn decode_recognises_aes_roundtrip() {
        let k = key();
        let plain = b"BATTLE packet payload 0123456789ABCDEF".to_vec();
        let mut enc = plain.clone();
        aes_encrypt_data(&k, &mut enc);
        let keys = TransportKeys { aes_key: Some(k), xor_seed: 0, xor_after_aes: false };
        let d = decode_packet(&enc, &keys, None, |c| c == &plain[..]);
        assert_eq!(d.kind, TransportKind::AesEcbXor);
        assert_eq!(d.bytes, plain);
    }

    #[test]
    fn lz4_block_roundtrip_through_decoder() {
        let payload = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBBBCCCCCC".to_vec();
        let comp = lz4_flex::block::compress(&payload);
        let keys = TransportKeys::default();
        let d = decode_packet(&comp, &keys, Some(payload.len()), |c| c == &payload[..]);
        assert_eq!(d.kind, TransportKind::Lz4);
        assert_eq!(d.bytes, payload);
    }
}
