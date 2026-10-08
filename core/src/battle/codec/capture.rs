//! 原始采集缓冲：`battle_proxy::battle::codec::capture`。
//!
//! 复刻样本 `src/battle/codec/capture.rs` + `src/battle/protocol_capture.rs` 的落盘侧。
//! 能力边界完全照抄样本的管理接口字符串：
//!
//! ```text
//! 协议诊断采集已手动停止
//! 整局适配采集已手动开启：文件落盘、限时限量、端点匿名化
//! battle-full-capture-<ts>.ndjson
//! battle-parse-<ts>.ndjson
//! no capture data available
//! application/x-ndjson; charset=utf-8
//! ```
//!
//! 三条硬规则（样本明文写了）
//! 1. **限时限量**：`max_capture_seconds` / `max_capture_mb`，超了自动停；
//! 2. **端点匿名化**：IP:port 一律哈希成 `h:xxxxxxxx`，不落真实地址；
//! 3. **只在显式开启时落盘**：默认关闭，且 `read_only_radar` 时远端无法开启。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 采集配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    pub max_capture_bytes: u64,
    pub max_capture_seconds: u64,
    /// 端点匿名化（默认 true，不允许关闭）。
    pub anonymize_endpoints: bool,
    /// 环形内存缓冲最大条数（供 Web UI 快速预览，不落盘）。
    pub ring_capacity: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            max_capture_bytes: 64 * 1024 * 1024,
            max_capture_seconds: 600,
            anonymize_endpoints: true,
            ring_capacity: 512,
        }
    }
}

/// 一条采集记录（NDJSON 一行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureEntry {
    pub ts_ms: u64,
    /// 方向：`c2s` / `s2c`
    pub dir: &'static str,
    /// 匿名化后的端点
    pub src: String,
    pub dst: String,
    pub session: u64,
    pub len: usize,
    /// 传输层识别结果（`plain` / `aes_ecb_xor` / `lz4` …）
    pub transport: String,
    /// 十六进制净荷（截断到 `hex_limit`）
    pub hex: String,
    pub truncated: bool,
    /// 分帧结论（`gate_passed` 等）
    pub framing: Option<String>,
}

/// 端点匿名化：`ip:port` → `h:<16 hex>`。
pub fn anonymize(endpoint: &str) -> String {
    if !endpoint.contains(':') {
        return endpoint.to_string();
    }
    let mut h = Sha256::new();
    h.update(endpoint.as_bytes());
    let d = h.finalize();
    format!("h:{}", hex::encode(&d[..8]))
}

/// 采集器。
pub struct Capture {
    cfg: CaptureConfig,
    /// 落盘文件名前缀：全量产物 `battle-full-capture-`，解析产物 `battle-parse-`。
    file_prefix: &'static str,
    enabled: bool,
    started_ms: u64,
    written_bytes: u64,
    ring: std::collections::VecDeque<CaptureEntry>,
    writer: Option<BufWriter<File>>,
    path: Option<PathBuf>,
    /// 本次采集写入的条数。
    pub written_entries: u64,
}

impl Capture {
    pub fn new(cfg: CaptureConfig) -> Self {
        Self {
            cfg,
            file_prefix: "battle-full-capture-",
            enabled: false,
            started_ms: 0,
            written_bytes: 0,
            ring: std::collections::VecDeque::new(),
            writer: None,
            path: None,
            written_entries: 0,
        }
    }

    /// 覆盖落盘文件名前缀（`battle-parse-` 用于"只落解析成功"的产物）。
    pub fn with_file_prefix(mut self, prefix: &'static str) -> Self {
        self.file_prefix = prefix;
        self
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn retained_bytes(&self) -> u64 {
        self.written_bytes
    }

    /// 开启采集：创建 `<prefix><ts>.ndjson`（默认 `battle-full-capture-`）。
    pub fn start(&mut self, data_dir: &Path, now_ms: u64) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(data_dir)?;
        let name = format!("{}{now_ms}.ndjson", self.file_prefix);
        let path = data_dir.join(name);
        let f = OpenOptions::new().create(true).append(true).open(&path)?;
        self.writer = Some(BufWriter::with_capacity(64 * 1024, f));
        self.path = Some(path.clone());
        self.enabled = true;
        self.started_ms = now_ms;
        self.written_bytes = 0;
        self.written_entries = 0;
        tracing::warn!(path = %path.display(), "整局适配采集已手动开启：文件落盘、限时限量、端点匿名化");
        Ok(path)
    }

    /// 停止采集并 flush。
    pub fn stop(&mut self) {
        if let Some(mut w) = self.writer.take() {
            let _ = w.flush();
        }
        self.enabled = false;
        tracing::warn!("协议诊断采集已手动停止");
    }

    /// 是否应当自动停（限时限量）。
    pub fn should_auto_stop(&self, now_ms: u64) -> bool {
        if !self.enabled {
            return false;
        }
        let by_time = now_ms.saturating_sub(self.started_ms) > self.cfg.max_capture_seconds * 1_000;
        let by_size = self.written_bytes >= self.cfg.max_capture_bytes;
        if by_time || by_size {
            tracing::warn!(
                by_time,
                by_size,
                bytes = self.written_bytes,
                "capture limit reached, auto-stopping"
            );
            return true;
        }
        false
    }

