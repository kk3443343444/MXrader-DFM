//! 生死/血量状态机：`battle_proxy::battle::udpxin_live`。
//!
//! 复刻样本 `src/battle/udpxin_live.rs`。样本在这一层留了一串很有意思的标识，
//! 它们就是本模块要实现的规则：
//!
//! ```text
//! authoritative_indexed_movement_after_recoverable_death
//! indexed_movement_proves_revival_after_dead
//! authoritative_current_local_char
//! move_after_recoverable_death
//! current_local_char
//! current_local_char_move_proves_revival_after_dead
//! bIsDeadCanOPtimise / DeadInfo / bIsBeingRescueReplicate
//! ```
//!
//! 三角洲的"倒地/救援"把生死拆成四态，而**死亡盒不是角色**（`bIsDeadBox` 在
//! PlayerState 上）。雷达必须区分：
//!
//! | 状态 | 判据 | 雷达表现 |
//! |---|---|---|
//! | Alive | 收到角色位移 | 正常点 + 朝向箭头 |
//! | Downed | `bIsBeingRescueReplicate`(133) 或 `DeathWaitRescueTime_Custom`(153) 有值 | 亮点 + 血条闪 |
//! | Dead | `DeadInfo` 属性非空 / `bDeadCanOPtimise`(93) | 灰点 |
//! | DeadBox | `BP_DFMPlayerState_C::bIsDeadBox`(129) | 方块 |
//!
//! 关键规则（样本命名的两条）：**"可恢复死亡"之后如果又收到 *indexed* 位移，
//! 判定为复活**。所谓 indexed 位移，指带 `LatestMovePackage`(126) 或
//! `MoveHandle`(60) 的位移包——它证明客户端仍在权威地驱动这个 pawn，
//! 而不是残留的陈旧复制。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::udpxin_entity::{ActorRecord, EntityTable};

/// 生死四态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveState {
    Alive,
    /// 倒地待救（可恢复死亡）。
    Downed,
    Dead,
    /// 已化为死亡盒（角色本身通常已 Destroy）。
    DeadBox,
}

impl LiveState {
    pub fn as_str(self) -> &'static str {
        match self {
            LiveState::Alive => "alive",
            LiveState::Downed => "downed",
            LiveState::Dead => "dead",
            LiveState::DeadBox => "dead_box",
        }
    }
    pub fn is_drawable(self) -> bool {
        !matches!(self, LiveState::DeadBox)
    }
}

/// 一条角色的活体状态跟踪。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveRecord {
    pub channel: u32,
    pub state: LiveState,
    /// 进入当前状态的时间。
    pub since_ms: u64,
    /// 最近一次"权威位移"的时间与是否 indexed。
    pub last_move_ms: u64,
    pub last_move_indexed: bool,
    /// 复活次数（样本用它做"存活率"诊断）。
    pub revivals: u32,
    pub deaths: u32,
    /// `DeathWaitRescueTime_Custom`(153) 的单位化秒数，>0 表示倒地计时。
    pub rescue_window_s: Option<f32>,
    /// 该角色是否可能是本地玩家（来自 `authoritative_current_local_char`）。
    pub local_char: bool,
    /// 属性层给的原始 live status（`CurrentCharacterLiveStatus`(127)）。
    pub raw_live_status: Option<u8>,
}

impl LiveRecord {
    fn new(channel: u32, now_ms: u64) -> Self {
        Self {
            channel,
            state: LiveState::Alive,
            since_ms: now_ms,
            last_move_ms: 0,
            last_move_indexed: false,
            revivals: 0,
            deaths: 0,
            rescue_window_s: None,
            local_char: false,
            raw_live_status: None,
        }
    }
}

/// 状态机。
#[derive(Debug, Default)]
pub struct LivenessTracker {
    records: HashMap<u32, LiveRecord>,
    /// 本地玩家当前角色通道（`authoritative_current_local_char`）。
    authoritative_local_channel: Option<u32>,
}

/// 位移事件的来源，决定它是否"证明复活"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveSource {
    /// 陈旧复制（可能是缓存/插值）。
    Stale,
    /// 索引化位移（带 `LatestMovePackage`/`MoveHandle`）。
    Indexed,
    /// 由开火/换弹等 RPC 间接证明客户端仍在跑（`battle_fire_cli` 路径）。
    RpcActivity,
}

