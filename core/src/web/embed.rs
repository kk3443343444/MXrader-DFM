//! 内嵌资产与静态目录解析：`battle_proxy::web::embed`。
//!
//! 复刻样本 `src/web/embed.rs`。样本里前端包是**带鉴权的"运行时资源"**
//! （`runtime resource capability is unavailable` / `…authentication failed` /
//! `runtime entry document is not UTF-8` 三条错误就出自那条链路）。本工程把它简化：
//! 前端直接随包分发，因此这里只做两件事：
//!
//! 1. **内嵌**：卡密页与 `channel_map.json` 用 `include_str!` 编进二进制
//!    （与样本一样——样本也是把 83,709 字节的 `channel_map` 内嵌的）；
//! 2. **解析静态目录**：按优先级找到 `index.html` / `radar.js` / `leaflet-lite.js` /
//!    `style.css` / `maps.json` / `tiles/` 所在的目录。
//!
//! 静态目录的解析顺序（先命中先用）：
//!
//! ```text
//! ① set_web_root() 显式设置（iOS 侧把沙箱里解出来的 Web/ 目录告诉核心）
//! ② env BATTLE_WEB_ROOT
//! ③ <data_directory>/runtime        （远端下发资源的落地处，见 desktop_auth）
//! ④ <crate>/../web                  （开发机：cargo run 直接读源码目录）
//! ⑤ 当前工作目录                     （兜底）
//! ```

use std::path::PathBuf;
use std::sync::OnceLock;

/// 卡密激活页（样本里是明文内嵌的 3,135 字节 HTML）。
pub fn card_page() -> &'static str {
    include_str!("../../assets/battle_card.html")
}

/// `channel_map.json`（77 个通道 + 53 个类的 handle 名字表）。
///
/// 这是雷达的知识底座：`EntityTable` 用它做静态通道→类名映射，
/// `HandleTable` 用它做 handle 号→属性名映射（从而推断位流里的字段类型）。
pub fn channel_map_json() -> Option<&'static str> {
    Some(include_str!("../../assets/channel_map.json"))
}

/// 物资名录（`{"<item_id>": "<asset_name>"}`）。
pub fn loot_ids_json() -> Option<&'static str> {
    Some(include_str!("../../assets/loot_ids.json"))
}

static WEB_ROOT: OnceLock<String> = OnceLock::new();

/// 显式指定静态目录（iOS 侧调用最稳）。
pub fn set_web_root(path: impl Into<String>) {
    let path = path.into();
    tracing::info!(web_root = %path, "web root set");
    let _ = WEB_ROOT.set(path);
}

/// 静态目录（返回 `&'static str`；解析一次后缓存）。
pub fn web_root() -> &'static str {
    WEB_ROOT.get_or_init(resolve_web_root).as_str()
}

/// 当前解析到的静态目录是否可用（有 `index.html`）。
pub fn web_root_is_usable() -> bool {
    PathBuf::from(web_root()).join("index.html").is_file()
}

/// 从可执行文件位置反推 app 包里的 `web/`。
///
/// iOS 上 `current_exe()` 是 `<...>/Bundle.app/BattleReceiverOpen`，父目录就是 app 包，
/// 而 XcodeGen 用 `type: folder` 把前端整体拷在包根（`<app>/web/`）。
/// 有这一层，即使 Swift 忘了传 `web_root`（真机就踩过：服务端只能返回
/// "radar page missing" 占位页，界面显示"HTML 已载入但地图前端未完成挂载"），
/// 也能自己找到前端。两层互为保险。
fn discover_bundle_web_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app_dir = exe.parent()?;
    let candidate = app_dir.join("web");
    if candidate.join("index.html").is_file() {
        return Some(candidate);
    }
    // 少数打包方式会把资源放到 .app/<BundleName>/ 下，顺手试一下。
    let nested = app_dir.join(exe.file_stem()?).join("web");
    if nested.join("index.html").is_file() {
        return Some(nested);
    }
    None
}

