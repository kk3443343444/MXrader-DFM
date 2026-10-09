//! # battle_proxy — 三角洲行动手游 局域网雷达接收核心
//!
//! 结构复刻自样本 `iPhone-MXrader-r39-3D转向修正.ipa`（`BattleReceiverOpen`，r39）。
//! 样本主程序为单一 arm64 MH_EXECUTE，Swift + 静态链接的 Rust crate `battle_proxy`
//! 共存于一个二进制中（无 Frameworks/ 目录、无独立 dylib）。
//!
//! 数据链路：
//!
//! ```text
//!  B 机（跑游戏）                 A 机（本 app，MXrader）
//!  ┌───────────┐   SOCKS5/小火箭  ┌────────────────────────────────────┐
//!  │ 三角洲行动 │ ───────────────▶ │ socks5::run   (TCP CONNECT +      │
//!  │  (UE5)    │  TCP + UDP 2025  │               UDP ASSOCIATE)      │
//!  └───────────┘                  │      │ 先转发，后解析 (tap)        │
//!                                 │      ▼                            │
//!                                 │ battle::BattleEngine              │
//!                                 │   transport_crypto (AES+LZ4)      │
//!                                 │   udpxin/*  (UE 包 → 实体位移)     │
//!                                 │   codec/*   (RepLayout → 属性)     │
//!                                 │   combat.rs (武器/开火/击杀链)     │
//!                                 │      │ radar snapshot             │
//!                                 │      ▼                            │
//!                                 │ web::serve  axum: /battle.html    │
//!                                 │   + /ws 状态流 + /api/admin/*     │
//!                                 └────────────────────────────────────┘
//! ```
//!
//! 对外只暴露 `ios_bridge` 里的 C ABI（见 docs/INTERFACES.md §1），Swift 侧零依赖耦合。

pub mod announcement;
pub mod battle;
pub mod card_activation;
/// 目标地址统计（分流规则要的"游戏服务器 IP"从这里来）。
pub mod census;
pub mod config;
pub mod debug_monitor;
pub mod desktop_auth;
pub mod ios_bridge;
pub mod socks5;
pub mod state;
pub mod web;

/// 测试专用工具（只在 `cfg(test)` 下编译）：见文件头对"为什么不用 %TEMP%"的说明。
#[cfg(test)]
pub mod testutil;

/// 版本串：一次构建的唯一标识，**由 `build.rs` 在编译期注入**（见需求「构建版本戳」）。
///
/// 形态（CI）：`2.3.7-a1b2c3d (57)` = `<MARKETING_VERSION> (<run number>)`，与启动页
/// `BattleSplashView` 的 `v\(version) (\(build))` 逐个字符对应；本地构建（没有任何注入）
/// 落回 `2.3.7-r39`。
///
/// 为什么不直接读 `env!("...")`：`env!` 在变量缺失时是编译错误，本机 `cargo test` 与
/// 开发机构建都不带这些变量。`build.rs` 负责算出兜底值，再用 `cargo:rustc-env` 交给
/// rustc，于是这里永远只是一个普通常量。
///
/// 同一份戳会出现在三处，必须互相印证：IPA 资产名 / 启动页文案 / 本常量所在的状态 JSON
/// （`status.version`，以及 C ABI 的 `battle_proxy_version()`）。
pub const VERSION: &str = env!("BATTLE_VERSION");

/// 样本品牌标识（Info.plist 的 `BattleBrandVariant`）。
pub const BRAND_VARIANT: &str = "mx";

/// 运行时全局初始化：日志、panic hook、全局分配器之外的一次性工作。
/// 可重复调用（内部 once）。
pub fn init_runtime() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("battle_proxy=info,tower_http=warn,hyper=warn")
        });
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_target(false)
            .try_init();

        // iOS 上 panic 不能中断整个进程：记录并让当前任务退出。
        std::panic::set_hook(Box::new(|info| {
            tracing::error!(target: "battle_proxy", "panic: {info}");
        }));

        tracing::info!(version = VERSION, brand = BRAND_VARIANT, "battle_proxy runtime initialised");
    });
}

/// 启动参数中所有端口的合法区间（样本：2025–2045 至少一个可用）。
pub const DEFAULT_PORT_RANGE: (u16, u16) = (2025, 2045);
