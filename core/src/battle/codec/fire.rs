//! 开火解码与弹道还原：`battle_proxy::battle::codec::fire`。
//!
//! 复刻样本 `src/battle/codec/fire.rs`，也是样本字符串里
//! **`battle_fire_cli 1.3.0 / sub_101877D34`** 这条链的实现。
//!
//! 样本把整个过程写得很明白，字段名就是需求说明书：
//!
//! ```text
//! ServerProcessWeaponEventDataForFirer          <- RPC 名
//! rpc_prefix_bits / event_type / fire_id_hex
//! initial_speed_source = FWeaponFireInfo.InitialSpeedQ100
//! fire_rotation / owner_velocity_m_s / initial_projectile_velocity_m_s / direction_unit
//! ballistic_formula:
//!     v = forward(FireRotation) * InitSpeed + OwnerVelocity
//!     origin = SpawnLocation
//! bullet_array_start_bit / bullet_decode_error
//! rpc_timestamp_start_bit / rpc_timestamp_seconds
//! variable_payload_region
//! ballistic_trajectory_complete      <- 完整解出
//! partial_raw_preserved              <- 部分解出，保留原始位
//! no_projectile_and_fire_move_exact  <- 无弹丸，用开火位移直接推
//! full_struct_end_candidate_bit
//! raw_preserved_not_required_for_ballistic_reconstruction
//! ```
//!
//! 雷达上一条"弹道"就是：**谁从哪儿、朝哪个方向、以多大初速打了一发**。
//! 有了它就能在雷达上画出射线，反推射手位置（这也是"被打了知道子弹从哪来"的原理）。

use serde::{Deserialize, Serialize};

use super::bitstream::BitReader;
use super::super::udpxin_entity::{Rot3, Vec3};

/// UE 默认重力（cm/s²）。
pub const UE_GRAVITY_CM_S2: f32 = 980.0;

/// 每米多少 UE 单位。
pub const CM_PER_M: f32 = 100.0;

/// 解码档位（`battle_fire_cli` 的版本号在这里落地）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FireCodecProfile {
    /// 解析器版本，对应样本的 `battle_fire_cli 1.3.0`。
    pub cli_version: String,
    /// RPC 名（UE 的 `ServerProcess...` 前缀）。
    pub rpc_name: String,
    /// RPC 前缀位数（调用者/函数名散列等）。
    pub rpc_prefix_bits: u32,
    /// 事件类型位宽。
    pub event_type_bits: u32,
    /// `InitialSpeedQ100` = 初速按 1/100 定点 → true。
    pub initial_speed_q100: bool,
    /// 是否有 `bullet_array` 区段。
    pub has_bullet_array: bool,
    /// 是否有 RPC 时间戳。
    pub has_rpc_timestamp: bool,
    /// 时间戳是否 16 位（1/100 秒）而非 32 位浮点秒。
    pub timestamp_16bit_centiseconds: bool,
}

impl Default for FireCodecProfile {
    fn default() -> Self {
        Self {
            cli_version: "1.3.0".to_string(),
            rpc_name: "ServerProcessWeaponEventDataForFirer".to_string(),
            rpc_prefix_bits: 8,
            event_type_bits: 8,
            initial_speed_q100: true,
            has_bullet_array: true,
            has_rpc_timestamp: true,
            timestamp_16bit_centiseconds: true,
        }
    }
}

impl FireCodecProfile {
    pub fn load_or_default(json: Option<&str>) -> Self {
        json.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
    }
}

/// 解码置信度（样本里三档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FireConfidence {
    /// `ballistic_trajectory_complete`：结构与尾部都对上了。
    #[default]
    Complete,
    /// `partial_raw_preserved`：只解出方向/初速，弹丸数组没懂，但射线可用。
    PartialRaw,
    /// `no_projectile_and_fire_move_exact`：没有弹丸信息，用开火位移推。
    MoveDerived,
}

