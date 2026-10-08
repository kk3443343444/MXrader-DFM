//! 物资容器采集：`battle_proxy::battle::codec::container_collector`。
//!
//! 复刻样本 `src/battle/codec/container_collector.rs`。样本在这个模块上暴露了
//! 一个**关键事实**（也是雷达产品的诚实边界）：
//!
//! ```text
//! randomized_container_contents_not_transmitted
//! contents_status / collector_slots / runtime_loot_observed
//! ```
//!
//! 即：三角洲的容器内容物是**服务端随机生成、开箱前不复制给客户端**的。
//! 所以"物资雷达"能给的只有：
//!
//! 1. **容器位置与类型**（容器 actor 本身会被复制，所以位置一定拿得到）；
//! 2. **已经被任何人打开过的容器**的真实内容（打开后内容物随
//!    `DFMContainerDataCollector` 的属性复制下来）—— 这就是 `runtime_loot_observed`；
//! 3. 静态概率表/目录（`loot_catalog.rs`）。
//!
//! 任何声称"未开箱就能看内容物"的实现，要么是在骗用户，要么是在用上面第 2 条的
//! 时序差糊人。本模块如实建模。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::super::udpxin_entity::{ActorKind, EntityTable, Vec3};

/// 容器内容可见性状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentsStatus {
    /// 服务端尚未复制内容（绝大多数字段，随机生成）。
    Randomised,
    /// 已开箱，内容物已在复制流里。
    Observed,
    /// 容器为空（`runtime_loot_observed` 明确给出空数组）。
    Empty,
}

impl ContentsStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ContentsStatus::Randomised => "randomised_not_transmitted",
            ContentsStatus::Observed => "observed",
            ContentsStatus::Empty => "empty",
        }
    }
}

/// 一个采集槽（`collector_slots`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectorSlot {
    pub slot: u8,
    /// 物品 ID（游戏内的 `GameItem` ID，如 `10010000903`）。
    pub item_id: Option<u64>,
    /// 由 loot_catalog 解析出的名字。
    pub item_name: Option<String>,
    pub count: u32,
    /// 稀有度（由目录给出）。
    pub rarity: Option<u8>,
}

/// 一个被观察到的容器。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    pub channel: u32,
    pub class: String,
    pub guid: Option<u64>,
    pub position: Vec3,
    pub status: ContentsStatus,
    /// 已观察到的内容（仅在 `Observed` 时非空）。
    pub slots: Vec<CollectorSlot>,
    /// 谁打开的（如果能从交互 RPC 关联到）。
    pub opened_by: Option<String>,
    pub first_seen_ms: u64,
    pub last_update_ms: u64,
}

impl Container {
    pub fn is_lootable(&self) -> bool {
        self.status != ContentsStatus::Empty
    }
    /// 价值估算（由 loot_catalog 的单价表给出 sum）。
    pub fn estimated_value(&self, unit_price: impl Fn(u64) -> Option<u32>) -> u64 {
        self.slots
            .iter()
            .filter_map(|s| s.item_id.and_then(|id| unit_price(id)))
            .map(|p| p as u64)
            .sum()
    }
}

/// 容器采集器。
#[derive(Debug)]
pub struct ContainerCollector {
    containers: HashMap<u32, Container>,
    /// `loot_parsing_enabled` 关闭时：只记位置，内容物一律 `Randomised`，
    /// 并且把"被跳过"的次数报给 `loot_payloads_skipped`。
    parsing_enabled: bool,
    pub observed_items: u64,
    pub slots_seen: u64,
}

impl ContainerCollector {
    pub fn new(parsing_enabled: bool) -> Self {
        Self { containers: HashMap::new(), parsing_enabled, observed_items: 0, slots_seen: 0 }
    }

    pub fn set_parsing_enabled(&mut self, enabled: bool) {
        self.parsing_enabled = enabled;
        if !enabled {
            // 关闭解析时把已缓存的随机内容标记为不可信；位置保留。
            for c in self.containers.values_mut() {
                if c.status == ContentsStatus::Randomised {
                    c.slots.clear();
                }
            }
        }
    }

