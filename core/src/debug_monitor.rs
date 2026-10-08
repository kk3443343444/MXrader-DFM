//! 运行监测：`battle_proxy::debug_monitor`。
//!
//! 复刻样本 `src/debug_monitor.rs`。样本的 `/api/status` 里有一组性能字段，
//! 这里把它们变成有明确语义的监测器：
//!
//! ```text
//! debug_started_at_ms
//! performancelast_server_transmission_ms
//! milliseconds_since_last_server_transmission
//! sessionpositioned_in_page
//! ```
//!
//! 用途很具体：**判断"游戏是不是还在跑"**。三角洲的服务器每秒都会推位置复制，
//! 若 `milliseconds_since_last_server_transmission` 持续 > 3 秒，说明：
//! * 玩家已经退出对局（回到大厅，没有复制流量）；或
//! * B 机的代理没把 UDP 转过来（`小火箭必须启用 UDP 转发` 就是这条）。
//!
//! 界面据此给出提示，而不是让用户对着一张空雷达发呆。本模块只读不写业务状态。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// 性能计数器。
#[derive(Debug, Default)]
pub struct Performance {
    /// 最近一次收到"服务端下发"的时间。
    pub last_server_transmission_ms: AtomicU64,
    /// 最近一次收到客户端上行的时间。
    pub last_client_transmission_ms: AtomicU64,
    /// 已处理的 datagram 总数。
    pub datagrams: AtomicU64,
    /// 通过 decode gate 的数量。
    pub gated: AtomicU64,
    /// 累计解析出的位移更新数。
    pub moves: AtomicU64,
}

impl Performance {
    pub fn snapshot(&self, now_ms: u64) -> PerfSnapshot {
        let last_s2c = self.last_server_transmission_ms.load(Ordering::Relaxed);
        PerfSnapshot {
            last_server_transmission_ms: last_s2c,
            milliseconds_since_last_server_transmission: if last_s2c == 0 {
                u64::MAX
            } else {
                now_ms.saturating_sub(last_s2c)
            },
            last_client_transmission_ms: self.last_client_transmission_ms.load(Ordering::Relaxed),
            datagrams: self.datagrams.load(Ordering::Relaxed),
            gated: self.gated.load(Ordering::Relaxed),
            moves: self.moves.load(Ordering::Relaxed),
        }
    }
}

/// 性能快照（进 `/api/status` 的 `performance`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSnapshot {
    pub last_server_transmission_ms: u64,
    /// `u64::MAX` 表示从未收到过。
    pub milliseconds_since_last_server_transmission: u64,
    pub last_client_transmission_ms: u64,
    pub datagrams: u64,
    pub gated: u64,
    pub moves: u64,
}

impl PerfSnapshot {
    /// 是否"对局中"（3 秒内有服务端下发）。
    pub fn in_match(&self) -> bool {
        self.milliseconds_since_last_server_transmission < 3_000
    }

    /// 给前端/诊断页的一句人话。
    pub fn diagnosis_zh(&self) -> &'static str {
        if self.last_server_transmission_ms == 0 {
            "等待代理核心数据"
        } else if self.in_match() {
            "对局中"
        } else if self.milliseconds_since_last_server_transmission < 15_000 {
            "暂未收到服务端数据（可能已回大厅）"
        } else {
            "长时间无数据：请确认小火箭已启用 UDP 转发，且 B 机在对局内"
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "last_server_transmission_ms": self.last_server_transmission_ms,
            "milliseconds_since_last_server_transmission":
                if self.milliseconds_since_last_server_transmission == u64::MAX {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(self.milliseconds_since_last_server_transmission)
                },
            "last_client_transmission_ms": self.last_client_transmission_ms,
            "datagrams": self.datagrams,
            "gated": self.gated,
            "moves": self.moves,
            "in_match": self.in_match(),
            "diagnosis": self.diagnosis_zh(),
        })
    }
}

/// 监测器句柄。
#[derive(Clone)]
pub struct DebugMonitor {
    inner: Arc<Inner>,
}

struct Inner {
    started_at_ms: u64,
    perf: Performance,
    /// 是否允许把周期性快照写盘（默认关；诊断页"网络日志"开启）。
    persist: AtomicBool,
    data_dir: std::path::PathBuf,
    /// 最近一次落盘时间（限频：最多 1 秒一次）。
    last_dump_ms: AtomicU64,
}