/// 一次开火事件。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FireEvent {
    /// 射手通道（由 RPC 所在 actor 决定，engine 回填）。
    pub shooter_channel: Option<u32>,
    /// `fire_id_hex` —— 一发的唯一 ID，用于去重。
    pub fire_id: u64,
    /// `event_type` 原始值。
    pub event_type: u32,
    /// `fire_rotation` —— UE 朝向。
    pub fire_rotation: Rot3,
    /// `SpawnLocation` —— 枪口/射线起点（cm）。
    pub origin: Vec3,
    /// `owner_velocity_m_s` —— 射手自身速度（m/s，已单位化）。
    pub owner_velocity: Vec3,
    /// `FWeaponFireInfo.InitialSpeedQ100` 还原后的初速（cm/s）。
    pub initial_speed_cm_s: f32,
    /// `initial_projectile_velocity_m_s`：v = forward*InitSpeed + OwnerVelocity。
    pub projectile_velocity: Vec3,
    /// `direction_unit` —— 归一化方向。
    pub direction_unit: [f32; 3],
    /// `rpc_timestamp_seconds`.
    pub rpc_timestamp_s: f32,
    /// 弹丸数（霰弹枪 > 1）。
    pub projectile_count: u32,
    pub confidence: FireConfidence,
    /// `full_struct_end_candidate_bit`：属性块结束位置，供上层对齐。
    pub struct_end_bit: Option<usize>,
    /// 未解释的原始位（诊断落盘用）。
    pub raw_bits: Vec<u8>,
    /// 武器类名（由 `BP_Weapon*` 通道回填）。
    pub weapon_class: Option<String>,
    /// 命中标记：由 killchain 回填。
    pub hit: bool,
}

impl FireEvent {
    /// 方向单位向量（若解码时给出则直接用）。
    pub fn direction(&self) -> [f32; 3] {
        let d = self.direction_unit;
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if len > 1e-3 {
            [d[0] / len, d[1] / len, d[2] / len]
        } else {
            rotator_to_forward(self.fire_rotation)
        }
    }

    /// `ballistic_formula`：v = forward(FireRotation)*InitSpeed + OwnerVelocity。
    pub fn recompute_velocity(&mut self) {
        let f = rotator_to_forward(self.fire_rotation);
        self.direction_unit = f;
        let s = self.initial_speed_cm_s;
        // owner_velocity 是 m/s → 转 cm/s
        let ov = self.owner_velocity;
        self.projectile_velocity = Vec3 {
            x: f[0] * s + ov.x * CM_PER_M,
            y: f[1] * s + ov.y * CM_PER_M,
            z: f[2] * s + ov.z * CM_PER_M,
        };
    }
}

/// UE `FRotator::Vector()`：X=cp*cy, Y=cp*sy, Z=sp。
pub fn rotator_to_forward(r: Rot3) -> [f32; 3] {
    let p = r.pitch.to_radians();
    let y = r.yaw.to_radians();
    let (sp, cp) = p.sin_cos();
    let (sy, cy) = y.sin_cos();
    [cp * cy, cp * sy, sp]
}

/// 弹道采样点（世界坐标 cm）：`p(t) = origin + v*t - 0.5*g*t²`。
pub fn ballistic_points(ev: &FireEvent, max_range_m: f32, step_m: f32, gravity: f32) -> Vec<Vec3> {
    let d = ev.direction();
    let origin = ev.origin;
    let v = ev.projectile_velocity;
    let speed_h = ((v.x * v.x + v.y * v.y).sqrt()).max(1.0); // cm/s
    let t_total = (max_range_m * CM_PER_M) / speed_h;
    let step_t = (step_m * CM_PER_M) / speed_h;
    let mut out = Vec::new();
    let mut t = 0.0f32;
    while t <= t_total && out.len() < 512 {
        let dt = t;
        let x = origin.x + v.x * dt;
        let y = origin.y + v.y * dt;
        let z = origin.z + v.z * dt - 0.5 * gravity * dt * dt;
        let p = Vec3 { x, y, z };
        if p.is_finite() {
            out.push(p);
        }
        t += step_t.max(1e-3);
        let _ = d;
    }
    out
}

/// 一条雷达弹道（WS `traces[]` 的单元）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FireTrace {
    /// 起点（米，世界系）
    pub x1: f32,
    pub y1: f32,
    pub z1: f32,
    /// 终点（米）
    pub x2: f32,
    pub y2: f32,
    pub z2: f32,
    pub shooter_uuid: Option<String>,
    pub weapon_class: Option<String>,
    pub ts_ms: u64,
    pub confidence: FireConfidence,
}

/// 由开火事件生成雷达弹道（截取前 `span_m` 米）。
pub fn to_trace(ev: &FireEvent, span_m: f32, ts_ms: u64) -> FireTrace {
    let pts = ballistic_points(ev, span_m, span_m, UE_GRAVITY_CM_S2);
    let start = pts.first().copied().unwrap_or(ev.origin);
    let end = pts.last().copied().unwrap_or(ev.origin);
    let s = start.to_metres();
    let e = end.to_metres();
    FireTrace {
        x1: s[0],
        y1: s[1],
        z1: s[2],
        x2: e[0],
        y2: e[1],
        z2: e[2],
        shooter_uuid: None,
        weapon_class: ev.weapon_class.clone(),
        ts_ms,
        confidence: ev.confidence,
    }
}

