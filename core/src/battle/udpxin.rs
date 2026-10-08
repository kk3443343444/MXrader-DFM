//! UDP 包分帧：`battle_proxy::battle::udpxin`。
//!
//! 复刻样本 `src/battle/udpxin.rs`。这一层负责把一条 UDP 数据报变成
//! **一组合法的 Bunch**，是整条解析链的第一道闸门（样本里的
//! `unresolved_bunch_header_variant` / `invalid_packet_framing` / `server_decode_gate`
//! 全部来自这里）。
//!
//! UE5 的数据报结构（`UNetConnection::ReceivedPacket` →
//! `UChannel::ReceivedNextBunch`）：
//!
//! ```text
//! [可选] Compression header: uint32 UncompressedSize (LE)  —— 若与压缩配置匹配
//! [可选] Encryption: 整包异或 + AES-256-ECB（见 transport_crypto.rs）
//! Packet header:
//!     bit   bHasServerFrameTime            (UE5.1+，可选)
//!     bits  PacketId    (SerializeInt / 固定位宽)      <- 序列号
//!     bits  AckPacketId (SerializeInt / 固定位宽)      <- 确认号
//!     bit   bHasPacketInfoDefaults / bHasChecksum? (视 profile)
//! Bunch 序列（读到数据报末尾或 bClose/错误为止）:
//!     bit bControl
//!     if bControl:
//!         bit bOpen, bClose, bDormant(仅 bOpen), bIsReplicationPaused, bReliable
//!         bits ChannelIndex (SerializeInt(MaxChannels))
//!         if bReliable: bits ReliableSequence (以 ControlChannel 为基准)
//!         if bOpen:
//!             FString Name, FString ChName
//!             bit bHasPackageMapExports, bit bHasMustBeMappedGUIDs, bit bPartial
//!             if bPartial: bits PartialGuid / bits PartialInitialNum / bits PartialBunchCount
//!         if bClose: bits CloseReason (SerializeInt)
//!         bits BunchDataBits (通常 16 位)
//!     else:
//!         bit bPartialInitial / bPartialCustom ...
//! ```
//!
//! 因为不同小版本的位宽/顺序有漂移，这里把**所有随版本变化的量抽成
//! `ProtocolProfile`**，并用 `BunchHeaderVariant` 显式枚举已知变体。这与样本行为一致：
//! 样本本身也会在某些包上落到 `unresolved_bunch_header_variant`，然后退化为
//! "只统计不解码"。

use serde::{Deserialize, Serialize};

use super::codec::bitstream::{BitReader, ceil_log2};

/// 通道索引上限（UE 默认 `MAX_CHANNELS = 32768`，三角洲实测远小于此）。
pub const MAX_CHANNELS: u32 = 4096;

/// 一版协议的位宽/开关集合。**换游戏小版本时只需要改这里或改 JSON**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolProfile {
    pub name: String,
    /// 包序号位宽；0 表示使用 `SerializeInt(packet_id_max)`。
    pub packet_id_bits: u32,
    pub packet_id_max: u32,
    /// 确认号位宽；0 表示使用 `SerializeInt`。
    pub ack_bits: u32,
    pub ack_max: u32,
    /// UE5.1+ 包头首个 bit。
    pub has_server_frame_time: bool,
    /// 包头是否带 1 位 "bHasPacketInfoDefaults"。
    pub has_packet_info_bit: bool,
    /// 通道索引位宽（0 表示 `SerializeInt(max_channels)`）。
    pub channel_index_bits: u32,
    pub max_channels: u32,
    /// bunch 数据长度字段位宽。
    pub bunch_data_bits_width: u32,
    /// 是否支持 partial bunch（跨包重组）。
    pub supports_partial_bunch: bool,
    /// 压缩包是否在数据报最前面带 `uint32` 未压缩长度。
    pub compression_prefix_u32: bool,
    /// 该版本 bunch 头变体。
    pub bunch_variant: BunchHeaderVariant,
}

/// 已知的 bunch 头变体。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BunchHeaderVariant {
    /// UE4.20 – UE5.0 经典布局：bControl 之后直接是 bOpen/bClose/bReliable/ChIndex。
    UeClassic,
    /// UE5.1+：在 bControl 后多了 `bIsReplicationPaused`，partial 信息也前移。
    Ue5ReplicationPaused,
    /// 未知变体：只允许"统计"，不允许进入属性解析。
    Unresolved,
}

