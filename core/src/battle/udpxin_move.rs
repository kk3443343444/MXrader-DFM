//! 位移解码与转向修正：`battle_proxy::battle::udpxin_move`。
//!
//! 复刻样本 `src/battle/udpxin_move.rs`，也是文件名里 **"3D转向修正"** 的落点。
//!
//! ## 1. FRepMovement 解码
//!
//! `BP_DFMCharacter_C` 的 handle 7 是 `ReplicatedMovement`（`FRepMovement`），
//! UE 把它压缩成一串位：
//!
//! ```text
//! uint8/位 PackedFlags
//!     bit0 bSimulatedPhysicSleep
//!     bit1 bRepPhysics
//!     bit2 该 actor 是否带 base（移动平台）
//!     bit3 bRelativeRotation（旋转是相对量）
//! 若 bRepPhysics:
//!     Location  (FVector_NetQuantize100, 每分量 MaxBitsPerComponent 位 + 符号)
//!     Rotation  (Rotator，三轴 16 位)
//!     LinearVelocity (FVector_NetQuantize100)
//! 否则:
//!     [若带 base] Location
//!     [若 bRelativeRotation] Rotation
//!     LinearVelocity
//! ```
//!
//! 位宽、Scale（100 / 1）与小版本的 PackedFlags 语义都有漂移，全部收进
//! `RepMovementProfile`；样本里的 `movement RepLayout not exactly closed` 就是
//! 在属性块结束标记（`CMD_END`）上没对齐时抛出的。
//!
//! ## 2. 转向修正
//!
//! UE 的 `FRotator.Yaw` 是"度，绕 Z 轴，从 +X 轴起算，**俯视顺时针为正**"，
//! 而雷达地图的"北"通常取 +X（零号大坝）或 -Y（长弓溪谷）。再叠加：
//!
//! * `yaw_offset_deg` —— 地图北向与 UE +X 的夹角；
//! * `yaw_sign` —— UE Y 轴方向（左手系）导致的镜像；
//! * pitch 透视 —— 3D 模式下朝向箭头要按俯仰做长度压缩。
//!
//! 三者合起来就是 `RotationCorrection`，其输出是**已经修正好的雷达朝向**，
//! 直接进 WS 的 `yaw` 字段，前端不再做任何换算。

use serde::{Deserialize, Serialize};

use super::codec::bitstream::BitReader;
use super::udpxin_entity::{Rot3, Vec3};

/// 位置/速度的压缩参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepMovementProfile {
    pub name: String,
    /// PackedFlags 是否是一个完整的字节（UE5 是 1 字节，部分 fork 只用 4 位）。
    pub packed_flags_bytes: u8,
    /// 位置每分量幅值位宽（另有 1 位符号）。20 位 × 0.01 只能覆盖 ±10 485.75
    /// 单位（≈105 m），而本工程自己的坐标用例是 ±123 456 cm（≈1.23 km，
    /// 见 `web::battle_view::tests::positions_are_converted_from_centimetres_to_metres`），
    /// 20 位档永远解不出真实对局的位移。取 24 位：±167 772 cm ≈ ±1.68 km。
    pub location_bits: u32,
    /// `FVector_NetQuantize100` 的 Scale：100 = 0.01 单位（即 cm 定点两位小数）。
    pub location_scale: f32,
    /// 速度每分量位宽。
    pub velocity_bits: u32,
    pub velocity_scale: f32,
    /// 是否有 `bRelativeRotation` 条件位。
    pub has_relative_rotation_bit: bool,
    /// 是否有 base（移动平台）条件位。
    pub has_base_bit: bool,
    /// 旋转是否 16 位量化（false = 3×32 位浮点，某些 fork）。
    pub rotator_16bit: bool,
}

impl Default for RepMovementProfile {
    fn default() -> Self {
        Self::dfm_r39()
    }
}

impl RepMovementProfile {
    /// 三角洲 r39 实测档。
    pub fn dfm_r39() -> Self {
        Self {
            name: "dfm-r39".to_string(),
            packed_flags_bytes: 1,
            location_bits: 24,
            location_scale: 0.01,
            velocity_bits: 16,
            velocity_scale: 1.0,
            has_relative_rotation_bit: true,
            has_base_bit: true,
            rotator_16bit: true,
        }
    }

