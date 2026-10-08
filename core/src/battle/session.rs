//! 会话状态：`battle_proxy::battle::session`。
//!
//! 复刻样本 `src/battle/session.rs`。一个**会话 = 一台跑游戏的设备的一条转发链路**
//! （`session_model = one_port_one_player`：一个源端口一个会话）。每个会话拥有
//! 完全独立的解析状态，因为：
//!
//! * 不同玩家的通道号/ GUID 空间是**各自客户端自己的**；
//! * 一个雷达实例可能同时服务多台设备（样本 `active_sessions` / `total_sessions`）；
//! * 某台设备的解析失败不能污染另一台。
//!
//! 会话里还负责"我是谁"的判定（`authoritative_current_local_char`）：本地玩家是
//! **客户端自己控制的那个 pawn**，通常由最先出现且持续发出 indexed 位移的角色确定。

use std::net::SocketAddr;

use super::codec::container_collector::ContainerCollector;
use super::codec::killchain::KillChain;
use super::combat::{CombatAnalyzer, WeaponRegistry};
use super::loot_catalog::LootCatalog;
use super::udpxin_entity::EntityTable;
use super::udpxin_identity::IdentityTable;
use super::udpxin_live::{LivenessTracker, MoveSource};

/// 会话状态。
#[derive(Debug)]
pub struct SessionState {
    pub id: u64,
    /// 客户端（跑游戏的设备）的源地址。
    pub peer: SocketAddr,
    /// 服务端地址（游戏服务器），用于区分上下行。
    pub upstream: Option<SocketAddr>,
    pub opened_ms: u64,
    pub last_activity_ms: u64,

    pub entities: EntityTable,
    pub identities: IdentityTable,
    pub liveness: LivenessTracker,
    pub containers: ContainerCollector,
    pub kills: KillChain,
    pub combat: CombatAnalyzer,

    /// 本地玩家（本客户端自己控制）的角色通道。
    pub local_character_channel: Option<u32>,
    /// 本地玩家最近一次 indexed 位移时间：用于和"卡住了"区分。
    pub last_local_move_ms: u64,
    /// 本会话识别出的地图名（由世界/POI 属性或房间信息给出）。
    pub map_name: Option<String>,
    /// 该会话的解码统计。
    pub stats: SessionStats,

    /// 本会话是否曾成功解析过属性（用来区分"加密了"和"没流量"）。
    pub decoded_any: bool,
}

/// 单会话解码统计。
#[derive(Debug, Default, Clone)]
pub struct SessionStats {
    pub packets_seen: u64,
    pub packets_gated: u64,
    pub packets_rejected: u64,
    pub bunches: u64,
    pub property_blocks: u64,
    pub moves: u64,
    pub kills: u64,
    pub fires: u64,
    pub containers_observed: u64,
    pub transport_plain: u64,
    pub transport_crypto: u64,
}

impl SessionState {
    pub fn new(id: u64, peer: SocketAddr, opened_ms: u64, loot_parsing: bool) -> Self {
        Self {
            id,
            peer,
            upstream: None,
            opened_ms,
            last_activity_ms: opened_ms,
            entities: EntityTable::new(),
            identities: IdentityTable::new(),
            liveness: LivenessTracker::new(),
            containers: ContainerCollector::new(loot_parsing),
            kills: KillChain::new(64),
            combat: CombatAnalyzer::new(16),
            local_character_channel: None,
            last_local_move_ms: 0,
            map_name: None,
            stats: SessionStats::default(),
            decoded_any: false,
        }
    }

    pub fn touch(&mut self, now_ms: u64) {
        self.last_activity_ms = now_ms;
    }