    pub fn parsing_enabled(&self) -> bool {
        self.parsing_enabled
    }

    pub fn len(&self) -> usize {
        self.containers.len()
    }
    pub fn is_empty(&self) -> bool {
        self.containers.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &Container> {
        self.containers.values()
    }

    /// 从实体表同步容器位置（每帧调用，轻量）。
    pub fn sync_from_entities(&mut self, table: &EntityTable, now_ms: u64) -> usize {
        let mut added = 0;
        for rec in table.iter() {
            if rec.kind != ActorKind::Container || !rec.has_valid_position() {
                continue;
            }
            let entry = self.containers.entry(rec.channel).or_insert_with(|| {
                added += 1;
                Container {
                    channel: rec.channel,
                    class: rec.class.clone(),
                    guid: rec.guid,
                    position: rec.position,
                    status: ContentsStatus::Randomised,
                    slots: Vec::new(),
                    opened_by: None,
                    first_seen_ms: now_ms,
                    last_update_ms: now_ms,
                }
            });
            entry.position = rec.position;
            entry.last_update_ms = now_ms;
            if entry.class.is_empty() {
                entry.class = rec.class.clone();
            }
        }
        added
    }

    /// 应用一次"容器内容物已观察"事件（`DFMContainerDataCollector` 属性块）。
    pub fn observe_contents(
        &mut self,
        channel: u32,
        slots: Vec<CollectorSlot>,
        now_ms: u64,
    ) -> bool {
        if !self.parsing_enabled {
            return false;
        }
        let Some(c) = self.containers.get_mut(&channel) else { return false };
        c.slots = slots;
        c.status = if c.slots.is_empty() { ContentsStatus::Empty } else { ContentsStatus::Observed };
        c.last_update_ms = now_ms;
        self.slots_seen += c.slots.len() as u64;
        self.observed_items += c.slots.iter().map(|s| s.count as u64).sum::<u64>();
        true
    }

    /// 标记某容器已被打开（用于在雷达上把"已搜过的箱子"变灰）。
    pub fn mark_opened(&mut self, channel: u32, by: Option<String>, now_ms: u64) {
        if let Some(c) = self.containers.get_mut(&channel) {
            c.opened_by = by;
            c.last_update_ms = now_ms;
            if c.status == ContentsStatus::Randomised {
                // 打开了但内容未复制到：如实标 Empty，不编造。
                c.status = ContentsStatus::Empty;
            }
        }
    }

    /// 清理长时间未更新的容器（默认 5 分钟，一局时长内）。
    pub fn reap_stale(&mut self, now_ms: u64, ttl_ms: u64) -> usize {
        let before = self.containers.len();
        self.containers
            .retain(|_, c| now_ms.saturating_sub(c.last_update_ms) < ttl_ms);
        before - self.containers.len()
    }

    pub fn clear(&mut self) -> usize {
        let n = self.containers.len();
        self.containers.clear();
        self.observed_items = 0;
        self.slots_seen = 0;
        n
    }

    /// 供 WS 序列化。
    pub fn to_json(&self, price: impl Fn(u64) -> Option<u32>) -> Vec<serde_json::Value> {
        let mut out: Vec<_> = self
            .containers
            .values()
            .map(|c| {
                let p = c.position.to_metres();
                serde_json::json!({
                    "channel": c.channel,
                    "class": c.class,
                    "kind": "loot",
                    "x": p[0], "y": p[1], "z": p[2],
                    "contents_status": c.status.as_str(),
                    "value": c.estimated_value(&price),
                    "items": c.slots.iter().map(|s| serde_json::json!({
                        "slot": s.slot,
                        "item_id": s.item_id,
                        "name": s.item_name,
                        "count": s.count,
                        "rarity": s.rarity,
                    })).collect::<Vec<_>>(),
                    "last_seen_ms": c.last_update_ms,
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b["value"].as_u64().unwrap_or(0).cmp(&a["value"].as_u64().unwrap_or(0))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_with_container() -> EntityTable {
        let mut t = EntityTable::new();
        let r = t.open_channel(20, Some("DFMContainerDataCollector"), Some(3), 100);
        r.position = Vec3 { x: 500.0, y: -200.0, z: 0.0 };
        t
    }

    #[test]
    fn sync_picks_up_container_positions() {
        let t = table_with_container();
        let mut c = ContainerCollector::new(true);
        assert_eq!(c.sync_from_entities(&t, 200), 1);
        assert_eq!(c.iter().next().unwrap().position.x, 500.0);
        // 二次同步不重复添加
        assert_eq!(c.sync_from_entities(&t, 300), 0);
    }

    #[test]
    fn contents_are_randomised_until_observed() {
        let t = table_with_container();
        let mut c = ContainerCollector::new(true);
        c.sync_from_entities(&t, 200);
        let cont = c.iter().next().unwrap();
        assert_eq!(cont.status, ContentsStatus::Randomised);
        assert!(cont.slots.is_empty());

        assert!(c.observe_contents(
            20,
            vec![CollectorSlot {
                slot: 2,
                item_id: Some(10010000903),
                item_name: Some("M16A4".into()),
                count: 1,
                rarity: Some(4),
            }],
            300
        ));
        let cont = c.iter().next().unwrap();
        assert_eq!(cont.status, ContentsStatus::Observed);
        assert_eq!(c.observed_items, 1);
    }

    #[test]
    fn parsing_disabled_skips_contents_but_keeps_positions() {
        let t = table_with_container();
        let mut c = ContainerCollector::new(false);
        c.sync_from_entities(&t, 200);
        assert!(!c.observe_contents(20, vec![], 300));
        assert_eq!(c.iter().next().unwrap().position.x, 500.0);
        assert_eq!(c.observed_items, 0);
    }

    #[test]
    fn opening_a_container_is_marked_but_not_fabricated() {
        let t = table_with_container();
        let mut c = ContainerCollector::new(true);
        c.sync_from_entities(&t, 200);
        c.mark_opened(20, Some("老六".into()), 400);
        let cont = c.iter().next().unwrap();
        assert_eq!(cont.opened_by.as_deref(), Some("老六"));
        assert_eq!(cont.status, ContentsStatus::Empty, "unknown contents must not be invented");
    }

    #[test]
    fn value_estimation_uses_price_table() {
        let t = table_with_container();
        let mut c = ContainerCollector::new(true);
        c.sync_from_entities(&t, 200);
        c.observe_contents(
            20,
            vec![
                CollectorSlot { slot: 1, item_id: Some(10), item_name: None, count: 1, rarity: None },
                CollectorSlot { slot: 2, item_id: Some(20), item_name: None, count: 1, rarity: None },
            ],
            300,
        );
        let v = c.iter().next().unwrap().estimated_value(|id| if id == 10 { Some(500) } else { Some(1500) });
        assert_eq!(v, 2000);
    }

    #[test]
    fn stale_reaping_and_clear() {
        let t = table_with_container();
        let mut c = ContainerCollector::new(true);
        c.sync_from_entities(&t, 1_000);
        assert_eq!(c.reap_stale(1_000_000, 300_000), 1);
        c.sync_from_entities(&t, 2_000);
        assert_eq!(c.clear(), 1);
        assert!(c.is_empty());
    }

    #[test]
    fn json_reports_status_and_metres() {
        let t = table_with_container();
        let mut c = ContainerCollector::new(true);
        c.sync_from_entities(&t, 200);
        let j = c.to_json(|_| None);
        assert_eq!(j[0]["kind"], "loot");
        assert_eq!(j[0]["x"], 5.0);
        assert_eq!(j[0]["contents_status"], "randomised_not_transmitted");
    }
}
