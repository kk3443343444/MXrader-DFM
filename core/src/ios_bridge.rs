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
    /// 监听器的句柄（socket 本体在各自 spawn 出去的循环里）。
    /// 留着是为了 stop 时能有序关掉：只发 shutdown 信号也能退出，
    /// 但显式 await 一次能保证"停"之后端口立刻可被下次启动绑定。
    socks: crate::socks5::Socks5Listener,
    web: crate::web::WebServer,
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
///
/// 直接取 `crate::VERSION` —— 那是 build.rs 注入的**同一次构建的戳**，于是
/// `battle_proxy_version()`（Swift 诊断页顶部显示的那个）与状态 JSON 的 `version`
/// 字段、以及 IPA 资产名不可能各说各话。
static VERSION_C: &str = concat!(env!("BATTLE_VERSION"), "\0");

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

    // 预检：区间里是否还有空闲端口。**只**用于给出友好的中文报错；
    // 真正的端口以下面两个 listener 实际绑定的为准。
    if pick_port(&cfg).is_none() {
        state.fail(format!(
            "接收端口被其他应用占用（已自动尝试 {}-{}）。请从后台彻底关闭旧版 MXrader/BATTLE，或卸载多余的雷达后，再点「重新启动」。",
            cfg.endpoint.ports.range[0], cfg.endpoint.ports.range[1]
        ));
        return serde_json::to_string(&state.status_snapshot())
            .unwrap_or_else(|_| "{\"phase\":\"failed\"}".to_string());
    }
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

    // 真正绑定：SOCKS5 占区间里第一个可用端口，web 占**另一个**（两者不可能同号）。
    // 必须绑完再返回 running —— iOS 启动页对 running 的文案就是"端口已绑定，
    // 正在等待雷达页面"，而且 Swift 侧随后会拿状态里的 web_port 去加载 WebView。
    // 这里如果只是 spawn 出去不管，端口号就是瞎上报的（曾经就是这么做，导致
    // WebView 连上 SOCKS5 端口 → "雷达页面加载失败"）。
    let bound = runtime.block_on(async {
        match crate::socks5::run(state.clone(), engine.clone(), cfg.clone()).await {
            Ok(socks) => match crate::web::serve(state.clone(), engine.clone(), cfg.clone()).await {
                Ok(web) => Ok((socks, web)),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        }
    });
    let (socks, web) = match bound {
        Ok(pair) => pair,
        Err(e) => {
            state.fail(format!("本地服务启动失败，请检查端口是否被占用。原始错误：{e}"));
            runtime.shutdown_timeout(std::time::Duration::from_secs(1));
            return json_status(&state);
        }
    };

    state.set_ports(socks.port(), web.port());
    state.set_phase(crate::state::Phase::Running);
    state.touch_health();
    tracing::info!(
        socks = socks.port(),
        web = web.port(),
        "receiver running: SOCKS5 TCP/UDP + radar web"
    );

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
    *slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(Receiver {
        runtime,
        state,
        engine,
        cfg,
        socks,
        web,
    });
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
            let Receiver { runtime, state, web, socks, .. } = r;
            // 通知所有听众退出（socks5 的转发任务、广播、解析流水都监听它），
            // 再显式关掉两个监听器，最后给 2 秒收尾。
            runtime.block_on(async {
                state.request_shutdown().await;
                let _ = tokio::time::timeout(std::time::Duration::from_millis(500), web.shutdown())
                    .await;
                let _ = tokio::time::timeout(std::time::Duration::from_millis(500), socks.shutdown())
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            });
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
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

    /// `ios_bridge` 用的是**进程级单例**（`RECEIVER`），所以任何真的去 start/stop
    /// 接收器的测试都必须串行执行。否则一个测试的 start/stop 会把另一个测试正在跑的
    /// 接收器关掉，症状是请求中途 `ConnectionReset`（"远程主机强迫关闭了一个现有的
    /// 连接"）—— 这在 CI 上表现为随机失败，很难查。
    ///
    /// 只用于测试，不参与生产路径。
    static SINGLETON_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_singleton() -> std::sync::MutexGuard<'static, ()> {
        SINGLETON_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

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
        let _guard = lock_singleton();
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

    /// 端到端回归测试，复刻真机上的失败场景（两部 iPhone 都复现过）：
    ///
    /// 状态里 phase=running，但 WebView 打开 status.endpoint.radar_url 时
    /// "无法连接本机雷达端口"。原因是 SOCKS5 与 web 各自在端口区间里找空位，
    /// 而状态上报的 web_port 其实是 SOCKS5 的端口。
    ///
    /// 所以这里不只比对字段，而是**真的按状态里的 radar_url 发一次 HTTP 请求**，
    /// 要求拿到 200 + 页面内容。不启动真实转发流量，只走"绑定 → 上报 → 访问"这条链。
    #[test]
    fn radar_url_from_status_is_actually_served() {
        // 与 null_config_is_accepted 等测试共享单例，必须串行（见 SINGLETON_LOCK）。
        let _guard = lock_singleton();
        // 每次用一个独立端口区间，避免和本机上别的东西（或这个测试的重复运行）抢端口。
        // 注意配置的真实形状是 endpoint.ports.range（闭区间），不是顶层的 socks_port；
        // serde 对未知字段是"静默忽略"，所以写错名字不会报错、只会悄悄跑在默认区间上。
        let base = 27400 + (std::process::id() % 100) as u16 * 8;
        // 数据目录指进工作区：这台开发机禁止子进程往 %TEMP% 写东西。
        let data_dir = crate::testutil::scratch_dir("ios-bridge-e2e");
        let cfg_json = format!(
            r#"{{"brand":"mx","data_directory":{},"endpoint":{{"ports":{{"range":[{base},{}]}}}}}}"#,
            serde_json::Value::String(data_dir.display().to_string()),
            base + 5
        );

        let started = start_impl(&cfg_json);
        let status: serde_json::Value = serde_json::from_str(&started).expect("status JSON");
        assert_eq!(status["phase"], "running", "启动后应为 running：{started}");

        let socks_port = status["socks_port"].as_u64().unwrap() as u16;
        let web_port = status["web_port"].as_u64().unwrap() as u16;
        assert!(socks_port != 0 && web_port != 0, "两个端口都要上报：{started}");
        assert!(
            (base..=base + 5).contains(&socks_port) && (base..=base + 5).contains(&web_port),
            "端口应取自配置区间 {base}..={}：{started}",
            base + 5
        );
        assert_ne!(
            socks_port, web_port,
            "SOCKS5 与 web 必须是区间里两个不同的端口（同号就意味着有一个没绑上）"
        );

        let radar_url = status["endpoint"]["radar_url"].as_str().unwrap().to_string();
        assert!(
            radar_url.contains(&format!(":{web_port}/")),
            "radar_url 必须指向 web 端口 {web_port}：{radar_url}"
        );

        // 真发一次请求：连 radar_url 里的端口，要 HTTP 200。
        let (host, rest) = radar_url
            .trim_start_matches("http://")
            .split_once('/')
            .expect("radar_url 形状");
        // 连接重试：accept 循环是在 runtime 里 spawn 的，刚返回时它可能还没被调度到。
        let mut connected = None;
        let mut last_err = String::new();
        for _ in 0..40 {
            match std::net::TcpStream::connect(host) {
                Ok(s) => {
                    connected = Some(s);
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
        let mut stream = connected.unwrap_or_else(|| {
            panic!("按状态里的地址 {host} 连不上（这正是真机的症状）：{last_err}")
        });

        use std::io::{Read, Write};
        // 读超时兜底：Connection: close 下服务端会收尾，但测试不该因为收尾慢就失败。
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));
        stream
            .write_all(format!("GET /{rest} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes())
            .unwrap();
        // 按字节读、有损解码，并把读超时/半关闭当作"读到这儿为止"：
        // 页面是 UTF-8 中文，用 read_to_string 会在"截断在多字节字符中间"时报
        // InvalidData —— 那是测试写法的问题，不是服务端的问题。
        let mut raw = Vec::new();
        let _ = stream.read_to_end(&mut raw);
        let response = String::from_utf8_lossy(&raw).into_owned();
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "雷达页没被服务：{}",
            response.lines().next().unwrap_or("")
        );
        assert!(response.contains("battle-ready"), "返回的不是雷达页");
        assert!(response.contains("</html>"), "页面不完整");

        let stopped = stop_impl();
        assert!(
            stopped.contains("\"phase\":\"stopping\""),
            "停止后应上报 stopping：{stopped}"
        );
    }
}
