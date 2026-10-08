//! 击杀链：`battle_proxy::battle::codec::killchain`。
//!
//! 复刻样本 `src/battle/codec/killchain.rs`。样本把伤害类型枚举整张表内嵌，
//! 并且明确要求：**数组长度 0 直接当作损坏包丢掉**（`invalid zero-length kill array`），
//! 因为"空击杀数组"在真实对局里不可能出现——它只可能是错位解析的产物。
//!
//! ```text
//! EKilledByWeapon / EkilledBySelf / EkilledByPoisonGas / EKilledFallDown
//! EKilledFromImpendingDeath / EKilledFromBuff / EKilledByGm / EKilledFromEnvExplosion
//! EKilledByVehicleWeapon / EKilledByAssassinateDamage / EKilledByBattleFieldSupportSkill
//! EKilledBySectorArtilerrateSkill / EKilledByGuidedMissleSkill
//! invalid zero-length kill array
//! ```
//!
//! 顺带解释为什么雷达需要它：击杀链给出**"谁杀了谁"**，配合弹道还原就能
//! 把自己的死亡归因到具体方向（"你被 137° 方向、420 米外的人打死"）。

use serde::{Deserialize, Serialize};

use super::bitstream::BitReader;
use super::super::udpxin_entity::Vec3;

/// 伤害类型（枚举名与游戏内一致，含游戏自身的拼写错误 `Artilerrate`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KillDamageType {
    Weapon,
    /// 枚举名在游戏里是 `EkilledBySelf`；这里不能叫 `Self`（Rust 关键字）。
    BySelf,
    PoisonGas,
    FallDown,
    ImpendingDeath,
    Buff,
    Gm,
    EnvExplosion,
    VehicleWeapon,
    AssassinateDamage,
    BattleFieldSupportSkill,
    /// 游戏内拼写为 `EKilledBySectorArtilerrateSkill`。
    SectorArtillerySkill,
    GuidedMissileSkill,
    Unknown,
}

impl KillDamageType {
    /// 原始枚举名（进 JSON 时保持与样本字符串一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            KillDamageType::Weapon => "EKilledByWeapon",
            KillDamageType::BySelf => "EkilledBySelf",
            KillDamageType::PoisonGas => "EkilledByPoisonGas",
            KillDamageType::FallDown => "EKilledFallDown",
            KillDamageType::ImpendingDeath => "EKilledFromImpendingDeath",
            KillDamageType::Buff => "EKilledFromBuff",
            KillDamageType::Gm => "EKilledByGm",
            KillDamageType::EnvExplosion => "EKilledFromEnvExplosion",
            KillDamageType::VehicleWeapon => "EKilledByVehicleWeapon",
            KillDamageType::AssassinateDamage => "EKilledByAssassinateDamage",
            KillDamageType::BattleFieldSupportSkill => "EKilledByBattleFieldSupportSkill",
            KillDamageType::SectorArtillerySkill => "EKilledBySectorArtilerrateSkill",
            KillDamageType::GuidedMissileSkill => "EKilledByGuidedMissleSkill",
            KillDamageType::Unknown => "EUnknown",
        }
    }

    pub fn from_str_opt(s: &str) -> Self {
        match s {
            "EKilledByWeapon" => KillDamageType::Weapon,
            "EkilledBySelf" => KillDamageType::BySelf,
            "EkilledByPoisonGas" => KillDamageType::PoisonGas,
            "EKilledFallDown" => KillDamageType::FallDown,
            "EKilledFromImpendingDeath" => KillDamageType::ImpendingDeath,
            "EKilledFromBuff" => KillDamageType::Buff,
            "EKilledByGm" => KillDamageType::Gm,
            "EKilledFromEnvExplosion" => KillDamageType::EnvExplosion,
            "EKilledByVehicleWeapon" => KillDamageType::VehicleWeapon,
            "EKilledByAssassinateDamage" => KillDamageType::AssassinateDamage,
            "EKilledByBattleFieldSupportSkill" => KillDamageType::BattleFieldSupportSkill,
            "EKilledBySectorArtilerrateSkill" => KillDamageType::SectorArtillerySkill,
            "EKilledByGuidedMissleSkill" => KillDamageType::GuidedMissileSkill,
            _ => KillDamageType::Unknown,
        }
    }

    pub fn from_raw(v: u8) -> Self {
        match v {
            0 => KillDamageType::Weapon,
            1 => KillDamageType::BySelf,
            2 => KillDamageType::PoisonGas,
            3 => KillDamageType::FallDown,
            4 => KillDamageType::ImpendingDeath,
            5 => KillDamageType::Buff,
            6 => KillDamageType::Gm,
            7 => KillDamageType::EnvExplosion,
            8 => KillDamageType::VehicleWeapon,
            9 => KillDamageType::AssassinateDamage,
            10 => KillDamageType::BattleFieldSupportSkill,
            11 => KillDamageType::SectorArtillerySkill,
            12 => KillDamageType::GuidedMissileSkill,
            _ => KillDamageType::Unknown,
        }
    }

    /// 中文短名（雷达击杀条用）。
    pub fn zh(self) -> &'static str {
        match self {
            KillDamageType::Weapon => "武器击杀",
            KillDamageType::BySelf => "自伤",
            KillDamageType::PoisonGas => "毒气",
            KillDamageType::FallDown => "坠落",
            KillDamageType::ImpendingDeath => "濒死",
            KillDamageType::Buff => "增益",
            KillDamageType::Gm => "管理员",
            KillDamageType::EnvExplosion => "环境爆炸",
            KillDamageType::VehicleWeapon => "载具武器",
            KillDamageType::AssassinateDamage => "处决",
            KillDamageType::BattleFieldSupportSkill => "战场支援",
            KillDamageType::SectorArtillerySkill => "区域炮击",
            KillDamageType::GuidedMissileSkill => "制导导弹",
            KillDamageType::Unknown => "未知",
        }
    }
}

