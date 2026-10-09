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
//! 4. **XTEA（64 轮）+ 8 键 bank** —— 参考实现 `MyRaderPro` 那条机内解密链
//!    （`docs/REFERENCE_DECRYPT_PATH.md` §1）。**这一层以前完全缺失**，是"解不出
//!    实体"的最直接原因（报告 §8/R1）。详见 [`super::xtea`]。
//!    与前三条不同，它**需要外部密钥材料**（128 B bank），所以只在
//!    `TransportKeys::xtea_bank` 装上以后才参与嗅探；`None` 时本文件的行为与
//!    历史版本**逐字节一致**。
//!
//! 设计原则（与样本一致）：**逐包嗅探 + 失败保留原始字节**。解密失败绝不丢包，
//! 解析器仍会拿到原始数据的只读视图（`DecodeOutcome::Raw`），因为位移等关键字段
//! 常常仍在明文里。
//!
//! 诊断（报告 §8/R4）：失败**不再静默**。装一把 [`TransportDiagnostics`] 之后，
//! "XTEA 解出来不像合法 bunch"（计数 + 一次性日志 + bank 指纹）与"最后回落原始字节"
//! 都会留下痕迹，线上才能区分"密钥错"和"协议变体错"。

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use aes::Aes256;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::xtea::{XTEA_BLOCK_LEN, XteaKeyBank};

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
    /// XTEA（64 轮）+ 8 键 bank 按 8 字节块轮换（参考实现 UDP 剖面）。
    XteaBank,
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
    /// **参考实现的 UDP XTEA bank（8 × 16 B = 128 B）**，按 8 字节块序号轮换。
    ///
    /// `None`（默认）= 没装银行 → 嗅探链与历史版本**逐字节一致**，既有测试不受影响。
    /// 装上以后 `decode_packet` 会在明文之后、LZ4/AES/XOR 之前先试 XTEA 剖面。
    /// 上游（"从 TCP 提取 8 把钥匙"的那个模块）只管往 [`XteaKeyBank`] 里塞钥匙，
    /// 然后用 [`TransportKeys::install_xtea_bank`] 装进来。
    pub xtea_bank: Option<Arc<XteaKeyBank>>,
    /// 诊断出口：计数 + 一次性日志。`None` = 不打诊断（历史行为）。
    ///
    /// 生产路径由 `BattleEngine` 默认装上一个；测试/离线回放可以留 `None` 保持沉默。
    pub diag: Option<Arc<TransportDiagnostics>>,
}

impl TransportKeys {
    /// 装上/替换 XTEA bank（清零诊断里的"已装上"判断随之变化）。
    pub fn install_xtea_bank(&mut self, bank: XteaKeyBank) {
        self.xtea_bank = Some(Arc::new(bank));
    }

    /// 卸下 XTEA bank：立刻回到"只有 Plain/LZ4/AES/XOR"的历史嗅探链。
    pub fn clear_xtea_bank(&mut self) {
        self.xtea_bank = None;
    }
}

/// 传输层失败诊断：**计数 + 一次性日志**（修报告 §8/R4：历史上失败是静默降级）。
///
/// 参考实现有一整套密钥类指标（`lastKeyFingerprint` / `tgcpKeyProbeMaxUs` /
/// `候选密钥`/`已验证绑定`），我们最少先做到"能区分是密钥错还是协议变体错"：
///
/// * 装了 bank、XTEA 解出来却过不了结构校验 ⇒ `xtea_rejected`（+ 当时 bank 的指纹）；
/// * 什么都没识别出来、回落原始字节 ⇒ `fallback_raw`（+ 当时是否装了 bank）。
///
/// 日志各只打**第一条**（避免每包刷屏），后面的同类包只累加计数。
#[derive(Debug, Default)]
pub struct TransportDiagnostics {
    xtea_accepted: AtomicU64,
    xtea_rejected: AtomicU64,
    fallback_raw: AtomicU64,
    last_bank_fingerprint: AtomicU64,
    xtea_reject_logged: AtomicBool,
    fallback_logged: AtomicBool,
}

/// 诊断快照（给 status/日志用；不含密钥材料，只有计数与指纹）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportDiagnosticsSnapshot {
    /// XTEA 剖面通过结构校验的包数。
    pub xtea_accepted: u64,
    /// 装了 bank 但解出来不像合法 bunch 的包数。
    pub xtea_rejected: u64,
    /// 所有剖面都失败、回落原始字节的包数（`udp_invalid_packets` 的细分量）。
    pub fallback_raw: u64,
    /// 最近一次 XTEA 尝试所用 bank 的 FNV-1a 指纹（0 = 还没试过）。
    pub last_bank_fingerprint: u64,
}

