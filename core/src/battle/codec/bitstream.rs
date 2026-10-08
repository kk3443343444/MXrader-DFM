//! UE 位流读写：`battle_proxy::battle::codec::bitstream`。
//!
//! 复刻样本中的 `src/battle/codec/bitstream.rs`。游戏侧（UE5）用 `FBitReader`/
//! `FBitWriter` 传输复制数据：位从**字节低位**开始填充，所有整数/浮点/向量都有
//! 自己的紧凑表示。这里把它们逐个实现，并保留 UE 的"溢出即标记错误、不 panic"语义。
//!
//! 参考：`FBitReader::SerializeBits` / `SerializeInt` / `SerializeCompressed` /
//! `UScriptStruct` 的 RepLayout 属性编码（`FRepLayout::SendProperties_r`）。

use std::f32;

/// 读取方向：正序（网络字节流）与位偏移。
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// 已消费的位数。
    bit_pos: usize,
    /// 出错后所有读取都返回 0，由调用方决定是否丢弃整个包。
    overflowed: bool,
    /// 记录首个错误位置，便于诊断（样本里的 `expected char at offset`）。
    first_error_bit: Option<usize>,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0, overflowed: false, first_error_bit: None }
    }

    /// 从任意位偏移开始（UE 的 partial bunch 会带初相位）。
    pub fn with_bit_offset(data: &'a [u8], bit_offset: usize) -> Self {
        let mut r = Self::new(data);
        r.bit_pos = bit_offset;
        if bit_offset > data.len() * 8 {
            r.mark_overflow();
        }
        r
    }

    #[inline]
    pub fn bit_pos(&self) -> usize {
        self.bit_pos
    }
    #[inline]
    pub fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.bit_pos)
    }
    #[inline]
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }
    #[inline]
    pub fn first_error_bit(&self) -> Option<usize> {
        self.first_error_bit
    }
    #[inline]
    pub fn is_byte_aligned(&self) -> bool {
        self.bit_pos % 8 == 0
    }
    #[inline]
    pub fn byte_pos(&self) -> usize {
        self.bit_pos / 8
    }

    fn mark_overflow(&mut self) {
        if !self.overflowed {
            self.overflowed = true;
            self.first_error_bit = Some(self.bit_pos);
        }
    }

    /// 读 1 位（UE 的 `SerializeBits(&b, 1)`）。
    #[inline]
    pub fn read_bit(&mut self) -> bool {
        if self.bit_pos >= self.data.len() * 8 {
            self.mark_overflow();
            return false;
        }
        let byte = self.data[self.bit_pos >> 3];
        let bit = (byte >> (self.bit_pos & 7)) & 1;
        self.bit_pos += 1;
        bit != 0
    }

    /// 读 n 位（n <= 32），LSB-first，与 `FBitReader::SerializeBits` 一致。
    pub fn read_bits(&mut self, count: u32) -> u32 {
        debug_assert!(count <= 32);
        if count == 0 {
            return 0;
        }
        if self.bit_pos + count as usize > self.data.len() * 8 {
            self.mark_overflow();
            return 0;
        }
        let mut out: u32 = 0;
        let mut got = 0u32;
        while got < count {
            let byte_idx = self.bit_pos >> 3;
            let bit_off = (self.bit_pos & 7) as u32;
            let take = std::cmp::min(8 - bit_off, count - got);
            let byte = self.data[byte_idx] as u32;
            let mask = if take >= 32 { u32::MAX } else { (1u32 << take) - 1 };
            let chunk = (byte >> bit_off) & mask;
            out |= chunk << got;
            got += take;
            self.bit_pos += take as usize;
        }
        out
    }

    /// 读 n 位到 u64（UE5 的序列号用 16/32 位，但 packed 值可能更宽）。
    pub fn read_bits64(&mut self, count: u32) -> u64 {
        if count <= 32 {
            return self.read_bits(count) as u64;
        }
        let lo = self.read_bits(32) as u64;
        let hi = self.read_bits(count - 32) as u64;
        lo | (hi << 32)
    }

    /// 读 n 个字节（要求位对齐；不对齐时先丢弃到下一字节边界，符合 UE 行为）。
    pub fn read_bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        if !self.is_byte_aligned() {
            self.align_to_byte();
        }
        let start = self.byte_pos();
        if start + n > self.data.len() {
            self.mark_overflow();
            return None;
        }
        self.bit_pos += n * 8;
        Some(&self.data[start..start + n])
    }

    #[inline]
    pub fn align_to_byte(&mut self) {
        self.bit_pos = (self.bit_pos + 7) & !7;
    }

    /// `FBitReader::SerializeInt(Value, Max)`：读 `ceil(log2(Max+1))` 位。
    pub fn read_serialized_int(&mut self, max: u32) -> u32 {
        if max == 0 {
            return 0;
        }
        let bits = ceil_log2(max + 1);
        let v = self.read_bits(bits);
        if v > max {
            // UE 会 SetOverflowed —— 我们保持宽容：截断并打标，让上层走"可疑包"路径。
            self.mark_overflow();
            return 0;
        }
        v
    }

    /// `FBitReader::SerializeIntPacked`：1 字节 7 位净荷 + 续读位（小端序）。
    pub fn read_packed_int(&mut self) -> u32 {
        let mut value: u32 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.read_bits(8);
            value |= (byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 28 {
                self.mark_overflow();
                break;
            }
        }
        value
    }

    /// `FArchive::SerializeIntPacked64`
    pub fn read_packed_int64(&mut self) -> u64 {
        let mut value: u64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.read_bits(8) as u64;
            value |= (byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                self.mark_overflow();
                break;
            }
        }
        value
    }

    /// `FBitReader::SerializeCompressed`（UE 的 1..N 字节压缩整数）：
    /// 4 位“有效位数-1”，随后是有效字节，其余位为符号扩展位。
    pub fn read_compressed_int(&mut self, max_bits: u32) -> i32 {
        let wanted = self.read_bits(4);
        let mut value: u32 = 0;
        let mut i = 0;
        while i <= wanted {
            value |= self.read_bits(8) << (i * 8);
            i += 1;
        }
        let sign_bit = 1u32 << (wanted * 8 + 7);
        let ext = if value & sign_bit != 0 { u32::MAX } else { 0 };
        let signed = (value | ext) as i32;
        // 兼容 UE 的负值编码：实际值 = 位宽内取反的前缀
        let _ = max_bits;
        signed
    }

    /// `FBitReader::SerializeCompressedF` / `WriteCompressed`：先读位宽再读整数。
    pub fn read_compressed_float(&mut self, max_bits: u32) -> f32 {
        if max_bits == 0 {
            return 0.0;
        }
        let bits = self.read_bits(6);
        if bits == 0 {
            return 0.0;
        }
        if bits > max_bits {
            self.mark_overflow();
            return 0.0;
        }
        let raw = self.read_bits(bits);
        let sign = if self.read_bit() { -1.0f32 } else { 1.0f32 };
        sign * raw as f32
    }

    /// UE 的 `SerializeFloat`（值化整：1 位符号 + 1 位「需要缩放」+ 可变位尾数）。
    pub fn read_serialized_float(&mut self, max: f32) -> f32 {
        if max == 0.0 {
            return 0.0;
        }
        let neg = self.read_bit();
        let scale = self.read_bit();
        let abs_bits = ceil_log2((max * 1000.0) as u32 + 1);
        let mut value = self.read_bits(abs_bits);
        if scale {
            // 定点尾：后续 4 位为小数
            let frac_bits = self.read_bits(4);
            let frac = self.read_bits(frac_bits) as f32 / (1u32 << frac_bits) as f32;
            value = ((value as f32 + frac) * 1000.0) as u32;
        }
        let out = value as f32 / 1000.0;
        if neg {
            -out
        } else {
            out
        }
    }

    /// `FRepMovement` 里的量化向量：各分量按 1/精度 定点，UE 默认 1cm/1°。
    pub fn read_quantized_vector(&mut self, scale: f32, bits: u32) -> [f32; 3] {
        let x = self.read_bits(bits) as i32;
        let y = self.read_bits(bits) as i32;
        let z = self.read_bits(bits) as i32;
        // 量化值为有符号：UE 写入前加了 bias
        let bias = 1i32 << (bits - 1);
        [
            (x - bias) as f32 * scale,
            (y - bias) as f32 * scale,
            (z - bias) as f32 * scale,
        ]
    }

    /// UE 的 `SerializeFixedVector`（每分量固定位宽，带符号扩展到 32 位）。
    pub fn read_fixed_vector(&mut self, max_bits_per_component: u32) -> [f32; 3] {
        let mut out = [0f32; 3];
        for c in out.iter_mut() {
            let raw = self.read_bits(max_bits_per_component) as i32;
            let shift = 32 - max_bits_per_component as i32;
            *c = (raw << shift >> shift) as f32;
        }
        out
    }

    /// `Rotator`：三轴按 16 位量化（UE 的 yaw/pitch/roll 均为 65536 分之一圈）。
    pub fn read_rotator_16(&mut self) -> [f32; 3] {
        let p = self.read_bits(16) as u16;
        let y = self.read_bits(16) as u16;
        let r = self.read_bits(16) as u16;
        [rotator_to_deg(p), rotator_to_deg(y), rotator_to_deg(r)]
    }

    /// `FString`（UE 序列化：int32 长度 + ANSI/UTF-16 净荷，负数代表 UTF-16）。
    pub fn read_fstring(&mut self) -> Option<String> {
        let len = self.read_bits(32) as i32;
        if len == 0 {
            return Some(String::new());
        }
        if len > 0 {
            let bytes = self.read_bytes(len as usize)?;
            Some(String::from_utf8_lossy(&bytes[..bytes.len().saturating_sub(1)]).to_string())
        } else {
            let n = (-len) as usize;
            let mut s = String::with_capacity(n);
            for _ in 0..n {
                let u = self.read_bits(16) as u16;
                if u == 0 {
                    break;
                }
                s.push(char::from_u32(u as u32).unwrap_or('\u{fffd}'));
            }
            Some(s)
        }
    }

    /// 原始位切片（UE 的 `SerializeBits(Buf, Count)`）。
    pub fn read_raw_bits(&mut self, count: usize) -> Vec<bool> {
        (0..count).map(|_| self.read_bit()).collect()
    }
}