    pub fn load_or_default(json: Option<&str>) -> Self {
        json.and_then(|s| serde_json::from_str::<Self>(s).ok()).unwrap_or_default()
    }
}

/// 解出的位移块。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepMovement {
    pub b_simulated_physic_sleep: bool,
    pub b_rep_physics: bool,
    pub b_server_has_base: bool,
    pub b_relative_rotation: bool,
    pub location: Vec3,
    pub rotation: Rot3,
    pub linear_velocity: Vec3,
    /// 是否解出了有效位置（有些包里只有速度）。
    pub has_location: bool,
    pub has_rotation: bool,
    /// 消耗的位数，便于属性块对齐。
    pub bits_used: usize,
}

/// 读一个 `FVector_NetQuantize<Scale>`：每分量 = 1 位符号 + 位宽位幅值的整数，
/// 再按 `scale` 还原（UE 的定点压缩）。
fn read_net_quantize_vector(r: &mut BitReader<'_>, bits: u32, scale: f32) -> Vec3 {
    let mut out = [0f32; 3];
    for slot in out.iter_mut() {
        let neg = r.read_bit();
        let raw = r.read_bits(bits.min(32)) as i64;
        let v = raw as f32 * scale;
        *slot = if neg { -v } else { v };
    }
    Vec3 { x: out[0], y: out[1], z: out[2] }
}

/// 读三轴 32 位浮点旋转（少数分支/版本）。
fn read_rotator_f32(r: &mut BitReader<'_>) -> Rot3 {
    let mut v = [0f32; 3];
    for slot in v.iter_mut() {
        let raw = r.read_bits(32);
        *slot = f32::from_bits(raw);
    }
    Rot3 { pitch: v[0], yaw: v[1], roll: v[2] }
}

/// 解析 `FRepMovement`。
pub fn read_rep_movement(r: &mut BitReader<'_>, p: &RepMovementProfile) -> Option<RepMovement> {
    let start = r.bit_pos();
    let mut out = RepMovement::default();

    // PackedFlags
    let flags = if p.packed_flags_bytes >= 1 { r.read_bits(8) } else { r.read_bits(4) };
    out.b_simulated_physic_sleep = flags & 0x01 != 0;
    out.b_rep_physics = flags & 0x02 != 0;
    if p.has_base_bit {
        out.b_server_has_base = flags & 0x04 != 0;
    }
    if p.has_relative_rotation_bit {
        out.b_relative_rotation = flags & 0x08 != 0;
    }

    if out.b_rep_physics {
        out.location = read_net_quantize_vector(r, p.location_bits, p.location_scale);
        out.rotation = if p.rotator_16bit {
            Rot3::read_from(r)
        } else {
            read_rotator_f32(r)
        };
        out.linear_velocity = read_net_quantize_vector(r, p.velocity_bits, p.velocity_scale);
        out.has_location = true;
        out.has_rotation = true;
    } else {
        if out.b_server_has_base {
            out.location = read_net_quantize_vector(r, p.location_bits, p.location_scale);
            out.has_location = true;
        }
        if out.b_relative_rotation {
            out.rotation = if p.rotator_16bit {
                Rot3::read_from(r)
            } else {
                read_rotator_f32(r)
            };
            out.has_rotation = true;
        }
        out.linear_velocity = read_net_quantize_vector(r, p.velocity_bits, p.velocity_scale);
    }

    if r.overflowed() {
        return None;
    }
    out.bits_used = r.bit_pos() - start;
    Some(out)
}