/// 解码错误。
#[derive(Debug, Clone, PartialEq)]
pub enum FireDecodeError {
    Truncated,
    ImplausibleSpeed(f32),
    ImplausibleProjectiles(u32),
    BitOverflow,
}

/// 从 RPC 净荷解出开火事件。
///
/// `rpc_payload_start_bit` 指净荷里 RPC 参数的起点（调用方按 bunch 头算好）。
pub fn decode_fire_event(
    r: &mut BitReader<'_>,
    profile: &FireCodecProfile,
    max_projectiles: u32,
) -> Result<FireEvent, FireDecodeError> {
    // RPC 前缀（函数名散列 + 调用者索引）
    let _prefix = r.read_bits(profile.rpc_prefix_bits.min(32));
    let event_type = r.read_bits(profile.event_type_bits.min(32));
    let fire_id = r.read_packed_int64();

    let fire_rotation = Rot3::read_from(r);
    let origin = read_vec(r);
    let owner_velocity = read_vec(r);

    let raw_speed = r.read_bits(20) as f32;
    let initial_speed_cm_s = if profile.initial_speed_q100 { raw_speed * 100.0 } else { raw_speed };
    if !(10.0..=2_000_00.0).contains(&initial_speed_cm_s) {
        // 100 到 2000 m/s 之外的初速一定是解错了
        if initial_speed_cm_s != 0.0 {
            return Err(FireDecodeError::ImplausibleSpeed(initial_speed_cm_s));
        }
    }

    let mut ev = FireEvent {
        fire_id,
        event_type,
        fire_rotation,
        origin,
        owner_velocity,
        initial_speed_cm_s,
        confidence: FireConfidence::Complete,
        ..Default::default()
    };

    if profile.has_bullet_array {
        let bullet_count = r.read_bits(8);
        if bullet_count > max_projectiles {
            return Err(FireDecodeError::ImplausibleProjectiles(bullet_count));
        }
        ev.projectile_count = bullet_count.max(1);
        // 每条弹丸 3×10 位偏移 + 可选 1 位命中
        for _ in 0..bullet_count {
            let _dx = r.read_bits(10);
            let _dy = r.read_bits(10);
            let _dz = r.read_bits(10);
            let hit = r.read_bit();
            ev.hit |= hit;
        }
    }

    if profile.has_rpc_timestamp {
        ev.rpc_timestamp_s = if profile.timestamp_16bit_centiseconds {
            r.read_bits(16) as f32 / 100.0
        } else {
            f32::from_bits(r.read_bits(32))
        };
    }

    if r.overflowed() {
        return Err(FireDecodeError::BitOverflow);
    }
    ev.recompute_velocity();
    ev.struct_end_bit = Some(r.bit_pos());
    Ok(ev)
}