/// `Rotator`（0..65535）→ 角度。
#[inline]
pub fn rotator_to_deg(v: u16) -> f32 {
    v as f32 * 360.0 / 65536.0
}

/// 角度 → `Rotator`。
#[inline]
pub fn deg_to_rotator(deg: f32) -> u16 {
    let n = deg.rem_euclid(360.0);
    ((n / 360.0) * 65536.0) as u16
}

/// UE `appCeilLogTwo`。
#[inline]
pub fn ceil_log2(mut value: u32) -> u32 {
    let mut bits = 0u32;
    value = value.saturating_sub(1);
    while value > 0 {
        value >>= 1;
        bits += 1;
    }
    bits
}

/// 位写入器（自检与协议重放需要）。
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    data: Vec<u8>,
    bit_pos: usize,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write_bit(&mut self, v: bool) {
        if self.bit_pos >> 3 >= self.data.len() {
            self.data.push(0);
        }
        if v {
            self.data[self.bit_pos >> 3] |= 1 << (self.bit_pos & 7);
        }
        self.bit_pos += 1;
    }

    pub fn write_bits(&mut self, mut value: u32, count: u32) {
        for _ in 0..count {
            self.write_bit(value & 1 != 0);
            value >>= 1;
        }
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.write_bits(*b as u32, 8);
        }
    }

    pub fn align_to_byte(&mut self) {
        while self.bit_pos % 8 != 0 {
            self.write_bit(false);
        }
    }

    pub fn bit_pos(&self) -> usize {
        self.bit_pos
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        self.align_to_byte();
        self.data
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_order_is_lsb_first() {
        let data = [0b1011_0001u8, 0b0000_0011];
        let mut r = BitReader::new(&data);
        assert!(r.read_bit());
        assert!(!r.read_bit());
        assert!(!r.read_bit());
        assert!(r.read_bit());
        assert!(!r.read_bit());
        assert_eq!(r.read_bits(7), 0b0000_0011);
    }

    #[test]
    fn read_bits_across_byte_boundary() {
        // 0xAB = 1010_1011, 0xCD = 1100_1101 -> 从第 4 位起读 8 位 = 0xDA
        let mut r = BitReader::with_bit_offset(&[0xAB, 0xCD], 4);
        assert_eq!(r.read_bits(8), 0xDA);
    }

    #[test]
    fn serialized_int_uses_ceil_log2_bits() {
        // Max=3 -> 2 位；写入 2 -> 读回 2
        let mut w = BitWriter::new();
        w.write_bits(2, 2);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_serialized_int(3), 2);
    }

    #[test]
    fn packed_int_roundtrip() {
        for v in [0u32, 1, 127, 128, 300, 65_535, 1_000_000] {
            let mut w = BitWriter::new();
            let mut x = v;
            loop {
                let mut b = (x & 0x7f) as u32;
                x >>= 7;
                if x != 0 {
                    b |= 0x80;
                }
                w.write_bits(b, 8);
                if x == 0 {
                    break;
                }
            }
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.read_packed_int(), v, "packed int {v}");
        }
    }

    #[test]
    fn packed_int64_high_values() {
        let v: u64 = 0x0F_1234_5678_9ABC;
        let mut w = BitWriter::new();
        let mut x = v;
        loop {
            let mut b = (x & 0x7f) as u32;
            x >>= 7;
            if x != 0 {
                b |= 0x80;
            }
            w.write_bits(b, 8);
            if x == 0 {
                break;
            }
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_packed_int64(), v);
    }

    #[test]
    fn rotator_roundtrip_matches_ue_quantisation() {
        assert_eq!(rotator_to_deg(0), 0.0);
        assert!((rotator_to_deg(16384) - 90.0).abs() < 0.01);
        assert!((rotator_to_deg(32768) - 180.0).abs() < 0.01);
        assert!((rotator_to_deg(49152) - 270.0).abs() < 0.01);
        assert!((rotator_to_deg(deg_to_rotator(137.5)) - 137.5).abs() < 0.01);
    }

    #[test]
    fn overflow_is_flagged_not_panicking() {
        let mut r = BitReader::new(&[0x00]);
        let _ = r.read_bits(16);
        assert!(r.overflowed());
        assert_eq!(r.first_error_bit(), Some(8));
        // 后续读取仍然安全
        assert_eq!(r.read_bits(8), 0);
    }

    #[test]
    fn compressed_float_decodes_zero_without_bits() {
        let mut w = BitWriter::new();
        w.write_bits(0, 6);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_compressed_float(24), 0.0);
    }

    #[test]
    fn quantized_vector_recentres_on_zero() {
        // bias = 1<<15，写 32768 应得到 0
        let mut w = BitWriter::new();
        for _ in 0..3 {
            w.write_bits(32768, 16);
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let v = r.read_quantized_vector(1.0, 16);
        assert_eq!(v, [0.0, 0.0, 0.0]);
    }
}
