//! `battle_receiver` — 在 PC 上跑完整接收器（socks5 代理 + 雷达网页 + 解析引擎）。
//!
//! 为什么要有它：iOS 包必须用 macOS 编，但**协议标定不需要 iPhone**。核心是纯 Rust、
//! 跨平台，所以在这台 PC 上就能跑起和手机上完全一样的那套东西：
//!
//! ```text
//!   B 机（跑游戏）  ──SOCKS5+UDP──▶  本机 battle_receiver  ──▶ 解析 ──▶ 雷达网页
//! ```
//!
//! 于是流程变成：PC 上先跑通、先把 `ProtocolProfile` 标定好（见 battle_replay），
//! 之后再去 Mac 编 iOS 包 —— 一次成功的概率高得多。
//!
//! 用法：
//! ```text
//! scripts\rust.cmd run --bin battle_receiver                    # 默认端口 2025，自动挑
//! scripts\rust.cmd run --bin battle_receiver -- --port-range 30000-30010
//! scripts\rust.cmd run --bin battle_receiver -- --capture       # 启动即开始抓包
//! scripts\rust.cmd run --bin battle_receiver -- --token mytoken # 固定 admin token
//! ```
//! 启动后它会打印：SOCKS5 地址、雷达网址、admin token，以及本机局域网 IP。
//!
//! 注意：这是**开发/标定工具**，不是给终端用户的产品形态（产品形态是 iOS app）。
//! 它默认非只读（`read_only_radar = false`），方便你在浏览器里直接开关解析/抓包。

