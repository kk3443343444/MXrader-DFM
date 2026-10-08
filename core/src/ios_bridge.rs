//! C ABI：`battle_proxy::ios_bridge`。
//!
//! 复刻样本 `src/ios_bridge.rs`。Swift 只认这 7 个符号（见 docs/INTERFACES.md §1
//! 与 `core/include/battle_proxy.h`），因此 Rust 侧的内部重构永远不会影响 iOS 壳。
//!
//! ```c
//! const char *battle_proxy_version(void);
//! char       *battle_proxy_start(const char *config_json);
//! char       *battle_proxy_stop(void);
//! char       *battle_proxy_status(void);
//! char       *battle_proxy_admin(const char *token, const char *path, const char *body);
//! char       *battle_proxy_selftest(void);
//! void        battle_proxy_free_string(char *p);
//! ```
//!
//! 约定：
//! * 返回的 `char*` 都是 `CString::into_raw`，由 `battle_proxy_free_string` 释放；
//! * `start` 在一个后台线程上跑（Swift 已在 `Task.detached` 里调用），内部最多等
//!   5 秒拿到 `running` 或 `failed`，**绝不阻塞 iOS 主线程**；
//! * 任何 panic 都被 `catch_unwind` 兜住并转成 `phase=failed` 的 JSON，
//!   因为 iOS 上 panic 穿过 FFI 边界是未定义行为。

use std::ffi::{c_char, CStr, CString};
use std::sync::{Mutex, OnceLock};

use crate::battle::engine::BattleEngine;
use crate::config::Config;
use crate::state::AppState;

/// 全局单例：一个进程只有一个接收器（样本同样如此——端口区间独占）。
struct Receiver {
    runtime: tokio::runtime::Runtime,
    state: AppState,
    engine: BattleEngine,
    cfg: Config,
}

// SAFETY: 所有跨线程访问都通过 `OnceLock<Mutex<..>>` 串行化；
// 内部的听众（socks5/web）自带同步原语。
unsafe impl Send for Receiver {}
unsafe impl Sync for Receiver {}

static RECEIVER: OnceLock<Mutex<Option<Receiver>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Receiver>> {
    RECEIVER.get_or_init(|| Mutex::new(None))
}

/// 静态版本串（带 NUL，永远有效，不需要释放）。
static VERSION_C: &str = concat!("2.3.7-r39", "\0");

fn to_c_string(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => {
            // 内部字符串里出现 NUL（理论上不可能）：退化成兜底 JSON。
            CString::new("{\"phase\":\"failed\",\"error\":\"internal NUL\"}")
                .map(|c| c.into_raw())
                .unwrap_or(std::ptr::null_mut())
        }
    }
}

unsafe fn cstr_to_str<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    match CStr::from_ptr(p).to_str() {
        Ok(s) => s,
        Err(_) => "",
    }
}

/// 生成 32 位 hex 会话令牌。
fn new_session_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// 找第一个可用端口：TCP 与 UDP 都必须绑得上（雷达同时用两者）。
fn pick_port(cfg: &Config) -> Option<u16> {
    for port in cfg.candidate_ports() {
        let tcp = std::net::TcpListener::bind((cfg.endpoint.interface.as_str(), port));
        if tcp.is_err() {
            continue;
        }
        // TCP 绑定成功即释放，交给 tokio 重新绑（避免 TIME_WAIT 阻塞启动）。
        drop(tcp);
        match std::net::UdpSocket::bind((cfg.endpoint.interface.as_str(), port)) {
            Ok(_) => return Some(port),
            Err(_) => continue,
        }
    }
    None
}

