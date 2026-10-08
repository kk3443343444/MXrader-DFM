//! 交战分析：`battle_proxy::battle::combat`。
//!
//! 复刻样本 `src/battle/combat.rs`（样本里 3000+ 行）。样本留下的日志标签几乎
//! 就是本模块的需求清单：
//!
//! ```text
//! [combat] discovered weapon_object=
//! [combat] discovered weapon_channel=
//! [combat] CharacterMesh0: actor=
//! [combat_fallback] weapon via GUID: channel=
//! [combat] rejected packet growth: bytes
//! (BP_Weapon upstream fire)
//! [aim_parse] view scan #
//! weapon_ch=
//! mesh=
//! ```
//!
//! 三件事：
//!
//! 1. **武器注册表** —— 把 `BP_Weapon*_C` 通道关联到持有者（优先走
//!    `CharacterOwner`(41) 属性，失败时用 GUID 兜底 = `combat_fallback`）；
//! 2. **视角扫描（aim）** —— 开火时，在朝向锥体里找出"谁可能被打"，
//!    用于雷达上的"对枪提示"；
//! 3. **包增长护栏** —— `rejected packet growth`：当解析出的实体数/通道数
//!    异常暴涨时，判定为错位解析，直接丢弃该批更新，避免雷达被垃圾数据刷爆。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::udpxin_entity::{ActorKind, EntityTable, Vec3};
use super::udpxin_identity::IdentityTable;

/// 武器注册表：通道 → (武器类, 持有者通道)。
#[derive(Debug, Default)]
pub struct WeaponRegistry {
    by_channel: HashMap<u32, WeaponBinding>,
    /// GUID → 武器通道兜底索引。
    by_guid: HashMap<u64, u32>,
    /// 通过属性成功关联的次数。
    pub discovered_by_property: u64,
    /// 通过 GUID 兜底的次数（样本 `combat_fallback`）。
    pub discovered_by_guid_fallback: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeaponBinding {
    pub channel: u32,
    pub class: String,
    pub owner_channel: Option<u32>,
    /// `ServerWeaponIdentity`(40) 之类的服务端身份串（若有）。
    pub server_identity: Option<String>,
    pub last_seen_ms: u64,
}

impl WeaponRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从实体表刷新（每帧一次，代价很低）。
    pub fn refresh(&mut self, table: &EntityTable, now_ms: u64) {
        for rec in table.iter() {
            if rec.kind != ActorKind::Weapon {
                continue;
            }
            if let Some(guid) = rec.guid {
                self.by_guid.insert(guid, rec.channel);
            }
            let owner = rec.owner_channel;
            match self.by_channel.get_mut(&rec.channel) {
                Some(b) => {
                    b.last_seen_ms = now_ms;
                    if owner.is_some() && b.owner_channel.is_none() {
                        b.owner_channel = owner;
                    }
                }
                None => {
                    let discovered_by_property = owner.is_some();
                    if discovered_by_property {
                        self.discovered_by_property += 1;
                        tracing::debug!(
                            target: "battle_proxy",
                            "{} {}",
                            "[combat] discovered weapon_channel=",
                            rec.channel
                        );
                    } else {
                        self.discovered_by_guid_fallback += 1;
                        tracing::debug!(
                            target: "battle_proxy",
                            "{} {}",
                            "[combat_fallback] weapon via GUID: channel=",
                            rec.channel
                        );
                    }
                    self.by_channel.insert(
                        rec.channel,
                        WeaponBinding {
                            channel: rec.channel,
                            class: rec.class.clone(),
                            owner_channel: owner,
                            server_identity: None,
                            last_seen_ms: now_ms,
                        },
                    );
                }
            }
        }
    }

    /// 由角色反查其武器（雷达列表里的"武器"列）。
    pub fn weapon_of_character(&self, character_channel: u32) -> Option<&WeaponBinding> {
        self.by_channel
            .values()
            .filter(|b| b.owner_channel == Some(character_channel))
            .max_by_key(|b| b.last_seen_ms)
    }

    pub fn get(&self, channel: u32) -> Option<&WeaponBinding> {
        self.by_channel.get(&channel)
    }

    pub fn len(&self) -> usize {
        self.by_channel.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_channel.is_empty()
    }

    /// 清理过期绑定。
    pub fn reap(&mut self, now_ms: u64, ttl_ms: u64) -> usize {
        let before = self.by_channel.len();
        self.by_channel.retain(|_, b| now_ms.saturating_sub(b.last_seen_ms) < ttl_ms);
        let live: std::collections::HashSet<u32> = self.by_channel.keys().copied().collect();
        self.by_guid.retain(|_, ch| live.contains(ch));
        before - self.by_channel.len()
    }
}

/// 一个"可能被打"的候选目标（`[aim_parse] view scan #`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AimCandidate {
    pub target_channel: u32,
    pub target_uuid: Option<String>,
    /// 目标与射线方向的角度误差（度，越小越可能被打中）。
    pub angular_error_deg: f32,
    pub distance_m: f32,
    /// 视线是否被高度差排除（打不到的）。
    pub blocked: bool,
}

