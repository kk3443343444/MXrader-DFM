//! 服务端→客户端消息分帧：`battle_proxy::battle::codec::s2c`。
//!
//! 复刻样本 `src/battle/codec/s2c.rs`。样本在这个模块里留了三个词：
//! `request_reference_bytes`、`buffered_bytes`、`buffered_byte`——说明它维护的
//! 是一个**跨包的字节缓冲**：因为 TCP 侧的复制流没有包边界，而 UDP 侧的 bunch
//! 又可能被 partial 拆开，两边都需要"攒够再解"。
//!
//! 本模块提供两样东西：
//! 1. `S2cStream` —— 按 UE 的 `FOutBunch` 长度前缀（`uint16 BunchDataBits`）切分
//!    一条流，产出完整消息；
//! 2. `RefByteGuard` —— 对含 `bHasPackageMapExports` 的消息，把"引用字节"
//!    （netguid 引用表）单独截图，交给 `udpxin_exports` 解析，剩余净荷再交给
//!    `character`。样本里的 `request_reference_bytes` 就是这个截图动作。

use bytes::{Buf, BytesMut};

/// 一条完整消息：引用字节 + 净荷。
#[derive(Debug, Clone, PartialEq)]
pub struct S2cMessage {
    /// 是否带 PackageMap 导出。
    pub has_exports: bool,
    /// `request_reference_bytes`：引用/导出区原始字节。
    pub reference_bytes: Vec<u8>,
    /// 属性净荷（位对齐后）。
    pub payload: Vec<u8>,
    /// 净荷位长度（UE 用位而非字节计数）。
    pub payload_bits: u32,
}

/// 流式分帧器。
#[derive(Debug, Default)]
pub struct S2cStream {
    buf: BytesMut,
    /// 最多缓存多少字节，防止异常流把内存吃光。
    max_buffered: usize,
    /// 被丢弃的字节数（诊断：`buffered_bytes`）。
    dropped: u64,
    total_consumed: u64,
}

impl S2cStream {
    pub fn new() -> Self {
        Self { buf: BytesMut::new(), max_buffered: 4 * 1024 * 1024, dropped: 0, total_consumed: 0 }
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
    pub fn total_consumed(&self) -> u64 {
        self.total_consumed
    }

    /// 喂入原始字节（调用方已做传输层解密）。
    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if self.buf.len() > self.max_buffered {
            // 丢最旧的：宁可丢历史也不要 OOM，并且缓存量必须落在上限内。
            let overflow = self.buf.len() - self.max_buffered;
            self.buf.advance(overflow);
            self.dropped += overflow as u64;
            tracing::warn!(target: "battle_proxy", dropped = overflow, "s2c buffer overflow");
        }
    }

    /// 取出一条完整消息（无则 `None`）。
    ///
    /// 帧格式（r39 的保守版本）：`uint16 bit_len` + `uint16 flags` + 净荷。
    /// `flags & 1` = 带导出。
    pub fn next_message(&mut self) -> Option<S2cMessage> {
        if self.buf.len() < 4 {
            return None;
        }
        let bit_len = u16::from_le_bytes([self.buf[0], self.buf[1]]) as usize;
        let flags = u16::from_le_bytes([self.buf[2], self.buf[3]]);
        if bit_len > 65_500 {
            // 不合理：丢掉这 4 个字节重新同步。
            self.buf.advance(4);
            self.dropped += 4;
            return None;
        }
        let byte_len = bit_len.div_ceil(8);
        if self.buf.len() < 4 + byte_len {
            return None;
        }
        self.buf.advance(4);
        let payload = self.buf.split_to(byte_len).to_vec();
        self.total_consumed += (4 + byte_len) as u64;

        let has_exports = flags & 1 != 0;
        if has_exports {
            let (reference_bytes, rest) = RefByteGuard::split(&payload);
            Some(S2cMessage {
                has_exports,
                reference_bytes,
                payload: rest,
                payload_bits: bit_len as u32,
            })
        } else {
            Some(S2cMessage {
                has_exports,
                reference_bytes: Vec::new(),
                payload,
                payload_bits: bit_len as u32,
            })
        }
    }