use battle_proxy::battle::engine::BattleEngine;
use battle_proxy::config::Config;
use battle_proxy::state::{AppState, private_ipv4_addresses, primary_outbound_ipv4};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    battle_proxy::init_runtime();

    let mut cfg = Config {
        // PC 上没有沙箱限制，数据放在工作目录下的 .battle 里，方便直接翻文件。
        data_directory: std::path::PathBuf::from(".battle"),
        read_only_radar: false,
        ..Default::default()
    };

    let mut auto_capture = false;
    let mut port_range: Option<(u16, u16)> = None;
    let mut token: Option<String> = None;
    let mut web_root: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                if let Some(p) = args.next().and_then(|v| v.parse::<u16>().ok()) {
                    port_range = Some((p, p));
                }
            }
            "--port-range" => {
                if let Some(v) = args.next() {
                    if let Some((lo, hi)) = v.split_once('-') {
                        if let (Ok(lo), Ok(hi)) = (lo.parse(), hi.parse()) {
                            port_range = Some((lo, hi));
                        }
                    }
                }
            }
            "--capture" => auto_capture = true,
            "--token" => token = args.next(),
            "--data-dir" => {
                if let Some(d) = args.next() {
                    cfg.data_directory = d.into();
                }
            }
            "--web-root" => web_root = args.next(),
            "--iface" => {
                if let Some(i) = args.next() {
                    cfg.endpoint.interface = i;
                }
            }
            "-h" | "--help" => {
                println!(
                    "battle_receiver — PC 上的完整接收器（开发/标定用）\n\
                     \x20 --port N               固定端口\n\
                     \x20 --port-range LO-HI     端口区间（默认 2025-2045）\n\
                     \x20 --capture              启动即开始协议采集（落盘 NDJSON）\n\
                     \x20 --token STR            固定 admin token（默认随机）\n\
                     \x20 --data-dir PATH        数据目录（默认 ./.battle）\n\
                     \x20 --web-root PATH        雷达前端目录（默认自动找 ../web）\n\
                     \x20 --iface IP             监听网卡（默认 0.0.0.0）\n"
                );
                return Ok(());
            }
            other => eprintln!("忽略未知参数：{other}（--help 看用法）"),
        }
    }

    if let Some((lo, hi)) = port_range {
        cfg.endpoint.ports.range = [lo, hi];
    }
    if let Some(t) = token {
        cfg.admin_token = t;
    }
    cfg.ensure_data_directory()?;

    // 前端目录：优先命令行，其次环境变量，最后交给 embed 自动解析。
    if let Some(root) = web_root {
        battle_proxy::web::embed::set_web_root(root);
    }
    if !battle_proxy::web::embed::web_root_is_usable() {
        eprintln!(
            "警告：在 {} 里没找到 index.html，雷达页会 404。\n\
             提示：cd 到仓库根目录再跑，或用 --web-root 指定 web/ 目录。",
            battle_proxy::web::embed::web_root()
        );
    }

    let state = AppState::new(&cfg, random_token());
    state.set_phase(battle_proxy::state::Phase::Preflight);

    let engine = BattleEngine::new(state.clone(), cfg.clone());

    // 先绑端口，再定状态（与 iOS 侧同一条路径：端口全占就失败）。
    let listener = battle_proxy::socks5::run(state.clone(), engine.clone(), cfg.clone()).await?;
    let port = listener.port();
    state.set_ports(port, port);

    // 局域网展示地址
    let lan = primary_outbound_ipv4()
        .filter(battle_proxy::state::is_private_ipv4)
        .or_else(|| private_ipv4_addresses().into_iter().next());
    if let Some(ip) = lan {
        state.set_display_address(ip);
    }

    let web = battle_proxy::web::serve(state.clone(), engine.clone(), cfg.clone()).await?;
    state.set_phase(battle_proxy::state::Phase::Running);

    if auto_capture {
        state.set_capture(true).await;
        let now = battle_proxy::battle::parse_queue::now_ms();
        match engine.capture().start(now) {
            Ok((full, parsed)) => println!(
                "采集已开启：\n   全量 {}\n   解析 {}",
                full.display(),
                parsed.display()
            ),
            Err(e) => eprintln!("采集开启失败：{e}"),
        }
    }

    let addr = match state.display_address() {
        Some(ip) => ip.to_string(),
        None => "127.0.0.1".to_string(),
    };
    println!("\n=== battle_receiver 已启动 ===");
    println!("  SOCKS5（给 B 机填）: socks5://{addr}:{port}   <- 必须同时开 UDP 转发");
    println!("  雷达页面:            http://127.0.0.1:{port}/battle.html?brand={}", cfg.brand);
    println!("  局域网雷达页:        http://{addr}:{port}/battle.html?brand={}", cfg.brand);
    println!("  Hiddify 配置:        http://{addr}:{port}/api/socks5/hiddify.json");
    println!("  状态 JSON:           http://127.0.0.1:{port}/api/status");
    println!("  admin token:         {}", state.admin_token());
    println!("  监听:                {}", listener.local_addr());
    println!("  数据目录:            {}", cfg.data_directory.display());
    println!("  web 根目录:          {}", battle_proxy::web::embed::web_root());
    println!("\n按 Ctrl+C 停止。");

    // 每 5 秒打一行状态，方便盯着看有没有流量进来。
    let status_state = state.clone();
    let status_engine = engine.clone();
    let status_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tick.tick().await;
            let c = status_state.counters().await;
            let entities: u64 = status_engine
                .telemetry()
                .iter()
                .map(|t| t["entities"].as_u64().unwrap_or(0))
                .sum();
            println!(
                "[{:>5}s] 会话 {}  UDP↑{} ↓{} 无效 {}  已解析 {}  实体 {}  {}",
                status_state.uptime_ms() / 1000,
                c.active_sessions,
                c.udp_packets_up,
                c.udp_packets_down,
                c.udp_invalid_packets,
                c.parsed_packets,
                entities,
                if status_engine.decode_gate_open() {
                    "解码闸门已开（正在收到复制数据）"
                } else {
                    "尚未解出复制数据（B 机还没进对局？UDP 转发没开？）"
                }
            );
        }
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\n收到 Ctrl+C，正在停止…");
        }
        _ = async {
            let mut watch = state.shutdown_receiver();
            let _ = watch.changed().await;
        } => {
            println!("\n收到 shutdown 请求，正在停止…");
        }
    }

    status_task.abort();
    if state.capture_enabled() {
        engine.capture().stop(Some("process exit".to_string()));
    }
    let _ = web.shutdown().await;
    let _ = listener.shutdown().await;
    state.set_phase(battle_proxy::state::Phase::Idle);
    println!("已停止。");
    Ok(())
}

fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}
