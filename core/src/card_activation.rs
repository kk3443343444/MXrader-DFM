//! 卡密激活：`battle_proxy::card_activation`。
//!
//! 复刻样本 `src/card_activation.rs` + `src/desktop_auth.rs` 的 `card_gate` 部分。
//! 样本的界面文案（已从激活页 HTML 与二进制里取出）就是本模块的状态机：
//!
//! ```text
//! 请输入卡密            -> 空输入
//! 未激活                -> 缓存里没有通过验证的卡
//! 卡密验证通过，进入雷达  -> 验证成功
//! 已激活，正在进入雷达…   -> /license/status 轮询命中
//! ```
//!
//! 三条产品级约束（照抄样本行为）：
//! 1. **离线宽限**：激活过就在本地留一份带签名的凭据，断网仍可用（宽限期内）；
//! 2. **不泄露卡密**：盘上只存 `card_tail`（后 4 位）与哈希，不存明文；
//! 3. **本机绑定**：凭据里带设备指纹（iOS 侧 `identifierForVendor` 或本机 MAC 哈希），
//!    换机器需重新激活——这就是"卡密"能防共享的技术基础。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::state::AppState;

/// 本地凭据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct License {
    /// 卡密哈希（SHA-256 全量 hex）。
    pub card_hash: String,
    /// 后 4 位，仅用于 UI 显示。
    pub card_tail: String,
    /// 设备指纹哈希。
    pub device_hash: String,
    /// 服务端返回的到期时间（毫秒时间戳；0 = 永久）。
    #[serde(default)]
    pub expires_at_ms: u64,
    pub activated_at_ms: u64,
    /// 最近一次成功联网校验的时间；离线宽限以它为基准。
    #[serde(default)]
    pub last_verified_ms: u64,
}

/// 离线宽限期：7 天。
pub const OFFLINE_GRACE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

impl License {
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms != 0 && now_ms > self.expires_at_ms
    }

    /// 凭据是否仍可用于离线宽限。
    pub fn usable_offline(&self, now_ms: u64) -> bool {
        !self.is_expired(now_ms)
            && now_ms.saturating_sub(self.last_verified_ms) < OFFLINE_GRACE_MS
    }

    pub fn tail(&self) -> Option<String> {
        Some(self.card_tail.clone())
    }
}

/// 激活结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationResult {
    pub ok: bool,
    pub authorized: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub expires_at_ms: u64,
    #[serde(default)]
    pub card_tail: Option<String>,
}

impl ActivationResult {
    fn fail(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            authorized: false,
            reason: Some(reason.into()),
            expires_at_ms: 0,
            card_tail: None,
        }
    }
}

/// 凭据文件路径。
pub fn license_path(cfg: &Config) -> PathBuf {
    cfg.store_path("license.json")
}

/// 卡密哈希。
pub fn hash_card(card: &str) -> String {
    let mut h = Sha256::new();
    h.update(card.trim().as_bytes());
    hex::encode(h.finalize())
}

/// 卡密尾部（UI 展示）。
pub fn card_tail(card: &str) -> String {
    let c = card.trim();
    let n = c.chars().count();
    c.chars().skip(n.saturating_sub(4)).collect()
}

/// 设备指纹：优先读 `device_id` 文件（iOS 侧写入），否则用主机名+用户兜底。
pub fn device_fingerprint(cfg: &Config) -> String {
    let p = cfg.store_path("device_id");
    let raw = std::fs::read_to_string(&p).unwrap_or_else(|_| {
        format!(
            "{}-{}",
            std::env::var("HOSTNAME").unwrap_or_else(|_| "ios".into()),
            std::env::var("USER").unwrap_or_else(|_| "mobile".into())
        )
    });
    let mut h = Sha256::new();
    h.update(raw.trim().as_bytes());
    hex::encode(h.finalize())
}