impl ProtocolProfile {
    /// 三角洲行动手游 r39（样本）实测配置。位宽偏保守，靠 `server_decode_gate`
    /// 的位流校验兜底；升级游戏后优先调 `packet_id_bits` 与 `max_channels`。
    pub fn dfm_r39() -> Self {
        Self {
            name: "dfm-r39".to_string(),
            packet_id_bits: 0,
            packet_id_max: 1023,
            ack_bits: 0,
            ack_max: 1023,
            has_server_frame_time: true,
            has_packet_info_bit: false,
            channel_index_bits: 0,
            max_channels: MAX_CHANNELS,
            bunch_data_bits_width: 16,
            supports_partial_bunch: true,
            compression_prefix_u32: true,
            bunch_variant: BunchHeaderVariant::Ue5ReplicationPaused,
        }
    }

    /// UE5.0 兼容档（用于跨版本抓包比对）。
    pub fn ue5_classic() -> Self {
        Self {
            name: "ue5-classic".to_string(),
            has_server_frame_time: false,
            bunch_variant: BunchHeaderVariant::UeClassic,
            ..Self::dfm_r39()
        }
    }

    pub fn load_or_default(json: Option<&str>) -> Self {
        json.and_then(|s| serde_json::from_str::<Self>(s).ok()).unwrap_or_else(Self::dfm_r39)
    }

    #[inline]
    fn read_channel_index(&self, r: &mut BitReader<'_>) -> u32 {
        if self.channel_index_bits == 0 {
            r.read_serialized_int(self.max_channels)
        } else {
            r.read_bits(self.channel_index_bits)
        }
    }

    #[inline]
    fn read_packet_id(&self, r: &mut BitReader<'_>) -> u32 {
        if self.packet_id_bits == 0 {
            r.read_serialized_int(self.packet_id_max)
        } else {
            r.read_bits(self.packet_id_bits)
        }
    }

    #[inline]
    fn read_ack_id(&self, r: &mut BitReader<'_>) -> u32 {
        if self.ack_bits == 0 {
            r.read_serialized_int(self.ack_max)
        } else {
            r.read_bits(self.ack_bits)
        }
    }
}

impl Default for ProtocolProfile {
    fn default() -> Self {
        Self::dfm_r39()
    }
}

/// 包头。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PacketHeader {
    pub has_server_frame_time: bool,
    pub server_frame_time: f32,
    pub packet_id: u32,
    pub ack_id: u32,
    pub bits_consumed: usize,
}

/// 一个 Bunch 的头部信息 + 净荷位区间。
#[derive(Debug, Clone, PartialEq)]
pub struct Bunch {
    pub b_control: bool,
    pub b_open: bool,
    pub b_close: bool,
    pub b_dormant: bool,
    pub b_reliable: bool,
    pub b_is_replication_paused: bool,
    pub b_has_package_map_exports: bool,
    pub b_has_must_be_mapped_guids: bool,
    pub b_partial: bool,
    pub b_partial_initial: bool,
    pub partial_initial_num: u32,
    pub partial_bunch_count: u32,
    pub channel_index: u32,
    pub reliable_sequence: u32,
    pub close_reason: u32,
    pub name: Option<String>,
    pub ch_name: Option<String>,
    /// 净荷位数（样本里的 `Bunch payload exceeds packet at`）。
    pub payload_bits: u32,
    /// 该 bunch 数据的**位**起止（相对整包字节缓冲）。
    pub payload_bit_start: usize,
    pub payload_bit_end: usize,
    /// 净荷原始位。
    pub payload: Vec<u8>,
    pub variant: BunchHeaderVariant,
}

impl Bunch {
    /// 净荷是否为「完整且未跨包」，只有完整的 bunch 才允许做属性解析。
    pub fn is_complete(&self) -> bool {
        !self.b_partial
    }
}

/// 分帧错误（对应样本的一串错误标识）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameError {
    /// 包头不成立。
    InvalidPacketFraming,
    /// 位流里读出的 bunch 头不符合任何已知变体。
    UnresolvedBunchHeaderVariant,
    /// 声明的净荷长度超过数据报可用位。
    BunchPayloadExceedsPacket { at_bit: usize, wanted: u32, available: u32 },
    /// 未对齐就结束了。
    TruncatedBunch,
    /// 位流溢出。
    BitOverflow,
}