/// 写入（自检/协议重放用，与 `read_rep_movement` 严格互逆）。
pub fn write_rep_movement(
    w: &mut super::codec::bitstream::BitWriter,
    m: &RepMovement,
    p: &RepMovementProfile,
) {
    let mut flags = 0u32;
    if m.b_simulated_physic_sleep {
        flags |= 0x01;
    }
    if m.b_rep_physics {
        flags |= 0x02;
    }
    if m.b_server_has_base {
        flags |= 0x04;
    }
    if m.b_relative_rotation {
        flags |= 0x08;
    }
    if p.packed_flags_bytes >= 1 {
        w.write_bits(flags, 8);
    } else {
        w.write_bits(flags, 4);
    }

    let write_vec = |w: &mut super::codec::bitstream::BitWriter, v: Vec3, bits: u32, scale: f32| {
        for c in [v.x, v.y, v.z] {
            let neg = c < 0.0;
            let raw = (c.abs() / scale).round() as u32;
            w.write_bit(neg);
            w.write_bits(raw, bits.min(32));
        }
    };

    if m.b_rep_physics {
        write_vec(w, m.location, p.location_bits, p.location_scale);
        write_rot(w, m.rotation, p);
        write_vec(w, m.linear_velocity, p.velocity_bits, p.velocity_scale);
    } else {
        if m.b_server_has_base {
            write_vec(w, m.location, p.location_bits, p.location_scale);
        }
        if m.b_relative_rotation {
            write_rot(w, m.rotation, p);
        }
        write_vec(w, m.linear_velocity, p.velocity_bits, p.velocity_scale);
    }
}

fn write_rot(
    w: &mut super::codec::bitstream::BitWriter,
    rot: Rot3,
    p: &RepMovementProfile,
) {
    use super::codec::bitstream::deg_to_rotator;
    if p.rotator_16bit {
        w.write_bits(deg_to_rotator(rot.pitch) as u32, 16);
        w.write_bits(deg_to_rotator(rot.yaw) as u32, 16);
        w.write_bits(deg_to_rotator(rot.roll) as u32, 16);
    } else {
        for c in [rot.pitch, rot.yaw, rot.roll] {
            w.write_bits(c.to_bits(), 32);
        }
    }
}

// ---------------------------------------------------------------------------
// 转向修正（3D_转向修正）
// ---------------------------------------------------------------------------

/// 罗盘朝向修正参数，逐地图标定。
///
/// **出厂默认 = 恒等映射**（`yaw_offset_deg = 0`、`yaw_sign = +1`），与
/// `web/maps.json` 的出厂标定一致，也与前端 `radar.js` 的内置默认一致 ——
/// 三层必须一致，否则会出现"双方都修正一次"的双重纠正（朝向看起来偏 2 倍）。
///
/// 服务端若下发的是**未修正的 UE 原始 yaw**，用 [`RotationCorrection::ue_raw`]
/// （`offset = -90`、`sign = -1`）：UE yaw 0（+X）→ 270°（西）。判别与标定方法见
/// `web/tiles/README.md` §4.2 与 `docs/PROTOCOL.md` §2 第 6 步。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotationCorrection {
    /// 地图"北"相对 UE +X 轴的夹角（度）。
    pub yaw_offset_deg: f32,
    /// UE Y 轴方向导致的镜像：左手系地图取 -1。
    pub yaw_sign: f32,
    /// 是否启用 3D（俯仰）修正。
    pub enable_3d: bool,
    /// 俯仰对朝向箭头长度的压缩增益（0 = 不压缩）。
    pub pitch_gain: f32,
    /// 地图是否随本地玩家朝向旋转（"跟随朝向"模式）。
    pub follow_heading: bool,
}

impl Default for RotationCorrection {
    fn default() -> Self {
        Self {
            yaw_offset_deg: 0.0,
            yaw_sign: 1.0,
            enable_3d: true,
            pitch_gain: 0.6,
            follow_heading: false,
        }
    }
}

impl RotationCorrection {
    /// 服务端下发原始 UE yaw 时使用（+X 为 0，俯视顺时针）。
    pub fn ue_raw() -> Self {
        Self { yaw_offset_deg: -90.0, yaw_sign: -1.0, ..Self::default() }
    }

    /// 从 `maps.json` 的一条地图记录构造。
    pub fn from_map_entry(v: &serde_json::Value) -> Self {
        let d = Self::default();
        Self {
            yaw_offset_deg: v
                .get("yaw_offset_deg")
                .and_then(|x| x.as_f64())
                .map(|x| x as f32)
                .unwrap_or(d.yaw_offset_deg),
            yaw_sign: v
                .get("yaw_sign")
                .and_then(|x| x.as_f64())
                .map(|x| x as f32)
                .unwrap_or(d.yaw_sign),
            enable_3d: v.get("enable_3d").and_then(|x| x.as_bool()).unwrap_or(d.enable_3d),
            pitch_gain: v
                .get("pitch_gain")
                .and_then(|x| x.as_f64())
                .map(|x| x as f32)
                .unwrap_or(d.pitch_gain),
            follow_heading: false,
        }
    }