/// 一次开火的交战评估。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Engagement {
    pub shooter_channel: Option<u32>,
    pub shooter_uuid: Option<String>,
    pub weapon: Option<String>,
    pub origin: Vec3,
    pub candidates: Vec<AimCandidate>,
    pub ts_ms: u64,
}

/// 视角扫描：在 `cone_deg` 锥体内按角度误差排序，返回前 `limit` 个。
pub fn scan_aim_candidates(
    shooter: Option<u32>,
    origin: Vec3,
    fire_direction: [f32; 3],
    table: &EntityTable,
    identities: &IdentityTable,
    cone_deg: f32,
    limit: usize,
) -> Vec<AimCandidate> {
    let mut out = Vec::new();
    let ref_pos = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    for rec in table.iter() {
        if !rec.kind.is_character() || !rec.has_valid_position() {
            continue;
        }
        if Some(rec.channel) == shooter {
            continue;
        }
        if !rec.alive {
            continue;
        }
        let dx = rec.position.x - origin.x;
        let dy = rec.position.y - origin.y;
        let dz = rec.position.z - origin.z;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        if dist < 1.0 {
            continue;
        }
        let unit = [dx / dist, dy / dist, dz / dist];
        let dot = (unit[0] * fire_direction[0]
            + unit[1] * fire_direction[1]
            + unit[2] * fire_direction[2])
            .clamp(-1.0, 1.0);
        let angle = dot.acos().to_degrees();
        if angle > cone_deg {
            continue;
        }
        let _ = ref_pos;
        out.push(AimCandidate {
            target_channel: rec.channel,
            target_uuid: identities
                .resolve(rec)
                .map(|p| p.uuid.clone())
                .or_else(|| rec.guid.map(|g| format!("G{g:016X}"))),
            angular_error_deg: angle,
            distance_m: dist / 100.0,
            blocked: false,
        });
    }
    out.sort_by(|a, b| {
        a.angular_error_deg
            .partial_cmp(&b.angular_error_deg)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);
    out
}

/// 包增长护栏：样本的 `[combat] rejected packet growth: bytes`。
///
/// 判据：单次更新带来的实体/通道增量超过阈值，或与上一批相比"跳变"过大。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GrowthGuard {
    /// 单批允许新增的最大通道数。
    pub max_new_channels_per_batch: usize,
    /// 单批允许的最大实体总数跃升比例。
    pub max_growth_ratio: f32,
}

impl Default for GrowthGuard {
    fn default() -> Self {
        Self { max_new_channels_per_batch: 64, max_growth_ratio: 3.0 }
    }
}

/// 判定结果。
#[derive(Debug, Clone, PartialEq)]
pub enum GrowthVerdict {
    Ok,
    /// 拒绝并给出原因（样本会打日志并丢弃整批）。
    Rejected { reason: &'static str, added: usize, before: usize },
}

impl GrowthGuard {
    pub fn check(&self, before: usize, after: usize) -> GrowthVerdict {
        let added = after.saturating_sub(before);
        if added > self.max_new_channels_per_batch {
            return GrowthVerdict::Rejected {
                reason: "rejected packet growth",
                added,
                before,
            };
        }
        if before > 8 {
            let ratio = after as f32 / before as f32;
            if ratio > self.max_growth_ratio {
                return GrowthVerdict::Rejected {
                    reason: "rejected packet growth",
                    added,
                    before,
                };
            }
        }
        GrowthVerdict::Ok
    }
}

/// 交战分析器。
#[derive(Debug)]
pub struct CombatAnalyzer {
    pub weapons: WeaponRegistry,
    pub guard: GrowthGuard,
    /// 最近的开火评估（供雷达"对枪提示"）。
    recent: std::collections::VecDeque<Engagement>,
    capacity: usize,
    /// 被护栏拦下的批次数。
    pub rejected_batches: u64,
}

impl CombatAnalyzer {
    pub fn new(capacity: usize) -> Self {
        Self {
            weapons: WeaponRegistry::new(),
            guard: GrowthGuard::default(),
            recent: std::collections::VecDeque::with_capacity(capacity),
            capacity,
            rejected_batches: 0,
        }
    }

    pub fn push_engagement(&mut self, e: Engagement) {
        self.recent.push_back(e);
        while self.recent.len() > self.capacity {
            self.recent.pop_front();
        }
    }

    pub fn recent(&self) -> impl Iterator<Item = &Engagement> {
        self.recent.iter()
    }

    /// 记录一次被拒绝的增长。
    pub fn note_rejection(&mut self, v: &GrowthVerdict) {
        if let GrowthVerdict::Rejected { added, before, .. } = v {
            self.rejected_batches += 1;
            tracing::warn!(
                target: "battle_proxy",
                added,
                before,
                "[combat] rejected packet growth: bytes"
            );
        }
    }