/// 一条数据报的分帧结果。
#[derive(Debug, Clone)]
pub struct FramedDatagram {
    pub header: PacketHeader,
    pub bunches: Vec<Bunch>,
    pub errors: Vec<FrameError>,
    /// 未识别的尾部（诊断采集会原样落盘）。
    pub trailing: Vec<u8>,
    /// 关键：通过 `server_decode_gate` 才为 true。
    pub gate_passed: bool,
}

/// 分帧器：持有 profile 与 partial bunch 的重组表。
pub struct PacketFramer {
    profile: ProtocolProfile,
    /// key = (channel_index, partial_initial_num) -> 已收位流
    partials: std::collections::HashMap<(u32, u32), PartialBunch>,
}

#[derive(Debug, Clone)]
struct PartialBunch {
    bit_len: usize,
    data: Vec<u8>,
    expected: u32,
    created_ms: u64,
    chunks: u32,
}

impl PacketFramer {
    pub fn new(profile: ProtocolProfile) -> Self {
        Self { profile, partials: std::collections::HashMap::new() }
    }

    pub fn profile(&self) -> &ProtocolProfile {
        &self.profile
    }

    pub fn set_profile(&mut self, p: ProtocolProfile) {
        self.profile = p;
    }

    /// 结构校验（`server_decode_gate`）：包头可读 + 至少一个 bunch 头成立 + 无致命错误。
    /// 传输层用这个闭包判断"是否解密成功"。
    pub fn validate_datagram(&self, bytes: &[u8]) -> bool {
        if bytes.len() < 3 {
            return false;
        }
        let mut r = BitReader::new(bytes);
        let hdr = match self.read_packet_header(&mut r) {
            Some(h) => h,
            None => return false,
        };
        // 序号/确认号必须落在合法区间；全 0 或全 1 视为垃圾。
        if hdr.packet_id == 0 && hdr.ack_id == 0 {
            return false;
        }
        let mut ok = false;
        for _ in 0..64 {
            if r.bits_left() < 8 {
                break;
            }
            match self.read_bunch_header(&mut r) {
                Ok(Some(b)) => {
                    ok = true;
                    if b.b_control && b.b_close {
                        break;
                    }
                    if b.payload_bits as usize > r.bits_left() {
                        break;
                    }
                    let _ = r.read_bits(b.payload_bits.min(32));
                    // 跳到净荷末尾
                    let remain = b.payload_bits.saturating_sub(32) as usize;
                    if remain > 0 {
                        let _ = r.read_bits(remain.min(32) as u32);
                        let rest = remain.saturating_sub(32);
                        if rest > 0 {
                            for _ in 0..rest {
                                r.read_bit();
                            }
                        }
                    }
                    if r.overflowed() {
                        break;
                    }
                }
                _ => break,
            }
        }
        ok
    }

    /// 完整分帧。
    pub fn frame(&mut self, bytes: &[u8], now_ms: u64) -> FramedDatagram {
        let mut errors = Vec::new();
        let mut r = BitReader::new(bytes);
        let header = match self.read_packet_header(&mut r) {
            Some(h) => h,
            None => {
                return FramedDatagram {
                    header: PacketHeader::default(),
                    bunches: Vec::new(),
                    errors: vec![FrameError::InvalidPacketFraming],
                    trailing: bytes.to_vec(),
                    gate_passed: false,
                };
            }
        };

        let mut bunches = Vec::new();
        let total_bits = bytes.len() * 8;

        while r.bits_left() >= 8 && bunches.len() < 256 {
            let start = r.bit_pos();
            let b = match self.read_bunch_header(&mut r) {
                Ok(Some(b)) => b,
                Ok(None) => break,
                Err(e) => {
                    errors.push(e);
                    break;
                }
            };

            let want = b.payload_bits as usize;
            let available = total_bits.saturating_sub(r.bit_pos());
            if want > available {
                errors.push(FrameError::BunchPayloadExceedsPacket {
                    at_bit: r.bit_pos(),
                    wanted: b.payload_bits,
                    available: available as u32,
                });
                // 样本行为：`Bunch payload exceeds packet at` 之后丢弃该 bunch，保留已解析部分。
                break;
            }

            let payload = self.extract_bits(bytes, r.bit_pos(), want);
            for _ in 0..want {
                r.read_bit();
            }

            let mut bunch = b;
            bunch.payload_bit_start = start;
            bunch.payload_bit_end = r.bit_pos();
            bunch.payload = payload;

            if bunch.b_partial && self.profile.supports_partial_bunch {
                self.absorb_partial(&mut bunch, now_ms, &mut errors);
            }
            if bunch.b_dormant || (bunch.b_control && bunch.b_close) {
                bunches.push(bunch);
                break;
            }
            bunches.push(bunch);
            if r.overflowed() {
                errors.push(FrameError::BitOverflow);
                break;
            }
        }

        // 尾部未识别字节（位对齐后剩余）。
        r.align_to_byte();
        let trailing = if r.byte_pos() < bytes.len() { bytes[r.byte_pos()..].to_vec() } else { Vec::new() };

        let fatal = errors.iter().any(|e| {
            matches!(
                e,
                FrameError::InvalidPacketFraming
                    | FrameError::BunchPayloadExceedsPacket { .. }
                    | FrameError::BitOverflow
            )
        });
        let gate_passed = !bunches.is_empty() && !fatal;

        FramedDatagram { header, bunches, errors, trailing, gate_passed }
    }

