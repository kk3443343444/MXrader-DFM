//! 目标地址统计（对应需求「destination census」）。
//!
//! 用户为什么会需要它：分流规则（"只代理游戏"）要求知道**游戏服务器 IP**，而 App 里
//! 本来没有任何地方能查出来。手机全局代理时 B 机的所有流量都双跳回 A 机，其他 App 直接
//! 断网 —— 于是"把游戏服务器 IP 告诉我"变成了刚需。
//!
//! 这里统计的是 SOCKS5 的 **TCP CONNECT 目标**与 **UDP 转发的每个 dest**，按目标 IP
//! 聚合（不按端口，因为一个游戏会同时用很多端口，而分流规则要的是 IP 段）。
//!
//! 有界内存：最多 [`MAX_TRACKED_DESTINATIONS`] 个 IP，超出时按**最久未更新**淘汰；
//! 淘汰不影响累计量（[`DestinationCensus::total_packets`] 记录见过的总量）。
//!
//! 并发：条目用 `DashMap` + 原子计数，网络热路径上只做一次 hash 查找 + 两次
//! `fetch_add`/`store`，不抢锁、不分配。

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// 最多同时跟踪多少条目标（超出按最久未更新淘汰）。
pub const MAX_TRACKED_DESTINATIONS: usize = 128;

/// 状态 JSON（`status.destinations`）里最多给多少条 —— 诊断页要的是"前几名"。
pub const STATUS_DESTINATION_LIMIT: usize = 32;

/// 雷达页 WS `state` 帧里最多给多少条（前端再取 Top 10）。
pub const RADAR_DESTINATION_LIMIT: usize = 10;

/// 一个目标的聚合结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationStat {
    /// 目标 IP（不带端口）。
    pub ip: String,
    /// 命中次数：TCP CONNECT 记 1，UDP 每个数据报记 1。
    pub packets: u64,
    /// 最后一次命中的 UNIX 毫秒时间戳。
    pub last_ms: u64,
}

/// 计数器的整体情况（诊断用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CensusStats {
    /// 当前跟踪中的目标数。
    pub tracked: usize,
    /// 上限。
    pub max_tracked: usize,
    /// 见过的命中总量（含已被淘汰的目标）。
    pub total_packets: u64,
    /// 因为超出上限被淘汰的次数。
    pub evictions: u64,
}

#[derive(Debug, Default)]
struct Entry {
    packets: AtomicU64,
    last_ms: AtomicU64,
}

/// 目标地址统计表。句柄很便宜（内部 `Arc` 由调用方持有）。
#[derive(Debug)]
pub struct DestinationCensus {
    entries: DashMap<IpAddr, Entry>,
    total_packets: AtomicU64,
    evictions: AtomicU64,
    max_entries: usize,
}

impl Default for DestinationCensus {
    fn default() -> Self {
        Self::new()
    }
}

impl DestinationCensus {
    pub fn new() -> Self {
        Self::with_limit(MAX_TRACKED_DESTINATIONS)
    }

    /// 自定义上限（用例里用小上限来验证淘汰行为）。
    pub fn with_limit(max_entries: usize) -> Self {
        Self {
            entries: DashMap::new(),
            total_packets: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            max_entries: max_entries.max(1),
        }
    }

    /// 记一次命中：`ip` 是目标地址，`ts_ms` 是发生时刻。
    ///
    /// 更新已存在的条目走 `DashMap::get`（命中率极高，因为目标集合很小且稳定）；
    /// 只有第一次见到某个 IP 才插入并可能触发淘汰。
    pub fn observe(&self, ip: IpAddr, ts_ms: u64) {
        self.total_packets.fetch_add(1, Ordering::Relaxed);

        if let Some(entry) = self.entries.get(&ip) {
            entry.packets.fetch_add(1, Ordering::Relaxed);
            entry.last_ms.store(ts_ms, Ordering::Relaxed);
            return;
        }

        self.entries.insert(
            ip,
            Entry {
                packets: AtomicU64::new(1),
                last_ms: AtomicU64::new(ts_ms),
            },
        );
        self.enforce_limit();
    }

    /// 淘汰到上限以内：每次挑 `last_ms` 最小的（= 最久未更新）丢掉。
    ///
    /// 并发插入时可能短暂超过上限几个条目，所以这里是 `while` 而不是 `if`；
    /// 正常情况下 `len <= max_entries` 直接返回，热路径上只多一次原子读。
    fn enforce_limit(&self) {
        while self.entries.len() > self.max_entries {
            let victim = self
                .entries
                .iter()
                .min_by_key(|e| e.value().last_ms.load(Ordering::Relaxed))
                .map(|e| *e.key());
            match victim {
                Some(ip) => {
                    self.entries.remove(&ip);
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                }
                // 表被清空（理论上不会）：退出，免得死循环。
                None => break,
            }
        }
    }

    /// 按命中次数降序（同次数按最近更新降序，再按 IP 升序保证确定性）取前 `limit` 条。
    pub fn snapshot(&self, limit: usize) -> Vec<DestinationStat> {
        let mut all: Vec<DestinationStat> = self
            .entries
            .iter()
            .map(|e| {
                let entry = e.value();
                DestinationStat {
                    ip: e.key().to_string(),
                    packets: entry.packets.load(Ordering::Relaxed),
                    last_ms: entry.last_ms.load(Ordering::Relaxed),
                }
            })
            .collect();
        all.sort_by(|a, b| {
            b.packets
                .cmp(&a.packets)
                .then_with(|| b.last_ms.cmp(&a.last_ms))
                .then_with(|| a.ip.cmp(&b.ip))
        });
        all.truncate(limit);
        all
    }

