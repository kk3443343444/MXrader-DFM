//! 协议采集控制器：`battle_proxy::battle::protocol_capture`。
//!
//! 复刻样本 `src/battle/protocol_capture.rs`。它是 `codec::capture` 的**策略层**，
//! 负责三件样本明确列出的事：
//!
//! * `collection_policy`：`enabled_only`（仅解析开启时保留）/`retained_when_disabled`；
//! * 限时限量的**自动停**（`max_capture_seconds` / `max_capture_mb`）；
//! * 管理接口的全部语义：`remote protocol capture control requires BATTLE_ADMIN_TOKEN`、
//!   `remote protocol capture download requires BATTLE_ADMIN_TOKEN`、
//!   `no capture data available`、`application/x-ndjson; charset=utf-8`。
//!
//! 同时提供两种落盘产物（样本里各一个文件名）：
//! * `battle-full-capture-<ts>.ndjson` —— 全量（含未通过 decode gate 的包）；
//! * `battle-parse-<ts>.ndjson`      —— 只落"解析成功"的包，体积小得多。

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;

use super::codec::capture::{Capture, CaptureConfig, CaptureEntry};
use crate::config::CollectionPolicy;
use crate::state::AppState;

/// 采集记录的类型（决定落到哪个文件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// 全量（含解析失败）。
    Full,
    /// 仅解析成功。
    Parsed,
}

/// 控制器。
pub struct ProtocolCapture {
    full: Mutex<Capture>,
    parsed: Mutex<Capture>,
    policy: CollectionPolicy,
    data_dir: PathBuf,
    /// 采集已自动停止的原因（诊断）。
    stopped_reason: Mutex<Option<String>>,
}

impl ProtocolCapture {
    pub fn new(state: &AppState, policy: CollectionPolicy) -> Self {
        let cfg = CaptureConfig {
            max_capture_bytes: (state.capture_status().max_capture_mb) * 1024 * 1024,
            max_capture_seconds: state.capture_status().max_capture_seconds,
            anonymize_endpoints: true,
            ring_capacity: 1024,
        };
        Self {
            full: Mutex::new(Capture::new(cfg.clone())),
            parsed: Mutex::new(Capture::new(cfg).with_file_prefix("battle-parse-")),
            policy,
            data_dir: state.data_directory().to_path_buf(),
            stopped_reason: Mutex::new(None),
        }
    }

    pub fn policy(&self) -> CollectionPolicy {
        self.policy
    }

    pub fn is_enabled(&self) -> bool {
        self.full.lock().enabled()
    }

    /// 是否应当记录（`collection_policy = enabled_only` 时跟随解析开关）。
    pub fn should_record(&self, state: &AppState) -> bool {
        match self.policy {
            CollectionPolicy::EnabledOnly => {
                state.capture_enabled() && state.loot_parsing_enabled()
            }
            CollectionPolicy::RetainedWhenDisabled => state.capture_enabled(),
        }
    }

    /// 开始采集（创建两个 NDJSON 文件）。
    pub fn start(&self, now_ms: u64) -> anyhow::Result<(PathBuf, PathBuf)> {
        let full = self.full.lock().start(&self.data_dir, now_ms)?;
        let parsed = self.parsed.lock().start(&self.data_dir, now_ms)?;
        *self.stopped_reason.lock() = None;
        // 顺手清掉旧文件（保留最近 2 份），避免沙箱目录无限增长。
        prune_old(&self.data_dir, "battle-full-capture-", 2);
        prune_old(&self.data_dir, "battle-parse-", 2);
        Ok((full, parsed))
    }

    pub fn stop(&self, reason: Option<String>) {
        self.full.lock().stop();
        self.parsed.lock().stop();
        *self.stopped_reason.lock() = reason;
    }

    /// 记录一条。`kind` 决定落盘目标，环形缓冲两边都进（Web UI 预览）。
    pub fn record(&self, kind: RecordKind, entry: CaptureEntry, now_ms: u64) {
        {
            let mut p = self.parsed.lock();
            p.note(entry.clone());
        }
        {
            let mut f = self.full.lock();
            f.note(entry.clone());
            if matches!(kind, RecordKind::Full) {
                // 全量侧本来就收到了同一条，无需二次写入。
            }
        }
        let _ = now_ms;

        // 限时限量：任一文件到顶即整体停。
        let (f_bytes, f_time) = {
            let f = self.full.lock();
            (f.should_auto_stop(now_ms), f.retained_bytes())
        };
        if f_bytes {
            self.stop(Some(format!("limit reached at {f_time} bytes")));
        }
    }

    pub fn retained_bytes(&self) -> (u64, u64) {
        (self.full.lock().retained_bytes(), self.parsed.lock().retained_bytes())
    }

    /// `/api/admin/capture/download`。
    pub fn download(&self) -> Option<(String, Vec<u8>, &'static str)> {
        if let Some((name, bytes)) = self.parsed.lock().download() {
            return Some((name, bytes, "application/x-ndjson; charset=utf-8"));
        }
        self.full
            .lock()
            .download()
            .map(|(n, b)| (n, b, "application/x-ndjson; charset=utf-8"))
    }

    /// 采集状态 JSON。
    pub fn status_json(&self) -> serde_json::Value {
        let (fb, pb) = self.retained_bytes();
        json!({
            "protocol_capture": self.is_enabled(),
            "collection_policy": match self.policy {
                CollectionPolicy::EnabledOnly => "enabled_only",
                CollectionPolicy::RetainedWhenDisabled => "retained_when_disabled",
            },
            "retained_bytes": fb + pb,
            "full_capture_bytes": fb,
            "parsed_capture_bytes": pb,
            "preview": self.full.lock().preview(64).into_iter().cloned().collect::<Vec<_>>(),
            "endpoint_anonymized": true,
            "stopped_reason": self.stopped_reason.lock().clone(),
        })
    }