    fn extract_bits(&self, bytes: &[u8], start_bit: usize, count: usize) -> Vec<u8> {
        if count == 0 {
            return Vec::new();
        }
        let mut r = BitReader::with_bit_offset(bytes, start_bit);
        let mut out = Vec::with_capacity(count.div_ceil(8));
        let mut acc = 0u32;
        let mut acc_bits = 0u32;
        for _ in 0..count {
            acc |= (r.read_bit() as u32) << acc_bits;
            acc_bits += 1;
            if acc_bits == 8 {
                out.push(acc as u8);
                acc = 0;
                acc_bits = 0;
            }
        }
        if acc_bits > 0 {
            out.push(acc as u8);
        }
        out
    }

    fn read_packet_header(&self, r: &mut BitReader<'_>) -> Option<PacketHeader> {
        let start = r.bit_pos();
        let mut h = PacketHeader::default();
        if self.profile.has_server_frame_time {
            h.has_server_frame_time = r.read_bit();
            if h.has_server_frame_time {
                h.server_frame_time = r.read_compressed_float(24);
            }
        }
        h.packet_id = self.profile.read_packet_id(r);
        h.ack_id = self.profile.read_ack_id(r);
        if self.profile.has_packet_info_bit {
            let _ = r.read_bit();
        }
        if r.overflowed() {
            return None;
        }
        h.bits_consumed = r.bit_pos() - start;
        Some(h)
    }

    /// 读一个 bunch 头。`Ok(None)` = 干净的流末尾。
    fn read_bunch_header(&self, r: &mut BitReader<'_>) -> Result<Option<Bunch>, FrameError> {
        if r.bits_left() < 3 {
            return Ok(None);
        }
        let start_bits = r.bits_left();
        let b_control = r.read_bit();
        let mut b = Bunch {
            b_control,
            b_open: false,
            b_close: false,
            b_dormant: false,
            b_reliable: false,
            b_is_replication_paused: false,
            b_has_package_map_exports: false,
            b_has_must_be_mapped_guids: false,
            b_partial: false,
            b_partial_initial: false,
            partial_initial_num: 0,
            partial_bunch_count: 0,
            channel_index: 0,
            reliable_sequence: 0,
            close_reason: 0,
            name: None,
            ch_name: None,
            payload_bits: 0,
            payload_bit_start: 0,
            payload_bit_end: 0,
            payload: Vec::new(),
            variant: self.profile.bunch_variant,
        };

        if !b_control {
            // 非控制 bunch：UE 仍要求 ChannelIndex + 长度，但不同版本差异最大，
            // 直接按 unresolved 处理（样本同路径）。
            if self.profile.bunch_variant == BunchHeaderVariant::UeClassic {
                b.channel_index = self.profile.read_channel_index(r);
                b.payload_bits = r.read_bits(self.profile.bunch_data_bits_width);
                return Ok(Some(b));
            }
            return Err(FrameError::UnresolvedBunchHeaderVariant);
        }

        b.b_open = r.read_bit();
        b.b_close = r.read_bit();
        if b.b_open {
            b.b_dormant = r.read_bit();
        }
        if self.profile.bunch_variant == BunchHeaderVariant::Ue5ReplicationPaused {
            b.b_is_replication_paused = r.read_bit();
        }
        b.b_reliable = r.read_bit();
        b.channel_index = self.profile.read_channel_index(r);

        if b.b_reliable {
            if b.channel_index == 0 {
                // ControlChannel 的可靠序号是 32 位
                b.reliable_sequence = r.read_bits(32);
            } else {
                b.reliable_sequence = r.read_bits(32);
            }
        }

        if b.b_open {
            b.name = r.read_fstring();
            b.ch_name = r.read_fstring();
            b.b_has_package_map_exports = r.read_bit();
            b.b_has_must_be_mapped_guids = r.read_bit();
            b.b_partial = r.read_bit();
            if b.b_partial {
                b.b_partial_initial = r.read_bit();
                b.partial_initial_num = r.read_bits(16);
                b.partial_bunch_count = r.read_bits(16);
            }
        }
        if b.b_close {
            b.close_reason = r.read_serialized_int(255);
        }
        b.payload_bits = r.read_bits(self.profile.bunch_data_bits_width);

        if r.overflowed() {
            return Err(FrameError::UnresolvedBunchHeaderVariant);
        }
        // 合理性检查：头部不该比可用位还长。
        if start_bits < (start_bits - r.bits_left()) {
            return Err(FrameError::UnresolvedBunchHeaderVariant);
        }
        Ok(Some(b))
    }