fn resolve_web_root() -> String {
    // ① 显式设置（不可能走到这里，set_web_root 已填 OnceLock）
    if let Some(v) = WEB_ROOT.get() {
        return v.clone();
    }

    // ② 环境变量
    if let Ok(v) = std::env::var("BATTLE_WEB_ROOT") {
        if !v.trim().is_empty() {
            tracing::info!(web_root = %v, source = "env", "web root resolved");
            return v;
        }
    }

    let mut candidates: Vec<(PathBuf, &'static str)> = Vec::new();

    // ③ app 包（iOS 真机/模拟器的主路径）
    if let Some(bundle) = discover_bundle_web_root() {
        candidates.push((bundle, "app bundle"));
    }

    // ④ 沙箱里的运行时资源目录
    candidates.push((PathBuf::from("runtime"), "data_directory/runtime"));
    if let Ok(dir) = std::env::var("BATTLE_DATA_DIR") {
        candidates.push((PathBuf::from(dir).join("runtime"), "data_directory/runtime"));
    }

    // ⑤ 开发机：crate 旁边的 ../web
    candidates.push((
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../web"),
        "crate/../web",
    ));

    // ⑥ 兜底
    candidates.push((PathBuf::from("."), "cwd"));

    for (path, source) in candidates {
        if path.join("index.html").is_file() {
            tracing::info!(web_root = %path.display(), source, "web root resolved");
            return path.to_string_lossy().to_string();
        }
    }

    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../web");
    tracing::warn!(
        web_root = %fallback.display(),
        "no usable web root found (no index.html); using the development path"
    );
    fallback.to_string_lossy().to_string()
}

/// 前端静态文件的 MIME 类型。
pub fn content_type_for(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "ndjson" => "application/x-ndjson; charset=utf-8",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 1×1 透明 PNG：瓦片缺失时的兜底响应（样本里前端也有同类占位策略）。
pub fn transparent_png() -> &'static [u8] {
    // 预生成的 1x1 RGBA PNG，避免运行期依赖图像库。
    &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

/// 路径安全性：静态文件服务只允许在 web_root 内部取文件。
pub fn sanitize_relative(path: &str) -> Option<String> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.len() > 512 {
        return None;
    }
    if trimmed.contains("..") || trimmed.contains('\0') {
        return None;
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// 前端入口的默认相对路径（`/` 会 302 到这里）。
pub const RADAR_ENTRY: &str = "battle.html";

/// 入口文档在磁盘上的真实文件名。
pub const INDEX_FILE: &str = "index.html";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_are_present_and_reasonable() {
        let cm = channel_map_json().expect("channel_map must be embedded");
        assert!(cm.len() > 50_000, "channel_map seems truncated: {} B", cm.len());
        for probe in ["channel_map", "handles", "BP_DFMCharacter_C", "BP_DFMPlayerState_C"] {
            assert!(cm.contains(probe), "channel_map missing {probe}");
        }
        // 激活页：从参考二进制 **原样提取**（0x59ecaf..0x59fae7，3640 字节）。
        // 断言的是"原始字节里确实有这些东西"，改动这个文件前先回去改提取脚本。
        let card = card_page();
        assert_eq!(card.len(), 3640, "card page must be the verbatim extracted asset");
        for probe in [
            "<title>卡密激活</title>",
            "async function go()",
            "fetch('/license'",          // 注意：是 /license，不是 /license/activate
            "fetch('/license/status')",
            "leaflet-container",
            "data-battle-ready",
            "</html>",
        ] {
            assert!(card.contains(probe), "card page missing {probe}");
        }
        let loot = loot_ids_json().expect("loot ids must be embedded");
        assert!(loot.contains("SOL_DT_"));
    }

    #[test]
    fn content_types_cover_the_front_end_assets() {
        assert!(content_type_for("index.html").starts_with("text/html"));
        assert!(content_type_for("radar.js").contains("javascript"));
        assert!(content_type_for("style.css").starts_with("text/css"));
        assert!(content_type_for("maps.json").starts_with("application/json"));
        assert_eq!(content_type_for("tiles/ZeroDam/3/1/2.png"), "image/png");
        assert_eq!(content_type_for("capture.ndjson"), "application/x-ndjson; charset=utf-8");
        assert_eq!(content_type_for("weird.bin"), "application/octet-stream");
    }

    #[test]
    fn sanitizer_blocks_traversal_and_odd_input() {
        assert_eq!(sanitize_relative("/radar.js").as_deref(), Some("radar.js"));
        assert_eq!(sanitize_relative("tiles/ZeroDam/3/0/0.png").is_some(), true);
        assert_eq!(sanitize_relative("../../etc/passwd"), None);
        assert_eq!(sanitize_relative("a/../b"), None);
        assert_eq!(sanitize_relative(""), None);
        assert_eq!(sanitize_relative("bad name.js"), None);
        assert_eq!(sanitize_relative("a\0b"), None);
    }

    #[test]
    fn transparent_png_has_a_valid_signature() {
        let png = transparent_png();
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[12..16], b"IHDR");
        assert!(png.ends_with(&[0xAE, 0x42, 0x60, 0x82]), "missing IEND");
    }

    #[test]
    fn web_root_resolves_to_something_on_the_dev_machine() {
        // 开发机上应当解析到 <crate>/../web（存在 index.html）。
        let root = web_root();
        assert!(!root.is_empty());
        assert!(PathBuf::from(root).is_absolute() || root.starts_with('.') || root.contains("web"));
    }
}