    /// 从整份 `maps.json` 里取某个地图键的修正参数；缺失则返回默认。
    ///
    /// `maps.json` 的顶层就是地图表（也可能被包在 `"maps"` 下）；`"default"` 是
    /// 显式的兜底条目。**未知地图必须回落 [`RotationCorrection::default()`]**
    /// （出厂恒等映射，与 `radar.js`/`maps.json` 一致）——不能拿表里"第一个"
    /// 条目顶替：`serde_json` 默认按 key 排序，那样 `Nope` 会随机拿到某张图的
    /// `yaw_sign`（例如 Layali 的 -1），朝向会被"纠正"成镜像。
    pub fn from_maps_json(json: &str, map_key: Option<&str>) -> Self {
        let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
            return Self::default();
        };
        let maps = root.get("maps").unwrap_or(&root);
        let entry = map_key.and_then(|k| maps.get(k)).or_else(|| maps.get("default"));
        match entry {
            Some(e) => Self::from_map_entry(e),
            None => Self::default(),
        }
    }

    /// UE yaw（度）→ 雷达罗盘朝向（度，0=北，顺时针）。
    pub fn heading(&self, ue_yaw_deg: f32, self_heading: Option<f32>) -> f32 {
        let mut h = ue_yaw_deg * self.yaw_sign + self.yaw_offset_deg;
        if self.follow_heading {
            // 地图自身被旋转了 -self_heading，所以标记要再抵消回来。
            if let Some(sh) = self_heading {
                h -= sh;
            }
        }
        normalize_deg(h)
    }

    /// 3D 模式下朝向箭头的可视长度：俯仰越陡，水平投影越短。
    pub fn heading_length_scale(&self, pitch_deg: f32) -> f32 {
        if !self.enable_3d {
            return 1.0;
        }
        let p = pitch_deg.to_radians().abs();
        (1.0 - self.pitch_gain * (1.0 - p.cos())).clamp(0.1, 1.0)
    }

    /// 世界坐标 → 地图坐标（米）。
    pub fn world_to_map(&self, pos: Vec3, origin: [f32; 2], scale: f32) -> [f32; 2] {
        [
            (pos.x - origin[0]) * scale * self.yaw_sign,
            (pos.y - origin[1]) * scale,
        ]
    }
}

/// 把角度规范到 [0,360)。
#[inline]
pub fn normalize_deg(d: f32) -> f32 {
    let m = d % 360.0;
    if m < 0.0 {
        m + 360.0
    } else {
        m
    }
}

/// 度 → 弧度。
#[inline]
pub fn deg2rad(d: f32) -> f32 {
    d * std::f32::consts::PI / 180.0
}