    /// partial bunch 重组（跨包）。
    fn absorb_partial(&mut self, b: &mut Bunch, now_ms: u64, errors: &mut Vec<FrameError>) {
        let key = (b.channel_index, b.partial_initial_num);
        if b.b_partial_initial {
            self.partials.insert(
                key,
                PartialBunch {
                    bit_len: b.payload_bits as usize,
                    data: b.payload.clone(),
                    expected: b.partial_bunch_count.max(1),
                    created_ms: now_ms,
                    chunks: 1,
                },
            );
            return;
        }
        let Some(entry) = self.partials.get_mut(&key) else {
            errors.push(FrameError::TruncatedBunch);
            return;
        };
        entry.data.extend_from_slice(&b.payload);
        entry.bit_len += b.payload_bits as usize;
        entry.chunks += 1;
        if entry.chunks >= entry.expected {
            let Some(done) = self.partials.remove(&key) else { return };
            b.payload = done.data;
            b.payload_bits = done.bit_len as u32;
            b.b_partial = false;
        }
    }

    /// 清理超时的 partial 半成品（默认 5 秒）。
    pub fn reap_partials(&mut self, now_ms: u64, max_age_ms: u64) -> usize {
        let before = self.partials.len();
        self.partials.retain(|_, v| now_ms.saturating_sub(v.created_ms) < max_age_ms);
        before - self.partials.len()
    }

    pub fn partial_count(&self) -> usize {
        self.partials.len()
    }
}