/// 启动接收器。返回 status JSON。
pub fn start_impl(config_json: &str) -> String {
    crate::init_runtime();

    {
        let guard = slot().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = guard.as_ref() {
            if r.state.phase() == crate::state::Phase::Running {
                return serde_json::to_string(&r.state.status_snapshot())
                    .unwrap_or_else(|_| "{\"phase\":\"running\"}".to_string());
            }
        }
    }

    let cfg = match Config::from_json_str(config_json) {
        Ok(c) => c,
        Err(e) => {
            return format!(
                "{{\"phase\":\"failed\",\"error\":{}}}",
                serde_json::to_string(&format!("配置解析失败：{e}")).unwrap_or_default()
            );
        }
    };

    if let Err(e) = cfg.ensure_data_directory() {
        return format!(
            "{{\"phase\":\"failed\",\"error\":{}}}",
            serde_json::to_string(&e.to_string()).unwrap_or_default()
        );
    }

    let state = AppState::new(&cfg, new_session_token());
    state.set_phase(crate::state::Phase::Preflight);

    // 雷达前端目录：iOS 上由 Swift 侧传 app bundle 里的 web/ 绝对路径。
    // 不设的话设备上会解析不到（没有源码目录、也没有工作目录的概念）。
    if let Some(root) = cfg.web_root.as_deref().filter(|r| !r.trim().is_empty()) {
        crate::web::embed::set_web_root(root);
        if crate::web::embed::web_root_is_usable() {
            tracing::info!(web_root = root, "web root set from config");
        } else {
            tracing::warn!(web_root = root, "web root from config has no index.html");
        }
    }

    let Some(port) = pick_port(&cfg) else {
        state.fail(format!(
            "接收端口被其他应用占用（已自动尝试 {}-{}）。请从后台彻底关闭旧版 MXrader/BATTLE，或卸载多余的雷达后，再点「重新启动」。",
            cfg.endpoint.ports.range[0], cfg.endpoint.ports.range[1]
        ));
        return serde_json::to_string(&state.status_snapshot())
            .unwrap_or_else(|_| "{\"phase\":\"failed\"}".to_string());
    };
    state.set_ports(port, port);
    state.set_phase(crate::state::Phase::Starting);

    // 局域网展示地址：优先出口网卡，其次 getifaddrs。
    let display = crate::state::primary_outbound_ipv4()
        .or_else(|| crate::state::private_ipv4_addresses().into_iter().next());
    if let Some(ip) = display {
        if crate::state::is_private_ipv4(&ip) {
            state.set_display_address(ip);
        }
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(2, 4),
        )
        .enable_all()
        .thread_name("battle-proxy")
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            state.fail(format!("Rust 服务启动失败（错误码 {}）：{e}", 1));
            return json_status(&state);
        }
    };

    // BattleEngine::new 内部会 tokio::spawn（解析流水 / 广播 / 清理任务），
    // 必须在 runtime 上下文里构造，否则会 panic。
    let engine = {
        let _guard = runtime.enter();
        BattleEngine::new(state.clone(), cfg.clone())
    };

    // 端口已绑定 → 进入 running（web 与 socks5 在后台真正监听）。
    state.set_phase(crate::state::Phase::Running);
    state.touch_health();

    // 后台拉起 socks5 + web；失败则把 phase 打回 failed（样本同路径）。
    {
        let state2 = state.clone();
        let engine2 = engine.clone();
        let cfg2 = cfg.clone();
        runtime.spawn(async move {
            let socks = crate::socks5::run(state2.clone(), engine2.clone(), cfg2.clone()).await;
            let web = crate::web::serve(state2.clone(), engine2.clone(), cfg2.clone()).await;
            match (socks, web) {
                (Ok(s), Ok(w)) => {
                    tracing::info!(
                        socks = s.port(),
                        web = w.port(),
                        "receiver running: SOCKS5 TCP/UDP + radar web"
                    );
                }
                (Err(e), _) | (_, Err(e)) => {
                    state2.fail(format!("本地服务启动失败，请检查端口是否被占用。原始错误：{e}"));
                }
            }
        });
    }

    // 授权 + 公告（离线可用：失败只记状态，不阻塞）。
    {
        let state2 = state.clone();
        let cfg2 = cfg.clone();
        runtime.spawn(async move {
            crate::card_activation::bootstrap(&state2, &cfg2).await;
            crate::announcement::refresh(&state2, &cfg2).await;
        });
    }

    let status = json_status(&state);
    *slot().lock().unwrap_or_else(|e| e.into_inner()) =
        Some(Receiver { runtime, state, engine, cfg });
    status
}

fn json_status(state: &AppState) -> String {
    serde_json::to_string(&state.status_snapshot()).unwrap_or_else(|_| {
        "{\"phase\":\"failed\",\"error\":\"status serialization failed\",\"socks_port\":0,\"web_port\":0}"
            .to_string()
    })
}

