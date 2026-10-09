//! 构建期版本戳注入（对应需求「构建版本戳」）。
//!
//! 为什么需要 build.rs：`env!("X")` 在变量缺失时是**编译错误**，而开发机（Windows）与
//! `cargo test` 都必须能在不带任何环境变量的情况下构建成功。所以这里把"三次注入"
//! 收敛成一个**一定存在**的编译期常量 `BATTLE_VERSION`：
//!
//! ```text
//!   CI:   BATTLE_BUILD_STAMP="2.3.7-a1b2c3d (57)"  ──┐
//!   本机: (什么都不设)                              ──┴─▶ BATTLE_VERSION
//! ```
//!
//! 解析优先级：
//!   1. `BATTLE_BUILD_STAMP` —— 完整戳，CI 显式注入，优先；
//!   2. `BATTLE_MARKETING_VERSION` + `BATTLE_BUILD_NUMBER` —— 只有零件时拼成
//!      `"<marketing> (<build>)"`（与启动页 `v\(version) (\(build))` 同形）；
//!   3. `FALLBACK_VERSION` —— 本地/测试，保持历史值 `2.3.7-r39`。
//!
//! 关键：CI 的 `core/target` 是**带缓存**的（见 .github/workflows/ios.yml 的 actions/cache）。
//! 不声明 `rerun-if-env-changed` 的话，cargo 会认为 build.rs 的输出仍然新鲜，于是线性库里
//! 留的是上一次的旧戳 —— IPA 名是新戳、状态 JSON 是旧戳，正好破坏"三处互相印证"。

use std::env;

/// 没有任何注入时使用的版本串（与 CFBundleShortVersionString 对齐）。
const FALLBACK_VERSION: &str = "2.3.7-r39";

/// 会被读取、且变化后必须重跑本脚本的环境变量。
const TRACKED_VARS: [&str; 5] = [
    "BATTLE_BUILD_STAMP",
    "BATTLE_MARKETING_VERSION",
    "BATTLE_BUILD_NUMBER",
    "GITHUB_SHA",
    "GITHUB_RUN_NUMBER",
];

fn main() {
    for key in TRACKED_VARS {
        println!("cargo:rerun-if-env-changed={key}");
    }
    // 源码/脚本本身变化时也要重跑（cargo 默认行为，显式写出来免得以后被删掉）。
    println!("cargo:rerun-if-changed=build.rs");

    let stamp = version_stamp();
    println!("cargo:rustc-env=BATTLE_VERSION={stamp}");
    println!("cargo:rustc-env=BATTLE_SHA={}", short_sha());
    // 让 CI 日志里能直接看到注入了什么（cargo 会把 build script 的 stdout 原样打印）。
    println!("cargo:warning=battle_proxy build stamp: {stamp}");
}

/// 干净的变量值：去掉首尾空白与换行（`cargo:rustc-env` 里带换行会直接破坏编译）。
fn env_value(key: &str) -> Option<String> {
    let raw = env::var(key).ok()?;
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() || c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn version_stamp() -> String {
    if let Some(full) = env_value("BATTLE_BUILD_STAMP") {
        return full;
    }

    // 只有零件时自己拼一个与启动页同形的戳：`2.3.7-a1b2c3d (57)`。
    match (env_value("BATTLE_MARKETING_VERSION"), env_value("BATTLE_BUILD_NUMBER")) {
        (Some(marketing), Some(build)) => format!("{marketing} ({build})"),
        (Some(marketing), None) => match env_value("GITHUB_RUN_NUMBER") {
            Some(build) => format!("{marketing} ({build})"),
            None => marketing,
        },
        _ => FALLBACK_VERSION.to_string(),
    }
}

/// 短 SHA：优先 `BATTLE_SHORT_SHA`，否则取 `GITHUB_SHA` 前 7 位，最后落 `local`。
fn short_sha() -> String {
    if let Some(sha) = env_value("BATTLE_SHORT_SHA") {
        return sha;
    }
    match env_value("GITHUB_SHA") {
        Some(sha) => sha.chars().take(7).collect(),
        None => "local".to_string(),
    }
}