/// 一条击杀记录。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Kill {
    pub killer_guid: Option<u64>,
    pub victim_guid: Option<u64>,
    /// 由 IdentityTable 回填的展示名。
    pub killer_name: Option<String>,
    pub victim_name: Option<String>,
    pub killer_is_local: bool,
    pub victim_is_local: bool,
    pub damage_type: KillDamageType,
    pub damage_type_raw: u8,
    /// `DeadthDamageInfos` 中的伤害值。
    pub damage: f32,
    /// 击杀点（cm）。
    pub position: Vec3,
    /// 击杀距离（cm，由 engine 用双方位置补）。
    pub distance_cm: Option<f32>,
    pub ts_ms: u64,
    /// `RevengeKillInfo` 非空 → 复仇击杀。
    pub revenge: bool,
    pub is_assist: bool,
}

impl Default for KillDamageType {
    fn default() -> Self {
        KillDamageType::Unknown
    }
}

/// 击杀链解码错误。
#[derive(Debug, Clone, PartialEq)]
pub enum KillChainError {
    /// 样本：长度为 0 的击杀数组不可信。
    InvalidZeroLengthKillArray,
    ImplausibleLength(u32),
    BitOverflow,
}

/// 一条击杀记录在流里的固定长度（12 字节）。
pub const KILL_RECORD_BYTES: usize = 12;

/// 解码一组击杀记录。
pub fn decode_kill_array(
    r: &mut BitReader<'_>,
    max_kills: u32,
) -> Result<Vec<Kill>, KillChainError> {
    let count = r.read_packed_int();
    if count == 0 {
        return Err(KillChainError::InvalidZeroLengthKillArray);
    }
    if count > max_kills {
        return Err(KillChainError::ImplausibleLength(count));
    }

    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let killer = r.read_packed_int64();
        let victim = r.read_packed_int64();
        let damage_type_raw = r.read_bits(8) as u8;
        let damage = r.read_compressed_float(16);
        let x = r.read_bits(32);
        let y = r.read_bits(32);
        let z = r.read_bits(32);
        let flags = r.read_bits(8);
        let _ = r.read_packed_int(); // 时间戳（相对局内时钟）

        out.push(Kill {
            killer_guid: (killer != 0).then_some(killer),
            victim_guid: (victim != 0).then_some(victim),
            damage_type: KillDamageType::from_raw(damage_type_raw),
            damage_type_raw,
            damage,
            position: Vec3 {
                x: f32::from_bits(x),
                y: f32::from_bits(y),
                z: f32::from_bits(z),
            },
            revenge: flags & 0x01 != 0,
            is_assist: flags & 0x02 != 0,
            ..Default::default()
        });

        if r.overflowed() {
            return Err(KillChainError::BitOverflow);
        }
    }
    Ok(out)
}

/// 击杀链接口：把 decoder 与身份表串起来。
#[derive(Debug, Default)]
pub struct KillChain {
    kills: std::collections::VecDeque<Kill>,
    /// 保留上限（雷达只显示最近 N 条）。
    capacity: usize,
    /// 总击杀数（诊断）。
    pub total: u64,
    /// 自伤 / 环境死亡的计数（样本 `death_markers` 相关）。
    pub death_markers: u64,
}

impl KillChain {
    pub fn new(capacity: usize) -> Self {
        Self { kills: std::collections::VecDeque::with_capacity(capacity), capacity, ..Default::default() }
    }

    pub fn push(&mut self, kill: Kill) {
        self.total += 1;
        if kill.killer_guid.is_none() || kill.killer_guid == kill.victim_guid {
            self.death_markers += 1;
        }
        self.kills.push_back(kill);
        while self.kills.len() > self.capacity {
            self.kills.pop_front();
        }
    }

    pub fn extend(&mut self, kills: impl IntoIterator<Item = Kill>) {
        for k in kills {
            self.push(k);
        }
    }

    pub fn recent(&self) -> impl Iterator<Item = &Kill> {
        self.kills.iter()
    }

    pub fn len(&self) -> usize {
        self.kills.len()
    }
    pub fn is_empty(&self) -> bool {
        self.kills.is_empty()
    }

    pub fn clear(&mut self) {
        self.kills.clear();
    }