    /// 记录一条（无论是否落盘都会进环形缓冲，供 Web UI 预览）。
    pub fn note(&mut self, mut entry: CaptureEntry) {
        if self.cfg.anonymize_endpoints {
            entry.src = anonymize(&entry.src);
            entry.dst = anonymize(&entry.dst);
        }
        self.ring.push_back(entry.clone());
        while self.ring.len() > self.cfg.ring_capacity {
            self.ring.pop_front();
        }

        if !self.enabled {
            return;
        }
        let Some(writer) = self.writer.as_mut() else { return };
        match serde_json::to_vec(&entry) {
            Ok(mut line) => {
                line.push(b'\n');
                if writer.write_all(&line).is_ok() {
                    self.written_bytes += line.len() as u64;
                    self.written_entries += 1;
                } else {
                    self.stop();
                }
            }
            Err(_) => {}
        }
    }

    /// 环形缓冲快照（Web UI 预览）。
    pub fn preview(&self, limit: usize) -> Vec<&CaptureEntry> {
        self.ring.iter().rev().take(limit).collect()
    }

    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// `/api/admin/capture/download` 的响应体：已有的 NDJSON 文件内容。
    pub fn download(&self) -> Option<(String, Vec<u8>)> {
        let path = self.path.as_ref()?;
        let bytes = std::fs::read(path).ok()?;
        Some((
            path.file_name()?.to_string_lossy().to_string(),
            bytes,
        ))
    }

    /// 十六进制截断助手。
    pub fn hex_preview(payload: &[u8], limit: usize) -> (String, bool) {
        let truncated = payload.len() > limit;
        let slice = &payload[..payload.len().min(limit)];
        (hex::encode(slice), truncated)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(mut w) = self.writer.take() {
            let _ = w.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(src: &str) -> CaptureEntry {
        CaptureEntry {
            ts_ms: 1,
            dir: "c2s",
            src: src.to_string(),
            dst: "10.0.0.2:2025".into(),
            session: 1,
            len: 12,
            transport: "plain".into(),
            hex: "010203".into(),
            truncated: false,
            framing: Some("gate_passed".into()),
        }
    }

    #[test]
    fn anonymisation_is_stable_and_hides_the_address() {
        let a = anonymize("192.168.1.23:2025");
        let b = anonymize("192.168.1.23:2025");
        assert_eq!(a, b);
        assert!(a.starts_with("h:"));
        assert!(!a.contains("192.168"));
        assert_ne!(a, anonymize("192.168.1.24:2025"));
    }

    #[test]
    fn ring_buffer_is_bounded_and_newest_first_in_preview() {
        let mut c = Capture::new(CaptureConfig { ring_capacity: 3, ..Default::default() });
        for i in 0..5 {
            let mut e = entry("1.1.1.1:1");
            e.ts_ms = i;
            c.note(e);
        }
        assert_eq!(c.ring_len(), 3);
        let p = c.preview(10);
        assert_eq!(p[0].ts_ms, 4);
    }

    #[test]
    fn nothing_is_written_when_disabled() {
        let dir = std::env::temp_dir().join(format!("battlecap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = Capture::new(CaptureConfig::default());
        c.note(entry("1.1.1.1:1"));
        assert!(!c.enabled());
        assert!(c.download().is_none());
        assert_eq!(c.retained_bytes(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_writes_ndjson_and_stop_flushes() {
        let dir = std::env::temp_dir().join(format!("battlecap2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = Capture::new(CaptureConfig::default());
        let path = c.start(&dir, 1_000).unwrap();
        assert!(path.file_name().unwrap().to_string_lossy().starts_with("battle-full-capture-"));
        c.note(entry("192.168.1.23:2025"));
        c.note(entry("192.168.1.23:2025"));
        c.stop();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(content.contains("\"dir\":\"c2s\""));
        assert!(!content.contains("192.168.1.23"), "endpoints must be anonymised on disk");
        let (name, bytes) = c.download().unwrap();
        assert!(name.starts_with("battle-full-capture-"));
        assert_eq!(bytes.len(), content.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_stop_by_size_and_by_time() {
        let dir = std::env::temp_dir().join(format!("battlecap3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = Capture::new(CaptureConfig {
            max_capture_bytes: 1,
            max_capture_seconds: 5,
            ..Default::default()
        });
        let _ = c.start(&dir, 0).unwrap();
        c.note(entry("1.1.1.1:1"));
        assert!(c.should_auto_stop(0));
        let mut c2 = Capture::new(CaptureConfig { max_capture_seconds: 5, ..Default::default() });
        let _ = c2.start(&dir, 0).unwrap();
        assert!(!c2.should_auto_stop(1_000));
        assert!(c2.should_auto_stop(6_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_preview_truncates() {
        let (h, t) = Capture::hex_preview(&[0xAB; 100], 16);
        assert_eq!(h.len(), 32);
        assert!(t);
        let (h2, t2) = Capture::hex_preview(&[0xAB; 4], 16);
        assert_eq!(h2.len(), 8);
        assert!(!t2);
    }
}
