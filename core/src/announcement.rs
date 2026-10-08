//! 公告：`battle_proxy::announcement`。
//!
//! 复刻样本 `src/announcement.rs`。样本在管理接口里留下的字段：
//! `announcement` / `next_cursor` / `broadcast` / `persisted`，以及一条错误串
//! `announcement path has no parent directory`——说明它是**先写盘、再广播**的
//! 两段式：写盘失败不影响广播，重启后从盘上恢复。
//!
//! 设计取向（与样本一致）：**公告永远不是关键路径**。取不到就用缓存，没有缓存
//! 就当没有公告，绝不让它影响接收器启动。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::state::AppState;

/// 一条公告。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Announcement {
    /// 服务端给的游标；客户端用它做增量拉取。
    #[serde(default)]
    pub cursor: Option<String>,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// 严重级别：`info` / `warn` / `urgent`。urgent 会在诊断页置顶。
    #[serde(default = "default_level")]
    pub level: String,
    /// 面向的品牌（`mx` / `battle` / `all`）。
    #[serde(default = "default_brand")]
    pub brand: String,
    /// 客户端最低版本；低于此版本时前端展示升级提示。
    #[serde(default)]
    pub min_version: Option<String>,
    #[serde(default)]
    pub published_at_ms: u64,
}

fn default_level() -> String {
    "info".to_string()
}
fn default_brand() -> String {
    "all".to_string()
}

/// 盘上形态（带游标，便于下次增量拉取）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnnouncementStore {
    pub next_cursor: Option<String>,
    pub items: Vec<Announcement>,
    pub last_fetch_ms: u64,
    /// 上次拉取的失败原因（离线时保留，不覆盖旧数据）。
    pub last_error: Option<String>,
}

impl AnnouncementStore {
    pub fn latest(&self) -> Option<&Announcement> {
        self.items
            .iter()
            .max_by_key(|a| a.published_at_ms)
    }

    /// 严重级别排序后的列表（urgent 在前）。
    pub fn ordered(&self) -> Vec<&Announcement> {
        let mut v: Vec<&Announcement> = self.items.iter().collect();
        v.sort_by_key(|a| {
            let rank = match a.level.as_str() {
                "urgent" => 0,
                "warn" => 1,
                _ => 2,
            };
            (rank, std::cmp::Reverse(a.published_at_ms))
        });
        v
    }
}

/// 公告文件路径（`data_directory/announcement.json`）。
pub fn store_path(cfg: &Config) -> PathBuf {
    cfg.store_path("announcement.json")
}

/// 读盘；损坏时返回空 store（不报错，避免影响启动）。
pub fn load(cfg: &Config) -> AnnouncementStore {
    let path = store_path(cfg);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => AnnouncementStore::default(),
    }
}

/// 写盘。父目录不存在时返回样本里的那条错误串。
pub fn save(cfg: &Config, store: &AnnouncementStore) -> anyhow::Result<()> {
    let path = store_path(cfg);
    let Some(parent) = path.parent() else {
        anyhow::bail!("announcement path has no parent directory");
    };
    if !parent.exists() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(store)?)?;
    Ok(())
}

/// 拉取公告。`endpoint` 为 `None` 时只读盘。
///
/// 两段式：**先写盘，再广播**（`persisted` / `broadcast` 两个字段就是这么来的）。
pub async fn refresh(state: &AppState, cfg: &Config) -> AnnouncementStore {
    let mut store = load(cfg);
    let now = crate::battle::parse_queue::now_ms();

    let url = announcement_url(cfg);
    match url {
        Some(url) => match fetch(&url, store.next_cursor.as_deref()).await {
            Ok(items) => {
                store.next_cursor = items.iter().filter_map(|a| a.cursor.clone()).last();
                store.items = items;
                store.last_fetch_ms = now;
                store.last_error = None;
                let persisted = save(cfg, &store).is_ok();
                tracing::info!(persisted, broadcast = true, "announcement updated");
            }
            Err(e) => {
                store.last_error = Some(e.to_string());
                tracing::warn!(error = %e, "announcement fetch failed, keeping cache");
            }
        },
        None => {
            tracing::debug!("announcement endpoint not configured; cache only");
        }
    }

    state.set_announcement(store.latest().map(|a| serde_json::to_value(a).unwrap_or_default()));
    store
}