/// 停止接收器。返回最终 status JSON。
pub fn stop_impl() -> String {
    let taken = {
        let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    match taken {
        Some(r) => {
            r.state.set_phase(crate::state::Phase::Stopping);
            let snapshot = json_status(&r.state);
            // 通知所有听众退出，然后给 2 秒收尾。
            r.runtime.block_on(async {
                r.state.request_shutdown().await;
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            });
            r.runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            snapshot
        }
        None => {
            let cfg = Config::default();
            let st = AppState::new(&cfg, String::new());
            json_status(&st)
        }
    }
}

/// 查询状态（不持有 receiver 时返回一个 idle 快照）。
pub fn status_impl() -> String {
    if let Some(r) = slot().lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        r.state.touch_health();
        return json_status(&r.state);
    }
    let cfg = Config::default();
    let st = AppState::new(&cfg, String::new());
    json_status(&st)
}

/// 本机管理接口（Swift 直接调用，不经 HTTP）。
///
/// `path` 语义与 HTTP 路由一致：`diag` / `loot` / `session/reset` / `announcement` /
/// `capture/start|stop|download` / `shutdown` / `selftest`。
pub fn admin_impl(token: &str, path: &str, body: &str) -> String {
    let guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    let Some(r) = guard.as_ref() else {
        return r#"{"ok":false,"error":"receiver not running"}"#.to_string();
    };

    if crate::battle::protocol_capture::require_admin(&r.state, Some(token), "admin control")
        .is_err()
    {
        return r#"{"ok":false,"reason":"invalid admin token"}"#.to_string();
    }

    let write_action = !matches!(path, "diag" | "loot" | "selftest" | "capture/download");
    if write_action && crate::battle::protocol_capture::deny_if_read_only(&r.state, path).is_err() {
        return r#"{"ok":false,"reason":"read-only radar mode"}"#.to_string();
    }

    // 状态对象上的写操作是 async（与 HTTP 层共用同一套 API），这里在 runtime 上同步跑完。
    let v = r.runtime.block_on(async {
        match path {
            "selftest" => crate::battle::selftest::run().to_json(),
            "diag" => serde_json::json!({
                "ok": true,
                "status": r.state.status_snapshot(),
                "capture": r.engine.capture().status_json(),
                "sessions": r.engine.telemetry(),
                "decode_gate_open": r.engine.decode_gate_open(),
            }),
            "loot" => {
                // body 形如 {"enabled":true}；缺省视为开启。
                let enabled = !body.contains("\"enabled\":false");
                r.state.set_loot_parsing(enabled).await;
                r.engine.set_loot_parsing(enabled);
                serde_json::json!({"ok": true, "enabled": enabled,
                    "message": if enabled { "已开启" } else { "已关闭" }})
            }
            "session/reset" => {
                let cleared = r.engine.reset_all_sessions();
                r.state.reset_sessions().await;
                r.state.broadcast_diag().await;
                serde_json::json!({"ok": true, "cleared_session_count": cleared})
            }
            "announcement" => match r.state.announce().await {
                Some(a) => serde_json::json!({"ok": true, "announcement": a}),
                None => serde_json::json!({"ok": true, "announcement": null}),
            },
            "capture/start" => {
                r.state.set_capture(true).await;
                let now = crate::battle::parse_queue::now_ms();
                let files = r.engine.capture().start(now).ok();
                serde_json::json!({
                    "ok": true,
                    "protocol_capture": true,
                    "message": "整局适配采集已手动开启：文件落盘、限时限量、端点匿名化",
                    "files": files.map(|(a, b)| vec![a.display().to_string(), b.display().to_string()]),
                })
            }
            "capture/stop" => {
                r.state.set_capture(false).await;
                r.engine.capture().stop(None);
                serde_json::json!({"ok": true, "protocol_capture": false,
                    "message": "协议诊断采集已手动停止"})
            }
            "capture/download" => match r.engine.capture().download() {
                Some((name, bytes, mime)) => serde_json::json!({
                    "ok": true, "filename": name, "mime": mime, "len": bytes.len(),
                }),
                None => serde_json::json!({"ok": false, "reason": "no capture data available"}),
            },
            "shutdown" => {
                r.state.request_shutdown().await;
                serde_json::json!({"ok": true, "shutting_down": true})
            }
            other => {
                serde_json::json!({"ok": false, "reason": format!("unknown admin path: {other}")})
            }
        }
    });
    serde_json::to_string(&v).unwrap_or_else(|_| "{\"ok\":false}".to_string())
}