/// 便捷函数：`ceil(log2(max_channels))`，profile 调参时常用。
pub fn channel_index_bits(max_channels: u32) -> u32 {
    ceil_log2(max_channels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::codec::bitstream::BitWriter;

    fn write_packet_header(w: &mut BitWriter, p: &ProtocolProfile, pid: u32, ack: u32) {
        if p.has_server_frame_time {
            w.write_bit(false);
        }
        if p.packet_id_bits == 0 {
            let bits = ceil_log2(p.packet_id_max + 1);
            w.write_bits(pid, bits);
        } else {
            w.write_bits(pid, p.packet_id_bits);
        }
        if p.ack_bits == 0 {
            let bits = ceil_log2(p.ack_max + 1);
            w.write_bits(ack, bits);
        } else {
            w.write_bits(ack, p.ack_bits);
        }
    }

    fn write_control_bunch(
        w: &mut BitWriter,
        p: &ProtocolProfile,
        channel: u32,
        close: bool,
        payload_bits: u32,
        payload: &[u8],
    ) {
        w.write_bit(true); // bControl
        w.write_bit(false); // bOpen
        w.write_bit(close);
        if p.bunch_variant == BunchHeaderVariant::Ue5ReplicationPaused {
            w.write_bit(false); // bIsReplicationPaused
        }
        w.write_bit(false); // bReliable
        let bits = ceil_log2(p.max_channels + 1);
        w.write_bits(channel, bits);
        if close {
            w.write_bits(0, 8); // close reason
        }
        w.write_bits(payload_bits, p.bunch_data_bits_width);
        for i in 0..payload_bits {
            let byte = payload[(i / 8) as usize];
            w.write_bit((byte >> (i % 8)) & 1 != 0);
        }
    }

    #[test]
    fn frames_a_single_control_bunch() {
        let p = ProtocolProfile::dfm_r39();
        let mut w = BitWriter::new();
        write_packet_header(&mut w, &p, 42, 41);
        write_control_bunch(&mut w, &p, 3, true, 16, &[0xAA, 0x55]);
        let bytes = w.into_bytes();

        let mut f = PacketFramer::new(p.clone());
        let out = f.frame(&bytes, 1);
        assert_eq!(out.header.packet_id, 42);
        assert_eq!(out.header.ack_id, 41);
        assert_eq!(out.bunches.len(), 1);
        let b = &out.bunches[0];
        assert_eq!(b.channel_index, 3);
        assert!(b.b_control && b.b_close);
        assert_eq!(b.payload_bits, 16);
        assert_eq!(b.payload, vec![0xAA, 0x55]);
        assert!(out.gate_passed);
    }

    #[test]
    fn gate_rejects_garbage() {
        let p = ProtocolProfile::dfm_r39();
        let mut f = PacketFramer::new(p);
        assert!(!f.validate_datagram(&[0, 0]));
        assert!(!f.validate_datagram(&[0xFF; 4]));
    }

    #[test]
    fn gate_accepts_a_real_ish_packet() {
        let p = ProtocolProfile::dfm_r39();
        let mut w = BitWriter::new();
        write_packet_header(&mut w, &p, 1000, 999);
        write_control_bunch(&mut w, &p, 12, true, 32, &[1, 2, 3, 4]);
        let bytes = w.into_bytes();
        let f = PacketFramer::new(p);
        assert!(f.validate_datagram(&bytes));
    }

    #[test]
    fn payload_exceeding_packet_is_reported() {
        let p = ProtocolProfile::dfm_r39();
        let mut w = BitWriter::new();
        write_packet_header(&mut w, &p, 7, 6);
        w.write_bit(true);
        w.write_bit(false);
        w.write_bit(false);
        w.write_bit(false);
        w.write_bit(false);
        w.write_bits(3, ceil_log2(p.max_channels + 1));
        w.write_bits(4096, p.bunch_data_bits_width); // 声明 4096 位但只有几字节
        let bytes = w.into_bytes();
        let mut f = PacketFramer::new(p);
        let out = f.frame(&bytes, 0);
        assert!(out
            .errors
            .iter()
            .any(|e| matches!(e, FrameError::BunchPayloadExceedsPacket { .. })));
        assert!(!out.gate_passed);
    }

    #[test]
    fn half_byte_payload_extraction_is_bit_exact() {
        let p = ProtocolProfile::dfm_r39();
        let mut w = BitWriter::new();
        write_packet_header(&mut w, &p, 5, 4);
        write_control_bunch(&mut w, &p, 2, true, 12, &[0b1010_1010, 0b0000_1111]);
        let bytes = w.into_bytes();
        let mut f = PacketFramer::new(p);
        let out = f.frame(&bytes, 0);
        let b = &out.bunches[0];
        assert_eq!(b.payload_bits, 12);
        assert_eq!(b.payload, vec![0xAA, 0x0F]);
    }

    #[test]
    fn partials_are_reaped_when_stale() {
        let mut f = PacketFramer::new(ProtocolProfile::dfm_r39());
        let mut b = Bunch {
            b_control: true,
            b_open: true,
            b_partial: true,
            b_partial_initial: true,
            partial_initial_num: 9,
            partial_bunch_count: 4,
            channel_index: 3,
            payload_bits: 8,
            payload: vec![0],
            b_close: false,
            b_dormant: false,
            b_reliable: false,
            b_is_replication_paused: false,
            b_has_package_map_exports: false,
            b_has_must_be_mapped_guids: false,
            reliable_sequence: 0,
            close_reason: 0,
            name: None,
            ch_name: None,
            payload_bit_start: 0,
            payload_bit_end: 0,
            variant: BunchHeaderVariant::Ue5ReplicationPaused,
        };
        let mut errs = Vec::new();
        f.absorb_partial(&mut b, 1_000, &mut errs);
        assert_eq!(f.partial_count(), 1);
        assert_eq!(f.reap_partials(10_000, 5_000), 1);
        assert_eq!(f.partial_count(), 0);
    }

    #[test]
    fn channel_index_bits_matches_ua_ceillogtwo() {
        assert_eq!(channel_index_bits(3), 2);
        assert_eq!(channel_index_bits(1023), 10);
        assert_eq!(channel_index_bits(4096), 12);
    }
}
