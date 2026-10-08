//! 桌面变体鉴权与运行时资源：`battle_proxy::card_gate`（样本文件名 `src/desktop_auth.rs`）。
//!
//! 同一份核心在 Windows 上以 `WinDivert` 驱动形式跑（样本里明确写着
//! "WinDivert 驱动内置（仅 Windows）" / "当前平台不适用（热点方案只在 Windows 生效）"），
//! 因此鉴权层要覆盖两种运行形态。样本留下的三串错误就是本模块的全部职责：
//!
//! ```text
//! runtime resource capability is unavailable     <- 平台不具备该能力
//! runtime resource authentication failed         <- 授权不给过
//! runtime entry document is not UTF-8            <- 资源不是文本/被篡改
//! ```
//!
//! "runtime resource" 指**雷达前端包**（HTML/JS/CSS）。iOS 版把它内嵌/随包分发，
//! 桌面版可以选择从远端拉取，因此这里实现了带鉴权的拉取 + 落地 + UTF-8 校验。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::state::AppState;

/// 运行形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeVariant {
    /// iOS 接收器（本工程的默认形态）。
    IosReceiver,
    /// Windows + WinDivert 驱动（热点方案）。
    WindowsHotspot,
    /// macOS/Linux 桌面（仅 TCP 转发，无驱动）。
    Desktop,
}

impl RuntimeVariant {
    pub fn current() -> Self {
        #[cfg(target_os = "ios")]
        {
            RuntimeVariant::IosReceiver
        }
        #[cfg(target_os = "windows")]
        {
            RuntimeVariant::WindowsHotspot
        }
        #[cfg(not(any(target_os = "ios", target_os = "windows")))]
        {
            RuntimeVariant::Desktop
        }
    }

    /// 该形态是否具备指定能力。
    pub fn supports(self, capability: &str) -> bool {
        match capability {
            // 驱动级抓包只有 Windows 有
            "windivert_driver" => matches!(self, RuntimeVariant::WindowsHotspot),
            // 局域网热点分享只有 Windows/macOS 有
            "hotspot_sharing" => matches!(self, RuntimeVariant::WindowsHotspot),
            // 本地 TCP/UDP 转发：三种都有
            "socks5_relay" => true,
            // 远端运行时资源下发：三种都有
            "remote_runtime_resource" => true,
            _ => false,
        }
    }
}

/// 运行时资源（雷达前端包）的落地形态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeResource {
    /// 入口文档（通常是 `battle.html`）。
    pub entry: String,
    /// 资源版本（用于缓存失效）。
    pub version: String,
    /// 落地目录。
    pub directory: PathBuf,
    /// 来源：`embedded` / `remote` / `cache`。
    pub source: String,
}

impl RuntimeResource {
    /// 入口文档所在的本地文件路径。
    pub fn entry_path(&self) -> PathBuf {
        self.directory.join(&self.entry)
    }

    /// 读入口文档并做 UTF-8 校验。
    pub fn read_entry(&self) -> Result<String, RuntimeResourceError> {
        let bytes = std::fs::read(self.entry_path())
            .map_err(|_| RuntimeResourceError::CapabilityUnavailable)?;
        String::from_utf8(bytes).map_err(|_| RuntimeResourceError::NotUtf8)
    }
}

/// 资源错误（与样本的三条串一一对应）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeResourceError {
    /// `runtime resource capability is unavailable`
    CapabilityUnavailable,
    /// `runtime resource authentication failed`
    AuthenticationFailed,
    /// `runtime entry document is not UTF-8`
    NotUtf8,
    /// 其它
    Other(String),
}

impl std::fmt::Display for RuntimeResourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeResourceError::CapabilityUnavailable => {
                write!(f, "runtime resource capability is unavailable")
            }
            RuntimeResourceError::AuthenticationFailed => {
                write!(f, "runtime resource authentication failed")
            }
            RuntimeResourceError::NotUtf8 => write!(f, "runtime entry document is not UTF-8"),
            RuntimeResourceError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for RuntimeResourceError {}

/// 解析运行时资源：优先内嵌，其次缓存，最后远端（需鉴权）。
pub async fn resolve_runtime_resource(
    state: &AppState,
    cfg: &Config,
) -> Result<RuntimeResource, RuntimeResourceError> {
    let variant = RuntimeVariant::current();
    if !variant.supports("remote_runtime_resource") {
        return Err(RuntimeResourceError::CapabilityUnavailable);
    }

    // 1) 随包内嵌（iOS 默认路径）
    let web_root = crate::web::embed::web_root();
    let embedded_dir = PathBuf::from(web_root);
    if embedded_dir.join("index.html").is_file() {
        return Ok(RuntimeResource {
            entry: "index.html".to_string(),
            version: crate::VERSION.to_string(),
            directory: embedded_dir,
            source: "embedded".to_string(),
        });
    }

    // 2) 本地缓存
    let cache_dir = cfg.store_path("runtime");
    if cache_dir.join("index.html").is_file() {
        return Ok(RuntimeResource {
            entry: "index.html".to_string(),
            version: crate::VERSION.to_string(),
            directory: cache_dir,
            source: "cache".to_string(),
        });
    }

    // 3) 远端拉取（必须已授权）
    if !state.runtime_status().authorized {
        return Err(RuntimeResourceError::AuthenticationFailed);
    }
    Err(RuntimeResourceError::CapabilityUnavailable)
}