// ---------------------------------------------------------------------------
// C ABI
// ---------------------------------------------------------------------------

/// 版本串。返回静态内存，调用方**不需要**释放。
#[no_mangle]
pub extern "C" fn battle_proxy_version() -> *const c_char {
    VERSION_C.as_ptr() as *const c_char
}

/// 启动。`config_json` 可为 NULL/空（走默认配置）。
#[no_mangle]
pub extern "C" fn battle_proxy_start(config_json: *const c_char) -> *mut c_char {
    let cfg = unsafe { cstr_to_str(config_json) }.to_string();
    let out = std::panic::catch_unwind(move || start_impl(&cfg)).unwrap_or_else(|_| {
        "{\"phase\":\"failed\",\"error\":\"rust panic during start\"}".to_string()
    });
    to_c_string(out)
}

/// 停止。
#[no_mangle]
pub extern "C" fn battle_proxy_stop() -> *mut c_char {
    let out = std::panic::catch_unwind(stop_impl)
        .unwrap_or_else(|_| "{\"phase\":\"failed\",\"error\":\"rust panic during stop\"}".to_string());
    to_c_string(out)
}

/// 查询状态。
#[no_mangle]
pub extern "C" fn battle_proxy_status() -> *mut c_char {
    let out = std::panic::catch_unwind(status_impl).unwrap_or_else(|_| {
        "{\"phase\":\"failed\",\"error\":\"rust panic during status\"}".to_string()
    });
    to_c_string(out)
}

/// 本机管理调用。
#[no_mangle]
pub extern "C" fn battle_proxy_admin(
    token: *const c_char,
    path: *const c_char,
    body: *const c_char,
) -> *mut c_char {
    let t = unsafe { cstr_to_str(token) }.to_string();
    let p = unsafe { cstr_to_str(path) }.to_string();
    let b = unsafe { cstr_to_str(body) }.to_string();
    let out = std::panic::catch_unwind(move || admin_impl(&t, &p, &b))
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"rust panic during admin\"}".to_string());
    to_c_string(out)
}

/// 自检（无网络也能跑；结果 JSON 里 `ok` 为 false 说明 profile 需要重新标定）。
#[no_mangle]
pub extern "C" fn battle_proxy_selftest() -> *mut c_char {
    let out = std::panic::catch_unwind(|| {
        crate::init_runtime();
        crate::battle::selftest::run().to_json().to_string()
    })
    .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"rust panic during selftest\"}".to_string());
    to_c_string(out)
}

/// 释放上面几个函数返回的字符串。
#[no_mangle]
pub extern "C" fn battle_proxy_free_string(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    // SAFETY: 只释放由 `CString::into_raw` 产生的指针，且只释放一次（C 侧约定）。
    unsafe {
        drop(CString::from_raw(p));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_static_and_nul_terminated() {
        let p = battle_proxy_version();
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, crate::VERSION);
    }

    #[test]
    fn status_before_start_is_idle_and_frees_cleanly() {
        let p = battle_proxy_status();
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        assert!(s.contains("\"phase\""));
        battle_proxy_free_string(p);
    }

    #[test]
    fn selftest_c_abi_returns_json() {
        let p = battle_proxy_selftest();
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        battle_proxy_free_string(p);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn null_config_is_accepted() {
        let p = battle_proxy_start(std::ptr::null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        battle_proxy_free_string(p);
        // 端口可能被占用或被成功绑定；两种情况都必须是合法 JSON。
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("phase").is_some());
        // 清理，避免测试之间互相占用端口。
        let _ = stop_impl();
    }

    #[test]
    fn free_string_tolerates_null() {
        battle_proxy_free_string(std::ptr::null_mut());
    }

    #[test]
    fn admin_rejects_bad_token_when_not_running() {
        let out = admin_impl("wrong", "diag", "");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], false);
    }

    #[test]
    fn cstr_helpers_are_null_safe() {
        let s = unsafe { cstr_to_str(std::ptr::null()) };
        assert_eq!(s, "");
    }

    #[test]
    fn session_token_is_32_hex_chars() {
        let t = new_session_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