    /// 供 WS 序列化的击杀条（最近 20 条，倒序）。
    pub fn to_json_recent(&self, limit: usize) -> Vec<serde_json::Value> {
        self.kills
            .iter()
            .rev()
            .take(limit)
            .map(|k| {
                let p = k.position.to_metres();
                serde_json::json!({
                    "ts": k.ts_ms,
                    "killer": k.killer_name.clone().unwrap_or_else(|| "未知".to_string()),
                    "victim": k.victim_name.clone().unwrap_or_else(|| "未知".to_string()),
                    "weapon": k.damage_type.as_str(),
                    "damage_type": k.damage_type.zh(),
                    "damage": k.damage,
                    "distance": k.distance_cm.map(|d| d / 100.0),
                    "x": p[0], "y": p[1], "z": p[2],
                    "revenge": k.revenge,
                    "assist": k.is_assist,
                    "local": k.killer_is_local || k.victim_is_local,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::codec::bitstream::BitWriter;

    fn write_kill(w: &mut BitWriter, killer: u64, victim: u64, dtype: u8, pos: Vec3) {
        for g in [killer, victim] {
            let mut v = g;
            loop {
                let mut b = (v & 0x7f) as u32;
                v >>= 7;
                if v != 0 {
                    b |= 0x80;
                }
                w.write_bits(b, 8);
                if v == 0 {
                    break;
                }
            }
        }
        w.write_bits(dtype as u32, 8);
        // compressed float: bits=8, value, sign
        w.write_bits(8, 6);
        w.write_bits(100, 8);
        w.write_bit(false);
        for c in [pos.x, pos.y, pos.z] {
            w.write_bits(c.to_bits(), 32);
        }
        w.write_bits(0b11, 8); // revenge + assist
        w.write_bits(0, 8); // timestamp packed
    }

    #[test]
    fn zero_length_array_is_rejected_like_the_sample() {
        let mut w = BitWriter::new();
        w.write_bits(0, 8);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            decode_kill_array(&mut r, 32),
            Err(KillChainError::InvalidZeroLengthKillArray)
        );
    }

    #[test]
    fn decode_one_kill_with_flags_and_position() {
        let mut w = BitWriter::new();
        w.write_bits(1, 8); // count = 1
        write_kill(&mut w, 0xAA, 0xBB, 0, Vec3 { x: 100.0, y: 200.0, z: 10.0 });
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let kills = decode_kill_array(&mut r, 32).unwrap();
        assert_eq!(kills.len(), 1);
        let k = &kills[0];
        assert_eq!(k.killer_guid, Some(0xAA));
        assert_eq!(k.victim_guid, Some(0xBB));
        assert_eq!(k.damage_type, KillDamageType::Weapon);
        assert!(k.revenge && k.is_assist);
        assert_eq!(k.position.x, 100.0);
    }

    #[test]
    fn implausible_length_rejected() {
        let mut w = BitWriter::new();
        w.write_bits(0xFF, 8);
        w.write_bits(0x7F, 8);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert!(matches!(decode_kill_array(&mut r, 32), Err(KillChainError::ImplausibleLength(_))));
    }

    #[test]
    fn damage_type_roundtrip_and_chinese_labels() {
        for raw in 0u8..=13 {
            let t = KillDamageType::from_raw(raw);
            assert_eq!(KillDamageType::from_str_opt(t.as_str()), t, "raw {raw}");
            assert!(!t.zh().is_empty());
        }
        assert_eq!(KillDamageType::from_raw(200), KillDamageType::Unknown);
    }

    #[test]
    fn chain_ring_buffer_keeps_capacity_and_counts_deaths() {
        let mut c = KillChain::new(3);
        for i in 0..5u64 {
            c.push(Kill { killer_guid: Some(i + 1), victim_guid: Some(99), ts_ms: i, ..Default::default() });
        }
        assert_eq!(c.len(), 3);
        assert_eq!(c.total, 5);
        c.push(Kill { killer_guid: None, victim_guid: Some(1), ..Default::default() });
        assert_eq!(c.death_markers, 1);
    }

    #[test]
    fn json_recent_uses_metres_and_chinese_type() {
        let mut c = KillChain::new(8);
        c.push(Kill {
            killer_guid: Some(1),
            victim_guid: Some(2),
            killer_name: Some("老六".into()),
            damage_type: KillDamageType::GuidedMissileSkill,
            damage: 88.0,
            position: Vec3 { x: 1000.0, y: 2000.0, z: 0.0 },
            ts_ms: 7,
            ..Default::default()
        });
        let j = c.to_json_recent(5);
        assert_eq!(j[0]["killer"], "老六");
        assert_eq!(j[0]["damage_type"], "制导导弹");
        assert_eq!(j[0]["x"], 10.0);
    }

    #[test]
    fn never_panics_on_random_bytes() {
        for seed in 0u32..64 {
            let bytes: Vec<u8> = (0..128)
                .map(|i| (seed.wrapping_mul(1103515245).wrapping_add(i * 7) & 0xFF) as u8)
                .collect();
            let mut r = BitReader::new(&bytes);
            let _ = decode_kill_array(&mut r, 64);
        }
    }
}