    pub fn is_idle(&self, now_ms: u64, ttl_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_activity_ms) > ttl_ms
    }

    /// 记录一次角色位移；同时维护"本地玩家是谁"的判定。
    pub fn note_move(&mut self, channel: u32, source: MoveSource, now_ms: u64) -> bool {
        self.stats.moves += 1;
        let revived = self.liveness.note_move(channel, source, now_ms);

        // 本地玩家判定：第一次出现 indexed 位移的角色即认为是"我"，
        // 之后不再更改（除非该通道消失）。
        if matches!(source, MoveSource::Indexed) {
            match self.local_character_channel {
                None => {
                    self.local_character_channel = Some(channel);
                    self.liveness.set_authoritative_local_channel(Some(channel));
                    tracing::info!(
                        target: "battle_proxy",
                        session = self.id,
                        channel,
                        "authoritative_current_local_char"
                    );
                }
                Some(c) if c == channel => {
                    self.last_local_move_ms = now_ms;
                }
                Some(_) => {}
            }
        }
        revived
    }

    /// 本地玩家是否已经被自己控制着持续移动（判"我死了在观战"）。
    pub fn local_is_capable(&self, now_ms: u64) -> bool {
        self.local_character_channel.is_some()
            && now_ms.saturating_sub(self.last_local_move_ms) < 3_000
    }

    /// 本地玩家 uuid（雷达用来居中显示）。
    pub fn local_uuid(&self) -> Option<String> {
        let ch = self.local_character_channel?;
        self.identities
            .resolve(self.entities.get(ch)?)
            .map(|p| p.uuid.clone())
            .or_else(|| self.entities.get(ch)?.guid.map(|g| format!("G{g:016X}")))
    }

    /// 刷新身份/武器索引（每帧末尾调用）。
    pub fn finalize_frame(&mut self, now_ms: u64) {
        self.identities.refresh_from_entities(&self.entities, now_ms);
        self.combat.weapons.refresh(&self.entities, now_ms);
        self.containers.sync_from_entities(&self.entities, now_ms);
    }

    /// 周期清理（1 秒一次足够）。
    pub fn reap(&mut self, now_ms: u64) {
        self.entities.reap_stale(now_ms, 20_000);
        self.identities.reap_stale(now_ms, 90_000);
        self.liveness.reap_stale(now_ms, 120_000);
        self.combat.weapons.reap(now_ms, 60_000);
        self.containers.reap_stale(now_ms, 300_000);
        // 先算再写：避免同时可变/不可变借用 self。
        let local = self.local_uuid();
        self.identities.set_local_uuid(local);
    }

    /// 会话遥测（`/api/status` 里按会话列出）。
    pub fn telemetry(&self) -> serde_json::Value {
        serde_json::json!({
            "session": self.id,
            "peer": self.peer.to_string(),
            "opened_at_ms": self.opened_ms,
            "last_activity_ms": self.last_activity_ms,
            "entities": self.entities.len(),
            "identities": self.identities.len(),
            "alive_downed_dead": self.liveness.summary(),
            "containers": self.containers.len(),
            "kills": self.kills.total,
            "local_channel": self.local_character_channel,
            "map": self.map_name,
            "stats": {
                "packets_seen": self.stats.packets_seen,
                "packets_gated": self.stats.packets_gated,
                "packets_rejected": self.stats.packets_rejected,
                "bunches": self.stats.bunches,
                "property_blocks": self.stats.property_blocks,
                "moves": self.stats.moves,
                "kills": self.stats.kills,
                "fires": self.stats.fires,
            },
            "decoded_any": self.decoded_any,
        })
    }
}