fn read_vec(r: &mut BitReader<'_>) -> Vec3 {
    // 净荷里的向量是 3×32 位浮点（`variable_payload_region`）。
    let mut v = [0f32; 3];
    for slot in v.iter_mut() {
        *slot = f32::from_bits(r.read_bits(32));
    }
    Vec3 { x: v[0], y: v[1], z: v[2] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::codec::bitstream::{BitWriter, deg_to_rotator};

    fn write_vec(w: &mut BitWriter, v: Vec3) {
        for c in [v.x, v.y, v.z] {
            w.write_bits(c.to_bits(), 32);
        }
    }

    fn write_fire(
        w: &mut BitWriter,
        p: &FireCodecProfile,
        rot: Rot3,
        origin: Vec3,
        owner_v: Vec3,
        speed_cm_s: f32,
        bullets: u32,
        hit: bool,
    ) {
        w.write_bits(0, p.rpc_prefix_bits);
        w.write_bits(7, p.event_type_bits);
        // fire_id: packed
        let mut id = 0x1234u64;
        loop {
            let mut b = (id & 0x7f) as u32;
            id >>= 7;
            if id != 0 {
                b |= 0x80;
            }
            w.write_bits(b, 8);
            if id == 0 {
                break;
            }
        }
        w.write_bits(deg_to_rotator(rot.pitch) as u32, 16);
        w.write_bits(deg_to_rotator(rot.yaw) as u32, 16);
        w.write_bits(deg_to_rotator(rot.roll) as u32, 16);
        write_vec(w, origin);
        write_vec(w, owner_v);
        w.write_bits((speed_cm_s / 100.0) as u32, 20);
        if p.has_bullet_array {
            w.write_bits(bullets, 8);
            for _ in 0..bullets {
                w.write_bits(0, 10);
                w.write_bits(0, 10);
                w.write_bits(0, 10);
                w.write_bit(hit);
            }
        }
        if p.has_rpc_timestamp {
            w.write_bits(1234, 16);
        }
    }

    #[test]
    fn decode_full_fire_event() {
        let p = FireCodecProfile::default();
        let mut w = BitWriter::new();
        write_fire(
            &mut w,
            &p,
            Rot3 { pitch: 5.0, yaw: 45.0, roll: 0.0 },
            Vec3 { x: 100.0, y: 200.0, z: 300.0 },
            Vec3 { x: 3.0, y: 0.0, z: 0.0 },
            60_000.0,
            1,
            true,
        );
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let ev = decode_fire_event(&mut r, &p, 64).unwrap();
        assert_eq!(ev.event_type, 7);
        assert_eq!(ev.fire_id, 0x1234);
        assert!((ev.fire_rotation.yaw - 45.0).abs() < 0.1);
        assert!((ev.initial_speed_cm_s - 60_000.0).abs() < 1.0);
        assert!(ev.hit);
        assert_eq!(ev.confidence, FireConfidence::Complete);
    }

    #[test]
    fn ballistic_formula_adds_owner_velocity_in_cm_per_s() {
        let mut ev = FireEvent {
            fire_rotation: Rot3 { pitch: 0.0, yaw: 0.0, roll: 0.0 },
            initial_speed_cm_s: 10_000.0,
            owner_velocity: Vec3 { x: 10.0, y: 0.0, z: 0.0 }, // 10 m/s
            ..Default::default()
        };
        ev.recompute_velocity();
        // forward(0,0) = +X ; 10000 + 1000 = 11000 cm/s
        assert!((ev.projectile_velocity.x - 11_000.0).abs() < 1.0);
    }

    #[test]
    fn forward_vector_matches_ue_convention() {
        let f = rotator_to_forward(Rot3 { pitch: 0.0, yaw: 0.0, roll: 0.0 });
        assert!((f[0] - 1.0).abs() < 1e-5, "yaw 0 must point +X");
        let f = rotator_to_forward(Rot3 { pitch: 0.0, yaw: 90.0, roll: 0.0 });
        assert!(f[1].abs() > 0.99, "yaw 90 must point +Y");
        let f = rotator_to_forward(Rot3 { pitch: 90.0, yaw: 0.0, roll: 0.0 });
        assert!((f[2] - 1.0).abs() < 1e-5, "pitch +90 must point +Z");
    }

    #[test]
    fn gravity_drops_the_trajectory() {
        let ev = FireEvent {
            fire_rotation: Rot3 { pitch: 0.0, yaw: 0.0, roll: 0.0 },
            initial_speed_cm_s: 50_000.0,
            projectile_velocity: Vec3 { x: 50_000.0, y: 0.0, z: 0.0 },
            origin: Vec3 { x: 0.0, y: 0.0, z: 150.0 },
            ..Default::default()
        };
        let pts = ballistic_points(&ev, 100.0, 10.0, UE_GRAVITY_CM_S2);
        assert!(pts.len() > 2);
        assert!(pts.last().unwrap().z < pts[0].z, "bullet must fall");
    }

    #[test]
    fn trace_converts_centimetres_to_metres() {
        let ev = FireEvent {
            origin: Vec3 { x: 1000.0, y: 2000.0, z: 100.0 },
            projectile_velocity: Vec3 { x: 30_000.0, y: 0.0, z: 0.0 },
            initial_speed_cm_s: 30_000.0,
            ..Default::default()
        };
        let t = to_trace(&ev, 10.0, 42);
        assert!((t.x1 - 10.0).abs() < 0.01);
        assert!((t.y1 - 20.0).abs() < 0.01);
        assert!(t.x2 > t.x1);
        assert_eq!(t.ts_ms, 42);
    }

    #[test]
    fn implausible_projectile_count_is_rejected() {
        let p = FireCodecProfile::default();
        let mut w = BitWriter::new();
        write_fire(
            &mut w,
            &p,
            Rot3::default(),
            Vec3::default(),
            Vec3::default(),
            50_000.0,
            200,
            false,
        );
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            decode_fire_event(&mut r, &p, 64),
            Err(FireDecodeError::ImplausibleProjectiles(200))
        );
    }

    #[test]
    fn truncated_rpc_returns_bit_overflow() {
        let p = FireCodecProfile::default();
        let mut r = BitReader::new(&[0u8; 3]);
        assert!(decode_fire_event(&mut r, &p, 64).is_err());
    }
}