impl DebugMonitor {
    pub fn new(data_dir: std::path::PathBuf, started_at_ms: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                started_at_ms,
                perf: Performance::default(),
                persist: AtomicBool::new(false),
                data_dir,
                last_dump_ms: AtomicU64::new(0),
            }),
        }
    }

    pub fn started_at_ms(&self) -> u64 {
        self.inner.started_at_ms
    }

    pub fn perf(&self) -> &Performance {
        &self.inner.perf
    }

    pub fn set_persist(&self, on: bool) {
        self.inner.persist.store(on, Ordering::Relaxed);
    }

    pub fn persisting(&self) -> bool {
        self.inner.persist.load(Ordering::Relaxed)
    }

    /// 收到一条 datagram（`c2s = false` 表示服务端下发）。
    pub fn note_datagram(&self, c2s: bool, now_ms: u64) {
        self.inner.perf.datagrams.fetch_add(1, Ordering::Relaxed);
        if c2s {
            self.inner.perf.last_client_transmission_ms.store(now_ms, Ordering::Relaxed);
        } else {
            self.inner.perf.last_server_transmission_ms.store(now_ms, Ordering::Relaxed);
        }
    }

    pub fn note_gated(&self) {
        self.inner.perf.gated.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_move(&self) {
        self.inner.perf.moves.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self, now_ms: u64) -> PerfSnapshot {
        self.inner.perf.snapshot(now_ms)
    }

    /// 周期性落盘（默认 1 秒最多一次；`persist` 关闭时是空操作）。
    pub fn maybe_dump(&self, now_ms: u64, extra: serde_json::Value) -> bool {
        if !self.persisting() {
            return false;
        }
        let last = self.inner.last_dump_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < 1_000 {
            return false;
        }
        if self
            .inner
            .last_dump_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }

        let line = serde_json::json!({
            "ts": now_ms,
            "debug_started_at_ms": self.inner.started_at_ms,
            "performance": self.snapshot(now_ms).to_json(),
            "extra": extra,
        });
        let path = self.inner.data_dir.join("battle-parse-monitor.ndjson");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
            return true;
        }
        false
    }

    /// 监测器自身的 JSON（进 `/api/status`）。
    pub fn to_json(&self, now_ms: u64) -> serde_json::Value {
        serde_json::json!({
            "debug_started_at_ms": self.inner.started_at_ms,
            "performance": self.snapshot(now_ms).to_json(),
            "persist": self.persisting(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        let d = crate::testutil::scratch_dir("battlemon");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn never_received_reports_null_and_hint() {
        let m = DebugMonitor::new(dir(), 100);
        let s = m.snapshot(500);
        assert_eq!(s.last_server_transmission_ms, 0);
        assert_eq!(s.milliseconds_since_last_server_transmission, u64::MAX);
        assert!(!s.in_match());
        assert_eq!(s.diagnosis_zh(), "等待代理核心数据");
        assert!(s.to_json()["milliseconds_since_last_server_transmission"].is_null());
    }

    #[test]
    fn in_match_window_is_three_seconds() {
        let m = DebugMonitor::new(dir(), 0);
        m.note_datagram(false, 10_000);
        assert!(m.snapshot(11_000).in_match());
        assert_eq!(m.snapshot(11_000).diagnosis_zh(), "对局中");
        assert!(!m.snapshot(14_000).in_match());
        assert!(m.snapshot(14_000).diagnosis_zh().contains("回大厅"));
        m.note_datagram(false, 20_000);
        assert!(m.snapshot(60_000).diagnosis_zh().contains("UDP 转发"));
    }

    #[test]
    fn direction_counters_are_separate() {
        let m = DebugMonitor::new(dir(), 0);
        m.note_datagram(true, 1_000);
        let s = m.snapshot(1_000);
        assert_eq!(s.last_client_transmission_ms, 1_000);
        assert_eq!(s.last_server_transmission_ms, 0);
        m.note_datagram(false, 2_000);
        assert_eq!(m.snapshot(2_000).last_server_transmission_ms, 2_000);
        assert_eq!(m.snapshot(2_000).datagrams, 2);
    }

    #[test]
    fn counters_accumulate() {
        let m = DebugMonitor::new(dir(), 0);
        for _ in 0..5 {
            m.note_gated();
            m.note_move();
        }
        let s = m.snapshot(0);
        assert_eq!(s.gated, 5);
        assert_eq!(s.moves, 5);
    }

    #[test]
    fn dump_only_when_persisting_and_throttled() {
        let d = dir();
        let m = DebugMonitor::new(d.clone(), 7);
        assert!(!m.maybe_dump(1_000, serde_json::json!({})));
        m.set_persist(true);
        assert!(m.maybe_dump(1_000, serde_json::json!({})));
        assert!(!m.maybe_dump(1_500, serde_json::json!({})), "throttled");
        assert!(m.maybe_dump(2_100, serde_json::json!({})));
        let content = std::fs::read_to_string(d.join("battle-parse-monitor.ndjson")).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(content.contains("debug_started_at_ms"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn json_shape_includes_started_at() {
        let m = DebugMonitor::new(dir(), 1234);
        let j = m.to_json(2_000);
        assert_eq!(j["debug_started_at_ms"], 1234);
        assert!(j["performance"].get("gated").is_some());
    }
}