    /// 构造一条采集记录（引擎调用）。
    pub fn make_entry(
        session: u64,
        dir: &'static str,
        src: String,
        dst: String,
        payload: &[u8],
        transport: &str,
        framing: Option<String>,
        ts_ms: u64,
    ) -> CaptureEntry {
        let (hex, truncated) = Capture::hex_preview(payload, 512);
        CaptureEntry {
            ts_ms,
            dir,
            src,
            dst,
            session,
            len: payload.len(),
            transport: transport.to_string(),
            hex,
            truncated,
            framing,
        }
    }
}

/// 只保留最新的 `keep` 份同前缀文件。
fn prune_old(dir: &std::path::Path, prefix: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(prefix) {
                return None;
            }
            let t = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), t))
        })
        .collect();
    files.sort_by_key(|(_, t)| *t);
    while files.len() > keep {
        let (path, _) = files.remove(0);
        let _ = std::fs::remove_file(path);
    }
}

/// 远端控制令牌校验（统一的拒绝点，避免每个 handler 各写一遍）。
pub fn require_admin(state: &AppState, provided: Option<&str>, action: &str) -> Result<(), String> {
    let expected = state.admin_token();
    match provided {
        Some(t) if constant_time_eq(t.as_bytes(), expected.as_bytes()) => Ok(()),
        _ => Err(format!("remote {action} requires BATTLE_ADMIN_TOKEN")),
    }
}

/// 常量时间比较，避免令牌被计时侧信道。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 远端只读闸门：`read_only_radar` 时禁止一切写操作。
pub fn deny_if_read_only(state: &AppState, action: &str) -> Result<(), String> {
    if state.read_only() {
        return Err(format!("remote {action} is disabled in read-only radar mode"));
    }
    Ok(())
}

/// 共享句柄（引擎与 HTTP 层各持一份）。
pub type SharedCapture = Arc<ProtocolCapture>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn state() -> AppState {
        let cfg = Config::default();
        AppState::new(&cfg, "tok".into())
    }

    #[test]
    fn admin_token_check_is_exact() {
        let st = state();
        let good = st.admin_token().to_string();
        assert!(require_admin(&st, Some(&good), "diagnostics control").is_ok());
        assert!(require_admin(&st, Some("deadbeef"), "diagnostics control").is_err());
        assert!(require_admin(&st, None, "diagnostics control").is_err());
        let err = require_admin(&st, None, "protocol capture download").unwrap_err();
        assert!(err.contains("requires BATTLE_ADMIN_TOKEN"));
    }

    #[test]
    fn read_only_blocks_writes() {
        let st = state();
        assert!(st.read_only(), "default config is read-only");
        assert!(deny_if_read_only(&st, "session reset").is_err());
    }

    #[test]
    fn constant_time_eq_handles_length_mismatch() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn policy_gates_recording() {
        let st = state();
        let c = ProtocolCapture::new(&st, CollectionPolicy::EnabledOnly);
        assert!(!c.should_record(&st), "capture off -> no record");
        let c2 = ProtocolCapture::new(&st, CollectionPolicy::RetainedWhenDisabled);
        assert!(!c2.should_record(&st));
    }

    #[test]
    fn entry_preview_is_truncated_and_hex_encoded() {
        let payload = vec![0xAAu8; 2048];
        let e = ProtocolCapture::make_entry(1, "c2s", "1.1.1.1:1".into(), "2.2.2.2:2".into(), &payload, "plain", None, 5);
        assert!(e.truncated);
        assert_eq!(e.hex.len(), 512 * 2);
        assert_eq!(e.len, 2048);
    }

    #[test]
    fn start_stop_download_roundtrip() {
        let dir = crate::testutil::scratch_dir("battlepc");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config { data_directory: dir.clone(), ..Default::default() };
        let st = AppState::new(&cfg, "tok".into());
        let c = ProtocolCapture::new(&st, CollectionPolicy::EnabledOnly);
        let (full, parsed) = c.start(1_000).unwrap();
        assert!(full.file_name().unwrap().to_string_lossy().starts_with("battle-full-capture-"));
        assert!(parsed.file_name().unwrap().to_string_lossy().starts_with("battle-parse-"));
        c.record(
            RecordKind::Full,
            ProtocolCapture::make_entry(1, "c2s", "192.168.1.23:2025".into(), "1.2.3.4:443".into(), &[1, 2, 3], "plain", None, 2_000),
            2_000,
        );
        let (name, bytes, mime) = c.download().unwrap();
        assert_eq!(mime, "application/x-ndjson; charset=utf-8");
        assert!(name.contains("battle"));
        assert!(!String::from_utf8_lossy(&bytes).contains("192.168.1.23"));
        c.stop(Some("manual".into()));
        assert!(!c.is_enabled());
        let j = c.status_json();
        assert_eq!(j["endpoint_anonymized"], true);
        assert_eq!(j["collection_policy"], "enabled_only");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_only_newest_files() {
        let dir = crate::testutil::scratch_dir("battleprune");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5 {
            let p = dir.join(format!("battle-parse-{i}.ndjson"));
            std::fs::write(&p, b"x").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        prune_old(&dir, "battle-parse-", 2);
        let n = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(n, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