    /// 一口气取出所有完整消息。
    pub fn drain(&mut self) -> Vec<S2cMessage> {
        let mut out = Vec::new();
        while let Some(m) = self.next_message() {
            out.push(m);
            if out.len() >= 512 {
                break;
            }
        }
        out
    }
}

/// 引用字节截图器。
pub struct RefByteGuard;

impl RefByteGuard {
    /// 截图规则：净荷首部 `uint16 ref_byte_len`，其后是引用区。
    /// 无该前缀时（损坏/旧版本）返回空引用区与完整净荷。
    pub fn split(payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
        if payload.len() < 2 {
            return (Vec::new(), payload.to_vec());
        }
        let len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        if len == 0 || len + 2 > payload.len() {
            return (Vec::new(), payload.to_vec());
        }
        (payload[2..2 + len].to_vec(), payload[2 + len..].to_vec())
    }

    /// 截图出的引用区还原成完整包（回写测试用）。
    pub fn join(reference: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(reference.len() + payload.len() + 2);
        out.extend_from_slice(&(reference.len() as u16).to_le_bytes());
        out.extend_from_slice(reference);
        out.extend_from_slice(payload);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(flags: u16, payload: &[u8], bits: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(&flags.to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn parses_a_single_message() {
        let mut s = S2cStream::new();
        s.feed(&frame(0, &[0xAA, 0xBB], 16));
        let m = s.next_message().unwrap();
        assert!(!m.has_exports);
        assert_eq!(m.payload, vec![0xAA, 0xBB]);
        assert_eq!(m.payload_bits, 16);
        assert!(s.next_message().is_none());
    }

    #[test]
    fn waits_for_complete_frame() {
        let mut s = S2cStream::new();
        let f = frame(0, &[1, 2, 3, 4], 32);
        s.feed(&f[..3]);
        assert!(s.next_message().is_none());
        s.feed(&f[3..]);
        assert!(s.next_message().is_some());
    }

    #[test]
    fn sub_byte_payload_rounds_up() {
        let mut s = S2cStream::new();
        s.feed(&frame(0, &[0x01], 3));
        let m = s.next_message().unwrap();
        assert_eq!(m.payload.len(), 1);
        assert_eq!(m.payload_bits, 3);
    }

    #[test]
    fn export_flag_splits_reference_bytes() {
        let reference = vec![0x11, 0x22, 0x33];
        let payload = vec![0x44, 0x55];
        let body = RefByteGuard::join(&reference, &payload);
        let mut s = S2cStream::new();
        s.feed(&frame(1, &body, (body.len() * 8) as u16));
        let m = s.next_message().unwrap();
        assert!(m.has_exports);
        assert_eq!(m.reference_bytes, reference);
        assert_eq!(m.payload, payload);
    }

    #[test]
    fn malformed_reference_length_falls_back_to_full_payload() {
        let (r, p) = RefByteGuard::split(&[0xFF, 0xFF, 1, 2, 3]);
        assert!(r.is_empty());
        assert_eq!(p, vec![0xFF, 0xFF, 1, 2, 3]);
    }

    #[test]
    fn drain_multiple_and_buffer_overflow_is_bounded() {
        let mut s = S2cStream::new();
        s.feed(&frame(0, &[1], 8));
        s.feed(&frame(0, &[2], 8));
        assert_eq!(s.drain().len(), 2);
        s.max_buffered = 8;
        s.feed(&[0u8; 64]);
        assert!(s.buffered() <= 8);
        assert!(s.dropped() > 0);
    }

    #[test]
    fn never_panics_on_random_bytes() {
        let mut s = S2cStream::new();
        for seed in 0u32..64 {
            let data: Vec<u8> = (0..64)
                .map(|i| (seed.wrapping_mul(31).wrapping_add(i * 7) & 0xFF) as u8)
                .collect();
            s.feed(&data);
            let _ = s.drain();
        }
    }
}