impl LivenessTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn authoritative_local_channel(&self) -> Option<u32> {
        self.authoritative_local_channel
    }

    pub fn set_authoritative_local_channel(&mut self, ch: Option<u32>) {
        self.authoritative_local_channel = ch;
        for r in self.records.values_mut() {
            r.local_char = Some(r.channel) == ch;
        }
    }

    pub fn get(&self, channel: u32) -> Option<&LiveRecord> {
        self.records.get(&channel)
    }

    pub fn state_of(&self, channel: u32) -> LiveState {
        self.records.get(&channel).map(|r| r.state).unwrap_or(LiveState::Alive)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 取记录；新建（或复用）时按 `authoritative_local_channel` 播下 `local_char`。
    ///
    /// `authoritative_current_local_char` 是权威标记：本地角色常常在
    /// `set_authoritative_local_channel` 之后才出现位移/死亡包，而 `entry(...)`
    /// 新建的记录默认 `local_char = false`，于是 `reap_stale` 会把自己的角色当僵尸
    /// 清掉（雷达上"自己"消失）。所以创建/取用两条路径都要跟上。
    fn record(&mut self, channel: u32, now_ms: u64) -> &mut LiveRecord {
        let local = Some(channel) == self.authoritative_local_channel;
        let rec = self
            .records
            .entry(channel)
            .or_insert_with(|| LiveRecord::new(channel, now_ms));
        rec.local_char = local;
        rec
    }

    /// 报告一次位移。返回是否发生了"复活"判定。
    pub fn note_move(&mut self, channel: u32, source: MoveSource, now_ms: u64) -> bool {
        let rec = self.record(channel, now_ms);
        rec.last_move_ms = now_ms;
        rec.last_move_indexed = matches!(source, MoveSource::Indexed);

        if matches!(source, MoveSource::Stale) {
            // 陈旧复制不能改变状态机 —— 否则死亡盒会"自己走起来"。
            return false;
        }

        let revived = matches!(rec.state, LiveState::Downed | LiveState::Dead);
        if revived {
            // indexed_movement_proves_revival_after_dead
            rec.state = LiveState::Alive;
            rec.since_ms = now_ms;
            rec.rescue_window_s = None;
            rec.revivals += 1;
            tracing::debug!(
                target: "battle_proxy",
                channel,
                "indexed_movement_proves_revival_after_dead"
            );
        }
        revived
    }

    /// 报告一次"可恢复死亡"（倒地）。
    pub fn note_recoverable_death(
        &mut self,
        channel: u32,
        rescue_window_s: Option<f32>,
        now_ms: u64,
    ) {
        let rec = self.record(channel, now_ms);
        if rec.state != LiveState::Downed {
            rec.since_ms = now_ms;
        }
        rec.state = LiveState::Downed;
        rec.rescue_window_s = rescue_window_s;
        rec.deaths += 1;
        tracing::debug!(
            target: "battle_proxy",
            channel,
            "authoritative_indexed_movement_after_recoverable_death"
        );
    }

    /// 报告一次确定死亡（`DeadInfo` 非空 / `bDeadCanOPtimise`）。
    pub fn note_death(&mut self, channel: u32, now_ms: u64) {
        let rec = self.record(channel, now_ms);
        rec.state = LiveState::Dead;
        rec.since_ms = now_ms;
        rec.rescue_window_s = None;
    }

    /// 报告角色化为死亡盒。
    pub fn note_dead_box(&mut self, channel: u32, now_ms: u64) {
        let rec = self.record(channel, now_ms);
        rec.state = LiveState::DeadBox;
        rec.since_ms = now_ms;
    }

    /// 应用属性层的 live status。
    pub fn note_raw_live_status(&mut self, channel: u32, status: u8, now_ms: u64) {
        // 先把旧状态读出来（不要跨调用持有 entry 的借用）。
        let existed = self.records.contains_key(&channel);
        let rec = self.record(channel, now_ms);
        rec.raw_live_status = Some(status);
        let (rescue_window, was_alive) = (rec.rescue_window_s, existed && rec.state == LiveState::Alive);
        // 约定（r39）：0=活，1=倒地，2=死亡，3=死亡盒；其它值不动状态机。
        match status {
            1 => self.note_recoverable_death(channel, rescue_window, now_ms),
            2 => self.note_death(channel, now_ms),
            3 => self.note_dead_box(channel, now_ms),
            0 => {
                if let Some(rec) = self.records.get_mut(&channel) {
                    if !was_alive {
                        rec.state = LiveState::Alive;
                        rec.since_ms = now_ms;
                    }
                }
            }
            _ => {}
        }
    }

    /// 血量变化（`BP_PropertyReplicationCharacterHealth_C` 中继）。
    pub fn note_health(&self, rec: &mut ActorRecord, health: f32, max_health: f32) {
        rec.health = health.max(0.0);
        if max_health > 0.0 {
            rec.max_health = max_health;
        }
        rec.alive = rec.health > 0.0;
    }

    /// 把状态机结果写回实体表（雷达序列化前调用）。
    pub fn apply_to_entities(&self, table: &mut EntityTable) {
        // 先收集要改的 (channel, alive, dead, kind)，再去改表 —— 避免同时借用。
        let updates: Vec<(u32, bool, bool, Option<super::udpxin_entity::ActorKind>)> = self
            .records
            .values()
            .filter_map(|live| {
                if !table.contains(live.channel) {
                    return None;
                }
                let deadbox = live.state == LiveState::DeadBox;
                Some((
                    live.channel,
                    matches!(live.state, LiveState::Alive | LiveState::Downed),
                    matches!(live.state, LiveState::Dead | LiveState::DeadBox),
                    deadbox.then_some(super::udpxin_entity::ActorKind::Container),
                ))
            })
            .collect();
        for (channel, alive, dead, kind) in updates {
            if let Some(slot) = table.get_mut(channel) {
                slot.alive = alive;
                slot.dead = dead;
                if let Some(k) = kind {
                    slot.kind = k;
                }
            }
        }
    }

    /// 清理长期不活跃记录（默认 120 秒）。
    pub fn reap_stale(&mut self, now_ms: u64, ttl_ms: u64) -> usize {
        let before = self.records.len();
        let local = self.authoritative_local_channel;
        self.records.retain(|ch, r| {
            let idle = now_ms.saturating_sub(r.last_move_ms.max(r.since_ms));
            // 权威本地通道永不回收（`local_char` 再兜一层：即使记录是在
            // `set_authoritative_local_channel` 之前建的老记录）。
            Some(*ch) == local || r.local_char || idle < ttl_ms
        });
        before - self.records.len()
    }

    /// 供雷达展示的统计。
    pub fn summary(&self) -> (usize, usize, usize) {
        let alive = self.records.values().filter(|r| r.state == LiveState::Alive).count();
        let downed = self.records.values().filter(|r| r.state == LiveState::Downed).count();
        let dead = self
            .records
            .values()
            .filter(|r| matches!(r.state, LiveState::Dead | LiveState::DeadBox))
            .count();
        (alive, downed, dead)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_movement_revives_after_death() {
        let mut t = LivenessTracker::new();
        t.note_death(3, 100);
        assert_eq!(t.state_of(3), LiveState::Dead);
        let revived = t.note_move(3, MoveSource::Indexed, 200);
        assert!(revived);
        assert_eq!(t.state_of(3), LiveState::Alive);
        assert_eq!(t.get(3).unwrap().revivals, 1);
    }

    #[test]
    fn stale_movement_never_revives() {
        let mut t = LivenessTracker::new();
        t.note_death(3, 100);
        assert!(!t.note_move(3, MoveSource::Stale, 200));
        assert_eq!(t.state_of(3), LiveState::Dead);
    }

    #[test]
    fn downed_then_rpc_activity_revives() {
        let mut t = LivenessTracker::new();
        t.note_recoverable_death(5, Some(30.0), 1_000);
        assert_eq!(t.state_of(5), LiveState::Downed);
        assert_eq!(t.get(5).unwrap().rescue_window_s, Some(30.0));
        assert!(t.note_move(5, MoveSource::RpcActivity, 2_000));
        assert_eq!(t.state_of(5), LiveState::Alive);
        assert_eq!(t.get(5).unwrap().deaths, 1);
    }

    #[test]
    fn raw_live_status_drives_the_machine() {
        let mut t = LivenessTracker::new();
        t.note_raw_live_status(7, 1, 10);
        assert_eq!(t.state_of(7), LiveState::Downed);
        t.note_raw_live_status(7, 3, 20);
        assert_eq!(t.state_of(7), LiveState::DeadBox);
        t.note_raw_live_status(7, 0, 30);
        assert_eq!(t.state_of(7), LiveState::Alive);
    }

    #[test]
    fn dead_box_is_not_drawable() {
        let mut t = LivenessTracker::new();
        let mut table = EntityTable::new();
        table.open_channel(3, Some("BP_DFMCharacter_C"), Some(1), 0);
        t.note_dead_box(3, 100);
        t.apply_to_entities(&mut table);
        let rec = table.get(3).unwrap();
        assert!(rec.dead);
        assert!(!rec.kind.is_drawable() || rec.kind == super::super::udpxin_entity::ActorKind::Container);
    }

    #[test]
    fn local_char_is_never_reaped() {
        let mut t = LivenessTracker::new();
        t.set_authoritative_local_channel(Some(3));
        t.note_move(3, MoveSource::Indexed, 1_000);
        t.note_death(3, 1_000);
        assert_eq!(t.reap_stale(1_000_000, 1_000), 0);
    }

    #[test]
    fn summary_counts_states() {
        let mut t = LivenessTracker::new();
        t.note_move(1, MoveSource::Indexed, 1);
        t.note_recoverable_death(2, None, 1);
        t.note_death(3, 1);
        assert_eq!(t.summary(), (1, 1, 1));
    }
}