impl TransportDiagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> TransportDiagnosticsSnapshot {
        TransportDiagnosticsSnapshot {
            xtea_accepted: self.xtea_accepted.load(Ordering::Relaxed),
            xtea_rejected: self.xtea_rejected.load(Ordering::Relaxed),
            fallback_raw: self.fallback_raw.load(Ordering::Relaxed),
            last_bank_fingerprint: self.last_bank_fingerprint.load(Ordering::Relaxed),
        }
    }

    fn record_xtea_accept(&self, bank: &XteaKeyBank) {
        self.xtea_accepted.fetch_add(1, Ordering::Relaxed);
        self.last_bank_fingerprint.store(bank.fingerprint(), Ordering::Relaxed);
    }

    fn record_xtea_reject(&self, len: usize, bank: &XteaKeyBank) {
        self.xtea_rejected.fetch_add(1, Ordering::Relaxed);
        let fingerprint = bank.fingerprint();
        self.last_bank_fingerprint.store(fingerprint, Ordering::Relaxed);
        if !self.xtea_reject_logged.swap(true, Ordering::Relaxed) {
            let fp = format!("{fingerprint:016x}");
            tracing::warn!(
                len,
                bank_fingerprint = fp.as_str(),
                all_zero_bank = bank.is_all_zero(),
                "xtea bank 解出来的字节不像合法 bunch（密钥不对 / 轮换规则不对 / 该流本来不是 XTEA）；\
                 继续按旧剖面嗅探，后续同类包只计数不再打日志"
            );
        }
    }

    fn record_fallback_raw(&self, len: usize, bank_present: bool) {
        self.fallback_raw.fetch_add(1, Ordering::Relaxed);
        if !self.fallback_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                len,
                xtea_bank_installed = bank_present,
                "传输层所有剖面都没过结构校验：保留原始字节交给解析器（不丢包）；\
                 后续同类包只计数不再打日志"
            );
        }
    }
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
    /// `seed == 0` 时的哨兵值 `0x9E37_79B9` 与 [`super::xtea::XTEA_DELTA`] **是同一个数**。
    ///
    /// 这是历史混淆，报告 §8/R2 专门点了它：TEA/XTEA 家族的 delta 常量当初被当成了
    /// "滚动 XOR 的缺省种子"，容易让人以为"加密已经处理过了"。两者语义毫无关系 ——
    /// XTEA 里它是每轮累加/递减的 32 位常量，而且样本用的是它的**补码**
    /// `0x61C88647`（`sub` 方向）。这里把字面量换成常量引用，只为让这层混淆在代码里
    /// 看得见：数值不变（既有行为逐字节一致），但**别**再从其中一个推另一个。
    pub fn new(seed: u32) -> Self {
        Self { state: if seed == 0 { super::xtea::XTEA_DELTA } else { seed } }
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

/// 逐包解码：按 `Plain → XTEA(bank) → LZ4 → AES → AES+LZ4 → XOR` 顺序嗅探，
/// 取第一个"结构成立"的结果。
///
/// `validate` 是调用方注入的结构校验（例如 UE 包头的 `PacketId`/`AckPacketId` 合理、
/// 对齐后能读出合法 bunch 头）。这是样本里 `server_decode_gate` 的实际语义：
/// **没有通过校验就不进入解析队列**，但仍然计入转发字节数。
///
/// XTEA 剖面的位置与理由：
///
/// * **明文仍然最优先**。样本那条链是"进了队列就无条件解密"（报告 §1.3 / 推断 I4），
///   但我们的嗅探器是逐包的，"明文包与密文包是否混流"还没实抓验证（报告 §9 第 3 条）。
///   把 XTEA 放在明文前面，会让"本来就是明文"的包有概率被解成另一份也过得了松校验的
///   垃圾 —— 那是净损失。所以顺序是"明文 → XTEA → 旧的三个剖面"：**装 bank 后 XTEA
///   优先于全部旧剖面**，但绝不抢明文的活。
/// * **装了 bank 才走这条**。`xtea_bank == None` 时整个分支不存在，返回值与历史
///   版本逐字节相同（既有测试因此不受影响）。
/// * **解不出来不静默**：记 `xtea_rejected` 计数 + 打一条一次性日志（带 bank 指纹），
///   然后继续按旧剖面嗅探，最后仍然保住原始字节。
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

    // 2) XTEA + 8 键 bank（仅在装了 bank 时；参考实现 UDP 剖面）。
    //    判定"解密成功"只能靠结构校验：XTEA 无 MAC，任何密钥都会产出像随机数的字节，
    //    密码学上无法自证。样本用的门也是同一个（`server_decode_gate`）。
    if let Some(bank) = keys.xtea_bank.as_deref() {
        // 不足 8 字节时样本一个块都不处理（`ands x8, x26, #~7`，vm 0x100067c04），
        // 解出来必然等于原文，试了也白试。
        if input.len() >= XTEA_BLOCK_LEN && !bank.is_all_zero() {
            let mut buf = input.to_vec();
            bank.decrypt_in_place(&mut buf);
            if validate(&buf) {
                if let Some(diag) = keys.diag.as_deref() {
                    diag.record_xtea_accept(bank);
                }
                return Decoded { kind: TransportKind::XteaBank, bytes: buf, transformed: true };
            }
            if let Some(diag) = keys.diag.as_deref() {
                diag.record_xtea_reject(input.len(), bank);
            }
        }
    }

    // 3) 仅 LZ4。
    if let Some(len) = expected_uncompressed {
        if looks_like_lz4(input, len) {
            if let Some(out) = lz4_decompress_block(input, len) {
                if validate(&out) {
                    return Decoded { kind: TransportKind::Lz4, bytes: out, transformed: true };
                }
            }
        }
    }

    // 4) 仅 AES。
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
            // 4b) AES + LZ4。
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

    // 5) 滚动 XOR（末位兜底，样本里 `unresolved_bunch_header_variant` 的常见来源）。
    if keys.xor_seed != 0 {
        let mut buf = input.to_vec();
        let mut s = XorStream::new(keys.xor_seed);
        s.apply(&mut buf);
        if validate(&buf) {
            return Decoded { kind: TransportKind::XorStream, bytes: buf, transformed: true };
        }
    }

    // 全部失败：保留原始字节，由上层计入 `udp_invalid_packets`。
    // 但不再静默（报告 §8/R4）：计数 + 一次性日志，并记下当时有没有 bank。
    if let Some(diag) = keys.diag.as_deref() {
        diag.record_fallback_raw(input.len(), keys.xtea_bank.is_some());
    }
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
        let keys = TransportKeys {
            aes_key: Some(key()),
            xor_seed: 0,
            xor_after_aes: false,
            ..TransportKeys::default()
        };
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
        let keys = TransportKeys {
            aes_key: Some(k),
            xor_seed: 0,
            xor_after_aes: false,
            ..TransportKeys::default()
        };
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

    /// 可辨识 bank：`bank[i][j] = i*16 + j`（8 块明文正好覆盖全部 8 把钥匙）。
    fn demo_bank() -> XteaKeyBank {
        let mut keys = [[0u8; 16]; 8];
        for (i, k) in keys.iter_mut().enumerate() {
            for (j, b) in k.iter_mut().enumerate() {
                *b = (i * 16 + j) as u8;
            }
        }
        XteaKeyBank::from_keys(keys)
    }

    /// 64 字节明文（8 块），装 bank 后必须走 XTEA 剖面并把 8 把钥匙都用上。
    fn xtea_fixture() -> (XteaKeyBank, Vec<u8>, Vec<u8>) {
        let bank = demo_bank();
        let plain: Vec<u8> = b"0123456789ABCDEF".repeat(4);
        let mut enc = plain.clone();
        bank.encrypt_in_place(&mut enc);
        assert_ne!(enc, plain);
        (bank, plain, enc)
    }

    /// 装了 bank ⇒ 走 XTEA 剖面；没装 bank ⇒ 同一个包回落原始字节（旧行为）。
    #[test]
    fn decode_uses_xtea_only_when_a_bank_is_installed() {
        let (bank, plain, enc) = xtea_fixture();

        // 没装 bank：与历史版本完全一致（未知 → 原始字节），诊断也不动。
        let bare = TransportKeys::default();
        let d = decode_packet(&enc, &bare, None, |c| c == &plain[..]);
        assert_eq!(d.kind, TransportKind::Unknown, "没装 bank 时不许有 XTEA 行为");
        assert_eq!(d.bytes, enc);
        assert!(!d.transformed);

        // 装上 bank：同一份输入必须被认出来。
        let diag = Arc::new(TransportDiagnostics::new());
        let keys = TransportKeys {
            xtea_bank: Some(Arc::new(bank)),
            diag: Some(diag.clone()),
            ..TransportKeys::default()
        };
        let d = decode_packet(&enc, &keys, None, |c| c == &plain[..]);
        assert_eq!(d.kind, TransportKind::XteaBank);
        assert_eq!(d.bytes, plain);
        assert!(d.transformed);
        let snap = diag.snapshot();
        assert_eq!(snap.xtea_accepted, 1);
        assert_eq!(snap.xtea_rejected, 0);
        assert_eq!(snap.fallback_raw, 0);
        assert_eq!(snap.last_bank_fingerprint, bank.fingerprint());

        // 卸下 bank 后又必须回到旧行为（install/clear 对称）。
        let mut keys2 = TransportKeys::default();
        keys2.install_xtea_bank(bank);
        assert!(keys2.xtea_bank.is_some());
        keys2.clear_xtea_bank();
        assert!(keys2.xtea_bank.is_none());
        assert_eq!(decode_packet(&enc, &keys2, None, |c| c == &plain[..]).kind, TransportKind::Unknown);
    }

    /// 明文最优先：装上 bank 也不许把合法明文解成别的东西（XTEA 在明文之后）。
    #[test]
    fn decode_still_prefers_plaintext_with_a_bank_installed() {
        let (bank, plain, _enc) = xtea_fixture();
        let mut keys = TransportKeys::default();
        keys.install_xtea_bank(bank);
        let d = decode_packet(&plain, &keys, None, |_| true);
        assert_eq!(d.kind, TransportKind::Plain);
        assert_eq!(d.bytes, plain);
        assert!(!d.transformed);
    }

    /// 诊断：装了 bank 但解出来不像合法 bunch ⇒ 计数 + 一次性日志（不静默降级）。
    #[test]
    fn xtea_rejection_and_raw_fallback_are_counted() {
        let diag = Arc::new(TransportDiagnostics::new());
        let mut keys = TransportKeys::default();
        keys.install_xtea_bank(demo_bank());
        keys.diag = Some(diag.clone());

        // 全是垃圾：XTEA 试了但过不了门，最后回落原始字节。
        for _ in 0..3 {
            let d = decode_packet(&[0x5A; 32], &keys, None, |_| false);
            assert_eq!(d.kind, TransportKind::Unknown);
            assert_eq!(d.bytes, vec![0x5A; 32]);
        }
        let snap = diag.snapshot();
        assert_eq!(snap.xtea_rejected, 3, "每次失败都要计数");
        assert_eq!(snap.fallback_raw, 3, "回落到原始字节也要计数");
        assert_eq!(snap.xtea_accepted, 0);
        assert_eq!(snap.last_bank_fingerprint, demo_bank().fingerprint());

        // 一次性日志：第二次之后的同类包只累加计数（这里用内部标志断言"只打了一次"）。
        assert!(diag.xtea_reject_logged.load(Ordering::Relaxed));
        assert!(diag.fallback_logged.load(Ordering::Relaxed));

        // 短包（< 8 字节）不触发 XTEA：样本一个块都不处理。
        let before = diag.snapshot();
        let d = decode_packet(&[1, 2, 3, 4], &keys, None, |_| false);
        assert_eq!(d.kind, TransportKind::Unknown);
        let after = diag.snapshot();
        assert_eq!(after.xtea_rejected, before.xtea_rejected);
        assert_eq!(after.fallback_raw, before.fallback_raw + 1);
    }

    /// 全 0 的占位 bank 不算"装上了钥匙"：不许把解密结果当成功。
    #[test]
    fn zeroed_bank_does_not_claim_success() {
        let diag = Arc::new(TransportDiagnostics::new());
        let mut keys = TransportKeys::default();
        keys.install_xtea_bank(XteaKeyBank::default());
        keys.diag = Some(diag.clone());
        let d = decode_packet(&[0x11; 16], &keys, None, |_| false);
        assert_eq!(d.kind, TransportKind::Unknown);
        assert_eq!(diag.snapshot().xtea_rejected, 0, "占位 bank 不该产生 XTEA 尝试");
        assert_eq!(diag.snapshot().fallback_raw, 1);
    }
}