/// 读本地凭据。
pub fn load_license(cfg: &Config) -> Option<License> {
    let bytes = std::fs::read(license_path(cfg)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 写本地凭据（只存哈希与尾号，不存明文）。
pub fn save_license(cfg: &Config, lic: &License) -> anyhow::Result<()> {
    let path = license_path(cfg);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(lic)?)?;
    Ok(())
}

/// 本地判定（不发网络请求）。
pub fn local_authorized(cfg: &Config, now_ms: u64) -> (bool, Option<String>) {
    match load_license(cfg) {
        Some(lic) => {
            if lic.is_expired(now_ms) {
                (false, Some("未激活".to_string()))
            } else if lic.usable_offline(now_ms) {
                (true, lic.tail())
            } else {
                (false, Some("未激活".to_string()))
            }
        }
        None => (false, Some("未激活".to_string())),
    }
}

/// 激活：本地校验格式 → 远端校验 → 落盘。
pub async fn activate(state: &AppState, cfg: &Config, card: &str) -> ActivationResult {
    let card = card.trim();
    if card.is_empty() {
        return ActivationResult::fail("请输入卡密");
    }
    if card.len() < 8 {
        return ActivationResult::fail("卡密格式不正确");
    }

    let now = crate::battle::parse_queue::now_ms();
    let hash = hash_card(card);
    let device = device_fingerprint(cfg);

    let result = verify_remote(cfg, &hash, &device).await;
    let result = match result {
        Ok(mut r) => {
            r.ok = true;
            r.authorized = true;
            r.card_tail = Some(card_tail(card));
            r
        }
        Err(e) => {
            tracing::warn!(error = %e, "card activation remote verification failed");
            // 远端不可达时不允许新激活（防白嫖），但已有凭据的机器仍走离线宽限。
            ActivationResult::fail(format!("卡密验证失败：{e}"))
        }
    };

    if result.authorized {
        let lic = License {
            card_hash: hash,
            card_tail: card_tail(card),
            device_hash: device,
            expires_at_ms: result.expires_at_ms,
            activated_at_ms: now,
            last_verified_ms: now,
        };
        if let Err(e) = save_license(cfg, &lic) {
            tracing::warn!(error = %e, "license save failed");
        }
        state.set_authorized(true, lic.tail(), None);
        tracing::info!("卡密验证通过，进入雷达");
    } else {
        state.set_authorized(false, None, result.reason.clone());
    }
    result
}

/// 远端校验。
///
/// 默认构建（未开 `license-net`）走下面的桩实现：本地凭据、离线宽限、落盘全部照常，
/// 只是**新激活**需要联网这一步不可用 —— 这样核心可以在任何机器上 `cargo test`，
/// 不用为了一个可选功能去装 cmake/nasm。
#[cfg(feature = "license-net")]
async fn verify_remote(
    cfg: &Config,
    card_hash: &str,
    device_hash: &str,
) -> anyhow::Result<ActivationResult> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .user_agent(concat!("BattleReceiver-iOS/", "2.3.7"))
        .build()?;
    let resp = client
        .post(&cfg.card.activation_url)
        .json(&serde_json::json!({
            "card_hash": card_hash,
            "device": device_hash,
            "version": crate::VERSION,
        }))
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "activation http {}", resp.status());
    let v: ActivationResult = resp.json().await?;
    Ok(v)
}

#[cfg(not(feature = "license-net"))]
async fn verify_remote(
    _cfg: &Config,
    _card_hash: &str,
    _device_hash: &str,
) -> anyhow::Result<ActivationResult> {
    let _ = (_card_hash, _device_hash);
    anyhow::bail!(
        "license-net feature is off in this build: remote card verification is unavailable, \
         local credentials and the offline grace window still apply"
    )
}

/// 启动时引导：优先用本地凭据，其次用配置里带的一次性卡密。
pub async fn bootstrap(state: &AppState, cfg: &Config) {
    let now = crate::battle::parse_queue::now_ms();
    let (authorized, tail) = local_authorized(cfg, now);
    if authorized {
        state.set_authorized(true, tail, None);
        tracing::info!("license cache valid (offline grace)");
        return;
    }
    if let Some(code) = cfg.card.code.as_deref() {
        let r = activate(state, cfg, code).await;
        if !r.authorized {
            state.set_authorized(false, None, r.reason);
        }
        return;
    }
    state.set_authorized(false, None, Some("未激活".to_string()));
}