    /// 当前跟踪中的目标数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 见过的命中总量（含已被淘汰的目标）。
    pub fn total_packets(&self) -> u64 {
        self.total_packets.load(Ordering::Relaxed)
    }

    /// 淘汰次数。
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    /// 诊断汇总。
    pub fn stats(&self) -> CensusStats {
        CensusStats {
            tracked: self.len(),
            max_tracked: self.max_entries,
            total_packets: self.total_packets(),
            evictions: self.evictions(),
        }
    }

    /// 清空（`/api/admin/session/reset` 之类的地方用得上）。
    pub fn clear(&self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test ip")
    }

    #[test]
    fn aggregates_by_ip_and_tracks_the_last_seen_stamp() {
        let c = DestinationCensus::new();
        c.observe(ip("1.2.3.4"), 1_000);
        c.observe(ip("1.2.3.4"), 2_000);
        c.observe(ip("5.6.7.8"), 1_500);

        let snap = c.snapshot(10);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].ip, "1.2.3.4");
        assert_eq!(snap[0].packets, 2);
        assert_eq!(snap[0].last_ms, 2_000, "last_ms 必须是最后一次命中");
        assert_eq!(snap[1].ip, "5.6.7.8");
        assert_eq!(snap[1].packets, 1);
        assert_eq!(c.total_packets(), 3);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn snapshot_is_sorted_by_packets_desc_then_recency() {
        let c = DestinationCensus::new();
        for _ in 0..3 {
            c.observe(ip("9.9.9.9"), 100);
        }
        for _ in 0..5 {
            c.observe(ip("8.8.8.8"), 200);
        }
        c.observe(ip("7.7.7.7"), 300);

        let snap = c.snapshot(2);
        assert_eq!(snap.len(), 2, "limit 生效");
        assert_eq!(snap[0].ip, "8.8.8.8");
        assert_eq!(snap[0].packets, 5);
        assert_eq!(snap[1].ip, "9.9.9.9");

        // 同包数时按最近更新在前。
        let c2 = DestinationCensus::new();
        c2.observe(ip("1.1.1.1"), 10);
        c2.observe(ip("2.2.2.2"), 20);
        let s2 = c2.snapshot(2);
        assert_eq!(s2[0].ip, "2.2.2.2");
        assert_eq!(s2[1].ip, "1.1.1.1");
    }

    #[test]
    fn capacity_is_bounded_and_the_stalest_target_is_evicted() {
        let c = DestinationCensus::with_limit(4);
        // 4 个目标，时间戳递增：1111 最旧。
        c.observe(ip("10.0.0.1"), 1_000);
        c.observe(ip("10.0.0.2"), 2_000);
        c.observe(ip("10.0.0.3"), 3_000);
        c.observe(ip("10.0.0.4"), 4_000);
        assert_eq!(c.len(), 4);
        assert_eq!(c.evictions(), 0);

        // 第 5 个：淘汰"最久未更新"的 10.0.0.1。
        c.observe(ip("10.0.0.5"), 5_000);
        assert_eq!(c.len(), 4, "有界内存：永远不超过上限");
        let ips: Vec<String> = c.snapshot(10).into_iter().map(|d| d.ip).collect();
        assert!(!ips.contains(&"10.0.0.1".to_string()), "最久未更新的被淘汰: {ips:?}");
        assert!(ips.contains(&"10.0.0.5".to_string()));
        assert_eq!(c.evictions(), 1);
        // 淘汰不影响累计量。
        assert_eq!(c.total_packets(), 5);
    }

    #[test]
    fn refreshing_a_target_protects_it_from_eviction() {
        let c = DestinationCensus::with_limit(3);
        c.observe(ip("10.0.0.1"), 1_000);
        c.observe(ip("10.0.0.2"), 2_000);
        c.observe(ip("10.0.0.3"), 3_000);
        // 10.0.0.1 又被访问了一次：它变成"最近更新"，该被淘汰的是 10.0.0.2。
        c.observe(ip("10.0.0.1"), 9_000);
        c.observe(ip("10.0.0.9"), 10_000);

        let ips: Vec<String> = c.snapshot(10).into_iter().map(|d| d.ip).collect();
        assert!(ips.contains(&"10.0.0.1".to_string()), "刚更新过的目标必须留下: {ips:?}");
        assert!(!ips.contains(&"10.0.0.2".to_string()), "该淘汰的是 10.0.0.2: {ips:?}");
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn stats_report_the_configured_bound() {
        let c = DestinationCensus::new();
        for i in 0..(MAX_TRACKED_DESTINATIONS + 5) {
            let octets = (i % 250 + 1) as u8;
            c.observe(ip(&format!("10.1.{}.{}", i / 250, octets)), 1_000 + i as u64);
        }
        let st = c.stats();
        assert_eq!(st.max_tracked, MAX_TRACKED_DESTINATIONS);
        assert!(st.tracked <= MAX_TRACKED_DESTINATIONS);
        assert_eq!(st.total_packets, (MAX_TRACKED_DESTINATIONS + 5) as u64);
        assert!(st.evictions > 0);
        // 状态 JSON 的取值上限不超过总量上限。
        assert!(c.snapshot(STATUS_DESTINATION_LIMIT).len() <= STATUS_DESTINATION_LIMIT);
        assert!(RADAR_DESTINATION_LIMIT <= STATUS_DESTINATION_LIMIT);
    }
}