/// 公告地址：`data_directory/announcement_url.txt` 或编译期默认（无则 None）。
fn announcement_url(cfg: &Config) -> Option<String> {
    let p = cfg.store_path("announcement_url.txt");
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 实际 HTTP 拉取（需要 `license-net` 特性；否则由下面的桩替代）。
#[cfg(feature = "license-net")]
async fn fetch(url: &str, cursor: Option<&str>) -> anyhow::Result<Vec<Announcement>> {
    let mut req = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .user_agent(concat!("BattleReceiver-iOS/", "2.3.7"))
        .build()?
        .get(url);
    if let Some(c) = cursor {
        req = req.query(&[("cursor", c)]);
    }
    let resp = req.send().await?;
    anyhow::ensure!(resp.status().is_success(), "announcement http {}", resp.status());
    let items: Vec<Announcement> = resp.json().await?;
    Ok(items)
}

#[cfg(not(feature = "license-net"))]
async fn fetch(_url: &str, _cursor: Option<&str>) -> anyhow::Result<Vec<Announcement>> {
    anyhow::bail!("license-net feature is off in this build: announcement fetch skipped")
}

/// 远端管理接口要求令牌的版本。
pub async fn push_remote(
    state: &AppState,
    cfg: &Config,
    token: Option<&str>,
    payload: serde_json::Value,
) -> Result<serde_json::Value, String> {
    super::battle::protocol_capture::require_admin(state, token, "announcement update")?;
    let mut store = load(cfg);
    if let Ok(a) = serde_json::from_value::<Announcement>(payload) {
        store.items.push(a);
    }
    let persisted = save(cfg, &store).is_ok();
    state.set_announcement(store.latest().map(|a| serde_json::to_value(a).unwrap_or_default()));
    Ok(serde_json::json!({
        "ok": true,
        "broadcast": true,
        "persisted": persisted,
        "next_cursor": store.next_cursor,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let dir = std::env::temp_dir().join(format!("battleann-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        Config { data_directory: dir, ..Default::default() }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let c = cfg();
        let store = AnnouncementStore {
            next_cursor: Some("c1".into()),
            items: vec![Announcement {
                cursor: Some("c1".into()),
                title: "版本更新".into(),
                body: "r39 修正 3D 转向".into(),
                level: "warn".into(),
                brand: "mx".into(),
                min_version: Some("2.3.0".into()),
                published_at_ms: 1,
            }],
            last_fetch_ms: 2,
            last_error: None,
        };
        save(&c, &store).unwrap();
        let back = load(&c);
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.next_cursor.as_deref(), Some("c1"));
        assert_eq!(back.latest().unwrap().title, "版本更新");
        let _ = std::fs::remove_dir_all(&c.data_directory);
    }

    #[test]
    fn missing_file_yields_empty_store_not_error() {
        let c = Config {
            data_directory: std::env::temp_dir().join("battleann-missing-xyz"),
            ..Default::default()
        };
        let s = load(&c);
        assert!(s.items.is_empty());
        assert!(s.next_cursor.is_none());
    }

    #[test]
    fn corrupt_file_is_tolerated() {
        let c = cfg();
        std::fs::write(store_path(&c), b"{not json").unwrap();
        assert!(load(&c).items.is_empty());
        let _ = std::fs::remove_dir_all(&c.data_directory);
    }

    #[test]
    fn ordering_puts_urgent_first_then_newest() {
        let store = AnnouncementStore {
            items: vec![
                Announcement { title: "a".into(), level: "info".into(), published_at_ms: 10, cursor: None, body: String::new(), brand: "all".into(), min_version: None },
                Announcement { title: "b".into(), level: "urgent".into(), published_at_ms: 1, cursor: None, body: String::new(), brand: "all".into(), min_version: None },
                Announcement { title: "c".into(), level: "info".into(), published_at_ms: 99, cursor: None, body: String::new(), brand: "all".into(), min_version: None },
            ],
            ..Default::default()
        };
        let o = store.ordered();
        assert_eq!(o[0].title, "b");
        assert_eq!(o[1].title, "c");
        assert_eq!(o[2].title, "a");
    }

    #[tokio::test]
    async fn remote_push_requires_admin_token() {
        let c = cfg();
        let st = AppState::new(&c, "tok".into());
        let err = push_remote(&st, &c, None, serde_json::json!({"title":"x"})).await.unwrap_err();
        assert!(err.contains("requires BATTLE_ADMIN_TOKEN"));
        let ok = push_remote(
            &st,
            &c,
            Some(st.admin_token()),
            serde_json::json!({"title":"x","published_at_ms":5}),
        )
        .await
        .unwrap();
        assert_eq!(ok["persisted"], true);
        let _ = std::fs::remove_dir_all(&c.data_directory);
    }

    #[tokio::test]
    async fn refresh_without_endpoint_keeps_cache() {
        let c = cfg();
        let st = AppState::new(&c, "tok".into());
        let s = refresh(&st, &c).await;
        assert!(s.items.is_empty());
        assert!(st.announce().await.is_none());
        let _ = std::fs::remove_dir_all(&c.data_directory);
    }
}