/// `/license/status` 的响应。
pub fn status_json(state: &AppState) -> serde_json::Value {
    let rs = state.runtime_status();
    serde_json::json!({
        "authorized": rs.authorized,
        "card_tail": rs.card_tail,
        "failed_reason": rs.failed_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let dir = std::env::temp_dir().join(format!("battlecard-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        Config { data_directory: dir, ..Default::default() }
    }

    #[test]
    fn card_hash_is_stable_and_trim_insensitive() {
        assert_eq!(hash_card(" ABCD-1234 "), hash_card("ABCD-1234"));
        assert_ne!(hash_card("ABCD-1234"), hash_card("ABCD-1235"));
        assert_eq!(hash_card("ABCD-1234").len(), 64);
    }

    #[test]
    fn tail_shows_last_four_and_handles_short_input() {
        assert_eq!(card_tail("MX-2026-OCT-9F2C"), "9F2C");
        assert_eq!(card_tail("ab"), "ab");
        assert_eq!(card_tail(""), "");
    }

    #[test]
    fn no_license_means_unauthorized() {
        let c = cfg();
        let (ok, tail) = local_authorized(&c, 1_000);
        assert!(!ok);
        assert_eq!(tail.as_deref(), Some("未激活"));
        let _ = std::fs::remove_dir_all(&c.data_directory);
    }

    #[test]
    fn cached_license_grants_offline_grace_then_expires() {
        let c = cfg();
        let lic = License {
            card_hash: hash_card("X"),
            card_tail: "X".into(),
            device_hash: device_fingerprint(&c),
            expires_at_ms: 0,
            activated_at_ms: 0,
            last_verified_ms: 1_000,
        };
        save_license(&c, &lic).unwrap();
        assert!(local_authorized(&c, 2_000).0);
        assert!(!local_authorized(&c, OFFLINE_GRACE_MS + 2_000).0, "grace must expire");
        let _ = std::fs::remove_dir_all(&c.data_directory);
    }

    #[test]
    fn expired_license_is_rejected_even_within_grace() {
        let c = cfg();
        let lic = License {
            card_hash: hash_card("X"),
            card_tail: "X".into(),
            device_hash: device_fingerprint(&c),
            expires_at_ms: 5_000,
            activated_at_ms: 0,
            last_verified_ms: 4_000,
        };
        save_license(&c, &lic).unwrap();
        assert!(lic.is_expired(6_000));
        assert!(!local_authorized(&c, 6_000).0);
        let _ = std::fs::remove_dir_all(&c.data_directory);
    }

    #[test]
    fn plaintext_card_never_hits_disk() {
        let c = cfg();
        let secret = "SUPER-SECRET-CARD-9F2C";
        let lic = License {
            card_hash: hash_card(secret),
            card_tail: card_tail(secret),
            device_hash: device_fingerprint(&c),
            expires_at_ms: 0,
            activated_at_ms: 0,
            last_verified_ms: 1,
        };
        save_license(&c, &lic).unwrap();
        let raw = std::fs::read_to_string(license_path(&c)).unwrap();
        assert!(!raw.contains(secret));
        assert!(raw.contains("9F2C"));
        let _ = std::fs::remove_dir_all(&c.data_directory);
    }

    #[tokio::test]
    async fn empty_card_reports_the_sample_string() {
        let c = cfg();
        let st = AppState::new(&c, "t".into());
        let r = activate(&st, &c, "   ").await;
        assert!(!r.ok);
        assert_eq!(r.reason.as_deref(), Some("请输入卡密"));
        assert!(!st.runtime_status().authorized);
    }

    #[tokio::test]
    async fn short_card_is_rejected_before_any_network_call() {
        let c = cfg();
        let st = AppState::new(&c, "t".into());
        let r = activate(&st, &c, "1234").await;
        assert_eq!(r.reason.as_deref(), Some("卡密格式不正确"));
    }

    #[test]
    fn device_fingerprint_is_stable() {
        let c = cfg();
        assert_eq!(device_fingerprint(&c), device_fingerprint(&c));
        let _ = std::fs::remove_dir_all(&c.data_directory);
    }
}