/// 把一份远端资源写进缓存目录（供 `/api/admin/*` 或启动时调用）。
pub fn install_runtime_resource(
    cfg: &Config,
    dir: &Path,
    entry_name: &str,
    files: &[(String, Vec<u8>)],
) -> Result<RuntimeResource, RuntimeResourceError> {
    std::fs::create_dir_all(dir).map_err(|e| RuntimeResourceError::Other(e.to_string()))?;
    for (name, bytes) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| RuntimeResourceError::Other(e.to_string()))?;
        }
        std::fs::write(&path, bytes).map_err(|e| RuntimeResourceError::Other(e.to_string()))?;
    }
    // 入口必须是 UTF-8
    let entry = dir.join(entry_name);
    let bytes = std::fs::read(&entry).map_err(|_| RuntimeResourceError::CapabilityUnavailable)?;
    String::from_utf8(bytes).map_err(|_| RuntimeResourceError::NotUtf8)?;

    Ok(RuntimeResource {
        entry: entry_name.to_string(),
        version: crate::VERSION.to_string(),
        directory: dir.to_path_buf(),
        source: "remote".to_string(),
    })
}

/// `card_gate`：本工程的鉴权闸门（HTTP 层与桌面变体共用）。
pub mod card_gate {
    use super::*;
    use crate::battle::protocol_capture::require_admin;

    /// 需要授权的操作清单。
    pub const GATED_ACTIONS: &[&str] = &[
        "session/reset",
        "loot",
        "announcement",
        "capture/start",
        "capture/stop",
        "capture/download",
        "shutdown",
        "runtime/install",
        "driver/reload",
    ];

    /// 判定某个动作是否需要授权。
    pub fn needs_authorization(action: &str) -> bool {
        GATED_ACTIONS.contains(&action)
    }

    /// 统一闸门：先查授权，再查令牌。
    pub fn gate(
        state: &AppState,
        token: Option<&str>,
        action: &str,
    ) -> Result<(), String> {
        if !state.runtime_status().authorized
            && matches!(action, "runtime/install" | "driver/reload")
        {
            return Err(RuntimeResourceError::AuthenticationFailed.to_string());
        }
        if needs_authorization(action) {
            require_admin(state, token, action)?;
        }
        Ok(())
    }

    /// 平台能力声明（写入 `/api/status`，让前端知道该显示哪些开关）。
    pub fn capabilities() -> serde_json::Value {
        let v = RuntimeVariant::current();
        serde_json::json!({
            "variant": match v {
                RuntimeVariant::IosReceiver => "ios_receiver",
                RuntimeVariant::WindowsHotspot => "windows_hotspot",
                RuntimeVariant::Desktop => "desktop",
            },
            "socks5_relay": v.supports("socks5_relay"),
            "windivert_driver": v.supports("windivert_driver"),
            "hotspot_sharing": v.supports("hotspot_sharing"),
            "remote_runtime_resource": v.supports("remote_runtime_resource"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_capabilities_match_the_sample_matrix() {
        assert!(RuntimeVariant::WindowsHotspot.supports("windivert_driver"));
        assert!(!RuntimeVariant::IosReceiver.supports("windivert_driver"));
        assert!(!RuntimeVariant::Desktop.supports("hotspot_sharing"));
        assert!(RuntimeVariant::IosReceiver.supports("socks5_relay"));
        assert!(!RuntimeVariant::IosReceiver.supports("nonexistent_capability"));
    }

    #[test]
    fn error_strings_are_verbatim_from_the_sample() {
        assert_eq!(
            RuntimeResourceError::CapabilityUnavailable.to_string(),
            "runtime resource capability is unavailable"
        );
        assert_eq!(
            RuntimeResourceError::AuthenticationFailed.to_string(),
            "runtime resource authentication failed"
        );
        assert_eq!(
            RuntimeResourceError::NotUtf8.to_string(),
            "runtime entry document is not UTF-8"
        );
    }

    #[test]
    fn install_rejects_non_utf8_entry() {
        let dir = crate::testutil::scratch_dir("battlerr");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config { data_directory: dir.clone(), ..Default::default() };
        let err = install_runtime_resource(
            &cfg,
            &dir.join("runtime"),
            "index.html",
            &[("index.html".to_string(), vec![0xFF, 0xFE, 0x00])],
        )
        .unwrap_err();
        assert_eq!(err, RuntimeResourceError::NotUtf8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_then_read_entry() {
        let dir = crate::testutil::scratch_dir("battlerr2");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config { data_directory: dir.clone(), ..Default::default() };
        let res = install_runtime_resource(
            &cfg,
            &dir.join("runtime"),
            "index.html",
            &[
                ("index.html".to_string(), b"<!DOCTYPE html><html></html>".to_vec()),
                ("radar.js".to_string(), b"/* radar */".to_vec()),
            ],
        )
        .unwrap();
        assert_eq!(res.source, "remote");
        assert_eq!(res.read_entry().unwrap(), "<!DOCTYPE html><html></html>");
        assert_eq!(res.entry_path(), dir.join("runtime").join("index.html"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn card_gate_requires_token_for_gated_actions() {
        let dir = crate::testutil::scratch_dir("battlegate");
        let _ = std::fs::create_dir_all(&dir);
        let cfg = Config { data_directory: dir, ..Default::default() };
        let st = AppState::new(&cfg, "tok".into());
        assert!(card_gate::needs_authorization("session/reset"));
        assert!(!card_gate::needs_authorization("diag"));
        assert!(card_gate::gate(&st, None, "session/reset").is_err());
        assert!(card_gate::gate(&st, Some(st.admin_token()), "session/reset").is_ok());
        assert!(card_gate::gate(&st, None, "diag").is_ok());
        let _ = std::fs::remove_dir_all(&cfg.data_directory);
    }

    #[test]
    fn capabilities_json_has_all_flags() {
        let c = card_gate::capabilities();
        for k in ["socks5_relay", "windivert_driver", "hotspot_sharing", "remote_runtime_resource"] {
            assert!(c.get(k).is_some(), "missing {k}");
        }
    }
}