/// 会话注册表：id → SessionState。用 `DashMap` + 每会话互斥，
/// 保证不同会话完全并行（样本 `active_sessions` 可达多台设备）。
#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: dashmap::DashMap<u64, parking_lot::Mutex<SessionState>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_create(
        &self,
        id: u64,
        peer: SocketAddr,
        now_ms: u64,
        loot_parsing: bool,
        ) -> dashmap::mapref::one::Ref<'_, u64, parking_lot::Mutex<SessionState>> {
        if !self.sessions.contains_key(&id) {
            self.sessions
                .insert(id, parking_lot::Mutex::new(SessionState::new(id, peer, now_ms, loot_parsing)));
        }
        self.sessions.get(&id).expect("just inserted")
    }

    pub fn with<R>(&self, id: u64, f: impl FnOnce(&mut SessionState) -> R) -> Option<R> {
        self.sessions.get(&id).map(|e| f(&mut e.lock()))
    }

    pub fn remove(&self, id: u64) -> Option<SessionState> {
        self.sessions.remove(&id).map(|(_, v)| v.into_inner())
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn ids(&self) -> Vec<u64> {
        self.sessions.iter().map(|e| *e.key()).collect()
    }

    /// 遍历全部会话（只读）。用 `FnMut` 便于闭包累积返回值。
    pub fn for_each<R>(&self, mut f: impl FnMut(u64, &SessionState) -> R) -> Vec<R> {
        self.sessions.iter().map(|e| f(*e.key(), &e.value().lock())).collect()
    }

    /// 遍历全部会话（可写）。
    pub fn for_each_mut<R>(&self, mut f: impl FnMut(u64, &mut SessionState) -> R) -> Vec<R> {
        self.sessions.iter().map(|e| f(*e.key(), &mut e.value().lock())).collect()
    }

    /// 清理空闲会话，返回被清理的 id。
    pub fn reap_idle(&self, now_ms: u64, ttl_ms: u64) -> Vec<u64> {
        let stale: Vec<u64> = self
            .sessions
            .iter()
            .filter(|e| e.value().lock().is_idle(now_ms, ttl_ms))
            .map(|e| *e.key())
            .collect();
        for id in &stale {
            self.sessions.remove(id);
        }
        stale
    }

    /// 合并所有会话的可绘制实体（雷达主视图；多设备时取并集）。
    pub fn merged_drawables(&self, now_ms: u64, ttl_ms: u64) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        for e in self.sessions.iter() {
            let s = e.value().lock();
            for rec in s.entities.drawable() {
                if rec.is_stale(now_ms, ttl_ms) {
                    continue;
                }
                let uuid = s
                    .identities
                    .resolve(rec)
                    .map(|p| p.uuid.clone())
                    .or_else(|| rec.guid.map(|g| format!("G{g:016X}")))
                    .unwrap_or_else(|| format!("Player-{}", rec.channel));
                out.push((*e.key(), uuid));
            }
        }
        out
    }
}

/// 会话空闲上限（30 秒没有任何字节）。
pub const SESSION_IDLE_TTL_MS: u64 = 30_000;