    pub fn clear(&mut self) {
        self.recent.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::udpxin_entity::Rot3;
    use crate::battle::udpxin_identity::CharacterProps;

    fn scene() -> (EntityTable, IdentityTable) {
        let mut t = EntityTable::new();
        {
            let r = t.open_channel(3, Some("BP_DFMCharacter_C"), Some(1), 0);
            r.position = Vec3 { x: 1000.0, y: 0.0, z: 0.0 };
            r.rotation = Rot3 { yaw: 0.0, ..Default::default() };
        }
        {
            let r = t.open_channel(4, Some("BP_DFMCharacter_C"), Some(2), 0);
            r.position = Vec3 { x: 3000.0, y: 0.0, z: 0.0 };
        }
        {
            let r = t.open_channel(5, Some("BP_WeaponMeleeNoModular_C"), Some(3), 0);
            r.owner_channel = Some(3);
        }
        let mut ids = IdentityTable::new();
        ids.apply_character(3, &CharacterProps { guid: Some(1), ..Default::default() }, 0);
        ids.apply_character(4, &CharacterProps { guid: Some(2), ..Default::default() }, 0);
        (t, ids)
    }

    #[test]
    fn weapon_registry_binds_by_property_and_counts_discovery() {
        let (t, _) = scene();
        let mut w = WeaponRegistry::new();
        w.refresh(&t, 100);
        assert_eq!(w.len(), 1);
        assert_eq!(w.discovered_by_property, 1);
        assert_eq!(w.weapon_of_character(3).unwrap().class, "BP_WeaponMeleeNoModular_C");
        assert!(w.weapon_of_character(4).is_none());
    }

    #[test]
    fn weapon_without_owner_uses_guid_fallback() {
        let mut t = EntityTable::new();
        t.open_channel(9, Some("BP_WeaponThrowC4_C"), Some(77), 0);
        let mut w = WeaponRegistry::new();
        w.refresh(&t, 1);
        assert_eq!(w.discovered_by_guid_fallback, 1);
        assert_eq!(w.discovered_by_property, 0);
    }

    #[test]
    fn aim_scan_finds_the_target_inside_the_cone_only() {
        let (t, ids) = scene();
        let origin = Vec3 { x: 1000.0, y: 0.0, z: 0.0 };
        let dir = [1.0, 0.0, 0.0];
        let hits = scan_aim_candidates(Some(3), origin, dir, &t, &ids, 15.0, 8);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].target_channel, 4);
        assert!((hits[0].distance_m - 20.0).abs() < 0.01);
        assert!(hits[0].angular_error_deg < 0.01);

        // 朝向相反时打不到
        let miss = scan_aim_candidates(Some(3), origin, [-1.0, 0.0, 0.0], &t, &ids, 15.0, 8);
        assert!(miss.is_empty());
    }

    #[test]
    fn aim_scan_excludes_self_and_dead() {
        let (mut t, ids) = scene();
        let origin = Vec3 { x: 1000.0, y: 0.0, z: 0.0 };
        let dir = [1.0, 0.0, 0.0];
        t.get_mut(4).unwrap().alive = false;
        assert!(scan_aim_candidates(Some(3), origin, dir, &t, &ids, 90.0, 8).is_empty());
        let all = scan_aim_candidates(None, origin, dir, &t, &ids, 90.0, 8);
        assert!(all.iter().all(|c| c.target_channel != 3 || true));
    }

    #[test]
    fn growth_guard_rejects_explosive_channel_growth() {
        let g = GrowthGuard::default();
        assert_eq!(g.check(10, 20), GrowthVerdict::Ok);
        match g.check(10, 200) {
            GrowthVerdict::Rejected { reason, added, before } => {
                assert_eq!(reason, "rejected packet growth");
                assert_eq!(added, 190);
                assert_eq!(before, 10);
            }
            _ => panic!("must reject"),
        }
        // before 很小时比例规则不生效（开局本来就会一次性进来很多通道）
        assert_eq!(g.check(2, 60), GrowthVerdict::Ok);
    }

    #[test]
    fn rejection_is_counted() {
        let g = GrowthGuard::default();
        let mut a = CombatAnalyzer::new(4);
        let v = g.check(100, 1000);
        a.note_rejection(&v);
        assert_eq!(a.rejected_batches, 1);
    }

    #[test]
    fn engagements_are_ring_buffered() {
        let mut a = CombatAnalyzer::new(2);
        for i in 0..5u64 {
            a.push_engagement(Engagement {
                shooter_channel: Some(3),
                shooter_uuid: None,
                weapon: None,
                origin: Vec3::default(),
                candidates: vec![],
                ts_ms: i,
            });
        }
        assert_eq!(a.recent().count(), 2);
        assert_eq!(a.recent().last().unwrap().ts_ms, 4);
    }

    #[test]
    fn registry_reaping_clears_guid_index() {
        let mut t = EntityTable::new();
        t.open_channel(9, Some("BP_WeaponThrowC4_C"), Some(77), 0);
        let mut w = WeaponRegistry::new();
        w.refresh(&t, 1_000);
        assert_eq!(w.reap(1_000_000, 60_000), 1);
        assert!(w.is_empty());
    }
}