/// 由朝向（度）得到单位向量（雷达坐标系：+X 北，+Y 东）。
pub fn heading_to_unit(heading_deg: f32) -> [f32; 2] {
    let r = deg2rad(heading_deg);
    [r.cos(), r.sin()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::codec::bitstream::BitWriter;

    #[test]
    fn rep_movement_roundtrip_physics() {
        let p = RepMovementProfile::dfm_r39();
        let m = RepMovement {
            b_rep_physics: true,
            location: Vec3 { x: -12_345.67, y: 5_432.1, z: 120.0 },
            rotation: Rot3 { pitch: 12.0, yaw: 137.5, roll: -3.0 },
            linear_velocity: Vec3 { x: 600.0, y: -300.0, z: 0.0 },
            ..Default::default()
        };
        let mut w = BitWriter::new();
        write_rep_movement(&mut w, &m, &p);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let got = read_rep_movement(&mut r, &p).unwrap();
        assert!(got.b_rep_physics);
        assert!((got.location.x + 12_345.67).abs() < 1.0);
        assert!((got.location.y - 5_432.1).abs() < 1.0);
        assert!((got.rotation.yaw - 137.5).abs() < 0.02);
    }

    #[test]
    fn rep_movement_without_physics_section() {
        let p = RepMovementProfile::dfm_r39();
        let m = RepMovement {
            b_rep_physics: false,
            b_server_has_base: true,
            b_relative_rotation: true,
            location: Vec3 { x: 100.0, y: 200.0, z: 300.0 },
            rotation: Rot3 { pitch: 0.0, yaw: 90.0, roll: 0.0 },
            ..Default::default()
        };
        let mut w = BitWriter::new();
        write_rep_movement(&mut w, &m, &p);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let got = read_rep_movement(&mut r, &p).unwrap();
        assert!(got.has_location && got.has_rotation);
        assert!((got.location.z - 300.0).abs() < 0.01);
    }

    #[test]
    fn truncated_flags_are_rejected() {
        let p = RepMovementProfile::dfm_r39();
        let mut r = BitReader::new(&[0b0000_0010]); // bRepPhysics 但后面没数据
        assert!(read_rep_movement(&mut r, &p).is_none());
    }

    #[test]
    fn heading_correction_default_is_identity() {
        // 出厂恒等：与 web/maps.json 和 radar.js 的内置默认一致（避免双重纠正）。
        let c = RotationCorrection::default();
        assert!((c.heading(0.0, None) - 0.0).abs() < 0.01);
        assert!((c.heading(90.0, None) - 90.0).abs() < 0.01);
        assert!((c.heading(359.0, None) - 359.0).abs() < 0.01);
    }

    #[test]
    fn heading_correction_ue_raw_variant() {
        // 服务端下发原始 UE yaw 时的档位。
        let c = RotationCorrection::ue_raw();
        assert!((c.heading(0.0, None) - 270.0).abs() < 0.01, "UE +X -> 西");
        assert!((c.heading(90.0, None) - 180.0).abs() < 0.01, "UE +Y -> 南");
    }

    #[test]
    fn correction_can_be_loaded_from_maps_json() {
        let json = r#"{"maps":{"ZeroDam":{"yaw_offset_deg":0,"yaw_sign":1},
                                 "Layali":{"yaw_offset_deg":-90,"yaw_sign":-1}}}"#;
        let zd = RotationCorrection::from_maps_json(json, Some("ZeroDam"));
        assert!((zd.heading(45.0, None) - 45.0).abs() < 0.01);
        let ll = RotationCorrection::from_maps_json(json, Some("Layali"));
        assert!((ll.heading(0.0, None) - 270.0).abs() < 0.01);
        // 未知地图 / 坏 JSON 都回落默认
        assert_eq!(
            RotationCorrection::from_maps_json(json, Some("Nope")).yaw_sign,
            RotationCorrection::default().yaw_sign
        );
        assert_eq!(
            RotationCorrection::from_maps_json("not json", None).yaw_offset_deg,
            0.0
        );
    }

    #[test]
    fn follow_heading_mode_cancels_map_rotation() {
        let mut c = RotationCorrection::default();
        c.follow_heading = true;
        let h = c.heading(90.0, Some(30.0));
        assert!((h - 60.0).abs() < 0.01);
    }

    #[test]
    fn heading_length_shrinks_with_pitch() {
        let c = RotationCorrection::default();
        assert!((c.heading_length_scale(0.0) - 1.0).abs() < 1e-6);
        assert!(c.heading_length_scale(60.0) < 0.75);
        let mut flat = c.clone();
        flat.enable_3d = false;
        assert_eq!(flat.heading_length_scale(60.0), 1.0);
    }

    #[test]
    fn world_to_map_applies_origin_scale_and_mirror() {
        let c = RotationCorrection::default();
        let p = c.world_to_map(Vec3 { x: 1000.0, y: 2000.0, z: 0.0 }, [1000.0, 2000.0], 0.01);
        assert_eq!(p, [0.0, 0.0]);
        let q = c.world_to_map(Vec3 { x: 1100.0, y: 2100.0, z: 0.0 }, [1000.0, 2000.0], 0.01);
        assert_eq!(q, [1.0, 1.0]);
        // 镜像档（ue_raw）下 X 方向取反。
        let m = RotationCorrection::ue_raw();
        let r = m.world_to_map(Vec3 { x: 1100.0, y: 2100.0, z: 0.0 }, [1000.0, 2000.0], 0.01);
        assert_eq!(r, [-1.0, 1.0]);
    }

    #[test]
    fn normalize_deg_wraps_negatives() {
        assert_eq!(normalize_deg(-90.0), 270.0);
        assert_eq!(normalize_deg(450.0), 90.0);
        assert_eq!(normalize_deg(360.0), 0.0);
    }
}