/// 加载目录：把内嵌 `channel_map.json` 同时灌进静态通道表与名字表。
pub fn bootstrap_catalogs(
    entities: &mut EntityTable,
    handles: &mut super::codec::character::HandleTable,
    channel_map_json: Option<&str>,
    loot: &mut LootCatalog,
    loot_json: Option<&str>,
) -> anyhow::Result<(usize, usize, usize)> {
    let mut channels = 0;
    let mut props = 0;
    let mut items = 0;
    if let Some(json) = channel_map_json {
        channels = entities.load_static_channel_map(json)?;
        props = handles.load_channel_map_json(json)?;
    }
    if let Some(json) = loot_json {
        items = loot.load_id_table_json(json)?;
    }
    Ok((channels, props, items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::udpxin_entity::Vec3;
    use crate::battle::udpxin_identity::CharacterProps;

    fn peer() -> SocketAddr {
        "192.168.1.9:40000".parse().unwrap()
    }

    #[test]
    fn first_indexed_move_claims_local_character() {
        let mut s = SessionState::new(1, peer(), 0, true);
        assert!(s.local_character_channel.is_none());
        s.note_move(7, MoveSource::Stale, 100);
        assert!(s.local_character_channel.is_none(), "stale move must not claim local char");
        s.note_move(7, MoveSource::Indexed, 200);
        assert_eq!(s.local_character_channel, Some(7));
        assert_eq!(s.local_uuid(), None, "no identity/guid yet");
    }

    #[test]
    fn stale_moves_do_not_refresh_local_liveness() {
        let mut s = SessionState::new(1, peer(), 0, true);
        s.note_move(3, MoveSource::Indexed, 1_000);
        assert!(s.local_is_capable(2_000));
        assert!(!s.local_is_capable(10_000), "must expire after 3s without indexed movement");
    }

    #[test]
    fn local_uuid_resolves_from_identity_when_guid_known() {
        let mut s = SessionState::new(1, peer(), 0, true);
        s.entities.open_channel(3, Some("BP_DFMCharacter_C"), Some(0x42), 0);
        s.identities.apply_character(3, &CharacterProps { guid: Some(0x42), ..Default::default() }, 0);
        s.note_move(3, MoveSource::Indexed, 100);
        // 契约：uuid = `format!("G{g:016X}")`（见 udpxin_identity::uuid_for 及其
        // `uuid_prefers_guid_then_uin_then_channel` 测试），即 G + 16 位十六进制。
        // 0x42 → "0000000000000042"（14 个 0），不是 13 个 0。
        assert_eq!(s.local_uuid(), Some("G0000000000000042".to_string()));
    }

    #[test]
    fn finalize_frame_refreshes_indexes() {
        let mut s = SessionState::new(1, peer(), 0, true);
        {
            let r = s.entities.open_channel(3, Some("BP_DFMCharacter_C"), Some(1), 0);
            r.position = Vec3 { x: 100.0, y: 100.0, z: 0.0 };
        }
        s.entities.open_channel(9, Some("BP_WeaponThrowC4_C"), Some(2), 0);
        {
            let r = s.entities.open_channel(20, Some("DFMContainerDataCollector"), None, 0);
            r.position = Vec3 { x: 50.0, y: 50.0, z: 0.0 };
        }
        s.finalize_frame(10);
        assert_eq!(s.identities.len(), 1);
        assert_eq!(s.combat.weapons.len(), 1);
        assert_eq!(s.containers.len(), 1);
    }

    #[test]
    fn reaping_clears_stale_entities() {
        let mut s = SessionState::new(1, peer(), 0, true);
        s.entities.open_channel(3, Some("BP_DFMCharacter_C"), None, 0);
        s.reap(1_000_000);
        assert_eq!(s.entities.len(), 0);
    }

    #[test]
    fn registry_creates_and_reaps_sessions() {
        let r = SessionRegistry::new();
        {
            let s = r.get_or_create(1, peer(), 1_000, true);
            let mut g = s.lock();
            g.touch(1_000);
        }
        assert_eq!(r.len(), 1);
        assert_eq!(r.reap_idle(1_000 + SESSION_IDLE_TTL_MS + 1, SESSION_IDLE_TTL_MS), vec![1]);
        assert!(r.is_empty());
    }

    #[test]
    fn registry_with_mutates_state() {
        let r = SessionRegistry::new();
        r.get_or_create(5, peer(), 0, true);
        let n = r.with(5, |s| {
            s.entities.open_channel(3, Some("BP_DFMCharacter_C"), None, 0);
            s.entities.len()
        });
        assert_eq!(n, Some(1));
        assert_eq!(r.with(999, |s| s.id), None);
    }

    #[test]
    fn bootstrap_loads_channel_map_and_loot() {
        let cm = r#"{"channel_map":[{"slot":3,"ch_index":3,"class":"BP_DFMCharacter_C"}],
                     "handles":{"BP_DFMCharacter_C":{"94":"MyGUIDValue"}}}"#;
        let loot = r#"{"10010000903":"SOL_DT_Basic_AR_low_M16A4_14"}"#;
        let mut e = EntityTable::new();
        let mut h = super::super::codec::character::HandleTable::new();
        let mut l = LootCatalog::new();
        let (c, p, i) = bootstrap_catalogs(&mut e, &mut h, Some(cm), &mut l, Some(loot)).unwrap();
        assert_eq!((c, p, i), (1, 1, 1));
        assert_eq!(h.class_count(), 1);
        assert!(l.name_of(10010000903).is_some());
    }
}
