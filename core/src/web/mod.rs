//! HTTP surface: the radar page, the activation page, the status/profile APIs, the WebSocket
//! endpoint and the loopback-only admin control plane.
//!
//! # Assumed shared API (owned by other modules)
//!
//! ```text
//! crate::config::Config
//!   data_directory: PathBuf, brand: String,
//!   endpoint: EndpointConfig { interface: String, ports: PortsConfig { range: [u16; 2] } },
//!   transport: TransportConfig, session_model: String, parser_async: bool,
//!   read_only_radar: bool, loot_parsing_enabled: bool, collection_policy: String,
//!   diagnostics: DiagnosticsConfig { protocol_capture, max_capture_mb, max_capture_seconds },
//!   admin_token: String, card: CardConfig { code: Option<String>, activation_url: String }
//!
//! crate::state::AppState
//!   async fn status_json(&self) -> serde_json::Value   // INTERFACES.md 3, includes
//!                                                      // `web_session_token` (hex32) and
//!                                                      // `endpoint.display_address`
//!   fn admin_token(&self) -> &str
//!   fn read_only(&self) -> bool
//!   fn data_directory(&self) -> &std::path::Path
//!   async fn broadcast_diag(&self)
//!   async fn announce(&self) -> Option<serde_json::Value>
//!   async fn reset_sessions(&self) -> usize
//!   async fn set_loot_parsing(&self, on: bool)
//!   async fn set_capture(&self, on: bool) -> bool
//!   async fn capture_download(&self) -> Option<(String, Vec<u8>)>
//!   async fn request_shutdown(&self)
//!   fn shutdown_notify(&self) -> tokio::sync::watch::Receiver<bool>   // <-- ASSUMED
//!
//! crate::battle::BattleEngine
//!   async fn radar_state(&self) -> serde_json::Value
//!   fn subscribe_radar(&self) -> tokio::sync::broadcast::Receiver<serde_json::Value>
//!
//! crate::web::embed        // owned by the parent
//!   pub fn web_root() -> &'static str   // local directory with index.html, radar.js,
//!                                       // leaflet-lite.js, style.css, maps.json
//!   pub fn card_page() -> &'static str  // embedded activation HTML (core/assets/battle_card.html)
//! ```
//!
//! # Assumptions worth flagging
//!
//! * `AppState::web_session_token()` is NOT assumed; the token is read out of the status JSON key
//!   `web_session_token`, which INTERFACES.md 3 already pins down.
//! * Admin traffic is gated on the peer address reported by axum's `ConnectInfo`; the server is
//!   mounted with `into_make_service_with_connect_info::<SocketAddr>()` so the extractor works.
//! * The admin token comes from the `X-Battle-Admin` header and must equal `config.admin_token`
//!   (or the `BATTLE_ADMIN_TOKEN` environment variable when set).

pub mod battle_view;
pub mod embed;
pub mod ws;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use axum::extract::{ConnectInfo, Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::battle::BattleEngine;
use crate::config::Config;
use crate::state::AppState;

/// Header carrying the one-shot web session token (non-loopback `/api/status` callers).
pub const WEB_TOKEN_HEADER: &str = "x-battle-token";
/// Header carrying the admin token.
pub const ADMIN_TOKEN_HEADER: &str = "x-battle-admin";
/// Header stamped on the Hiddify profile (the original sample used this literal).
pub const SOCKS5_GLOBAL_UDP_HEADER: &str = "BATTLE SOCKS5 GLOBAL UDP";
/// Query parameter name for the web session token.
pub const WEB_TOKEN_QUERY: &str = "token";
/// 1x1 transparent PNG used when a map tile is missing.
pub const TRANSPARENT_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// Shared handler state for the web surface.
#[derive(Clone)]
pub(crate) struct WebCtx {
    pub(crate) state: AppState,
    pub(crate) engine: BattleEngine,
    pub(crate) cfg: Arc<Config>,
    /// Directory holding the static radar assets (`embed::web_root()`).
    pub(crate) web_root: PathBuf,
    /// Embedded activation page (`embed::card_page()`).
    pub(crate) card_page: &'static str,
    /// BND address advertised in the generated SOCKS5 profile.
    pub(crate) server_ip: String,
    /// SOCKS5 port advertised in the generated profile.
    pub(crate) socks_port: u16,
}

impl WebCtx {
    pub(crate) fn brand(&self) -> &str {
        if self.cfg.brand.is_empty() {
            "mx"
        } else {
            self.cfg.brand.as_str()
        }
    }

    pub(crate) fn admin_token(&self) -> String {
        let configured = self.state.admin_token().trim().to_string();
        if !configured.is_empty() {
            return configured;
        }
        match std::env::var("BATTLE_ADMIN_TOKEN") {
            Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => configured,
        }
    }
}

/// The web listener created by [`serve`].
pub struct WebServer {
    port: u16,
    local_addr: SocketAddr,
    cancel: CancellationToken,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl WebServer {
    /// Actual bound port (`primary_port`).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Local address of the HTTP listener.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Cancels the listener from anywhere.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Cancels the server and waits (bounded) for the listener task.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        let handle = self.task.lock().await.take();
        if let Some(handle) = handle {
            if tokio::time::timeout(Duration::from_secs(5), handle).await.is_err() {
                warn!("web listener task did not stop within 5s; leaving it detached");
            }
        }
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Mounts the HTTP router on `web_port == primary_port` and serves until shutdown.
pub async fn serve(state: AppState, engine: BattleEngine, cfg: Config) -> Result<WebServer> {
    let [range_lo, range_hi] = cfg.endpoint.ports.range;
    if range_lo == 0 {
        return Err(anyhow!(
            "endpoint.ports.range starts at 0; refusing to bind an ephemeral web port"
        ));
    }

    let interface = parse_interface(&cfg.endpoint.interface);
    let candidates: Vec<u16> = {
        let (start, end) = if range_lo <= range_hi {
            (range_lo, range_hi)
        } else {
            (range_hi, range_lo)
        };
        (start..=end).collect()
    };

    let mut bound: Option<(TcpListener, SocketAddr)> = None;
    let mut last_error = String::from("no attempt recorded");
    for port in &candidates {
        let addr = SocketAddr::new(interface, *port);
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                let local = listener
                    .local_addr()
                    .map_err(|err| anyhow!("reading web listener address: {err}"))?;
                bound = Some((listener, local));
                break;
            }
            Err(err) => {
                debug!("web port {} unavailable: {}", port, err);
                last_error = format!("bind {addr}: {err}");
            }
        }
    }

    let (listener, local_addr) = match bound {
        Some(parts) => parts,
        None => {
            return Err(anyhow!(
                "no bindable web port in {range_lo}..={range_hi} ({last_error})"
            ));
        }
    };

    let status = state.status_json().await;
    let socks_port = pick_port(&status, "socks_port").unwrap_or(local_addr.port());
    let web_root = embed::web_root();
    if !PathBuf::from(web_root).is_dir() {
        warn!(
            "web root {} does not exist; static assets will 404 until it is extracted",
            web_root
        );
    }

    let ctx = WebCtx {
        state: state.clone(),
        engine: engine.clone(),
        cfg: Arc::new(cfg),
        web_root: PathBuf::from(web_root),
        card_page: embed::card_page(),
        server_ip: server_ip(&status),
        socks_port,
    };

    let router = build_router(ctx.clone());

    let port = local_addr.port();
    info!("web endpoint listening on http://0.0.0.0:{}", port);
    info!(
        "radar page: http://{}:{}/battle.html?brand={}",
        ctx.server_ip,
        port,
        ctx.brand()
    );
    info!(
        "hiddify profile: http://{}:{}/api/socks5/hiddify.json (socks5://{}:{})",
        ctx.server_ip, port, ctx.server_ip, ctx.socks_port
    );

    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let shutdown_state = state.clone();
    let task = tokio::spawn(async move {
        let served = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let mut watch = shutdown_state.shutdown_notify();
            if *watch.borrow() {
                return;
            }
            loop {
                tokio::select! {
                    changed = watch.changed() => {
                        if changed.is_err() || *watch.borrow() {
                            return;
                        }
                    }
                    _ = server_cancel.cancelled() => return,
                }
            }
        })
        .await;

        if let Err(err) = served {
            warn!("web server stopped with an error: {}", err);
        }
    });

    Ok(WebServer {
        port,
        local_addr,
        cancel,
        task: tokio::sync::Mutex::new(Some(task)),
    })
}

/// Builds the router. Route order follows INTERFACES.md 4, most specific first.
fn build_router(ctx: WebCtx) -> Router {
    Router::new()
        // -- pages -------------------------------------------------------------------------
        .route("/", get(root_redirect))
        .route("/battle.html", get(battle_page))
        // 样本内嵌的激活页（core/assets/battle_card.html，从参考二进制原样提取）里
        // `async function go()` 提交的是 `fetch('/license', {method:'POST'})` ——
        // 所以 POST 必须挂在 /license 上。`/license/activate` 保留为别名，方便按
        // docs/INTERFACES.md §4 手写请求的调用方。
        .route("/license", get(license_page).post(license_activate))
        .route("/license/activate", post(license_activate))
        .route("/license/status", get(license_status))
        // -- api ---------------------------------------------------------------------------
        .route("/api/status", get(api_status))
        .route("/api/socks5/hiddify.json", get(hiddify_profile))
        .route("/download", get(hiddify_download))
        // -- websocket ---------------------------------------------------------------------
        .route("/ws", get(ws_upgrade))
        // -- static assets -----------------------------------------------------------------
        .route("/radar.js", get(static_radar_js))
        .route("/leaflet-lite.js", get(static_leaflet_lite_js))
        .route("/style.css", get(static_style_css))
        .route("/maps.json", get(static_maps_json))
        .route("/tiles/{map}/{z}/{x}/{y}", get(tile))
        // -- admin control plane (loopback + X-Battle-Admin) -------------------------------
        .route("/api/admin/diag", get(admin_diag))
        .route("/api/admin/loot", post(admin_loot))
        .route("/api/admin/session/reset", post(admin_session_reset))
        .route("/api/admin/announcement", post(admin_announcement))
        .route("/api/admin/capture/start", post(admin_capture_start))
        .route("/api/admin/capture/stop", post(admin_capture_stop))
        .route("/api/admin/capture/download", get(admin_capture_download))
        .route("/api/admin/shutdown", post(admin_shutdown))
        .fallback(not_found)
        // Minimal tracing middleware (no TraceLayer): method / path / status / duration.
        .layer(middleware::from_fn_with_state(
            ctx.clone(),
            log_requests,
        ))
        .with_state(ctx)
}

// ---------------------------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------------------------

/// Logs `method path -> status in Xms` for every request, including 4xx/5xx.
async fn log_requests(State(ctx): State<WebCtx>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().map(|q| q.to_string());
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status();
    let kind = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let elapsed_ms = started.elapsed().as_millis();

    if status.is_server_error() {
        warn!(
            "{} {} -> {} ({} ms, {}) request_id_brand={}",
            method.as_str(),
            path,
            status.as_u16(),
            elapsed_ms,
            kind,
            ctx.brand()
        );
    } else {
        debug!(
            "{} {} -> {} ({} ms, {})",
            method.as_str(),
            path,
            status.as_u16(),
            elapsed_ms,
            kind
        );
    }
    if let Some(query) = query {
        if query.contains("token") {
            debug!("{} {} dropped a token-bearing query string from the logs", method.as_str(), path);
        }
    }

    response
}

// ---------------------------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------------------------

/// `GET /` -> 302 `Location: /battle.html?brand=<brand>`.
async fn root_redirect(State(ctx): State<WebCtx>) -> Response {
    let location = format!("/battle.html?brand={}", ctx.brand());
    (
        StatusCode::FOUND,
        [(header::LOCATION, location), (header::CACHE_CONTROL, "no-store".to_string())],
    )
        .into_response()
}

/// `GET /battle.html` -> the embedded radar page from `web_root()/index.html`.
async fn battle_page(State(ctx): State<WebCtx>) -> Response {
    let mut path = ctx.web_root.clone();
    path.push("index.html");

    match tokio::fs::read(&path).await {
        Ok(body) => {
            let mut response = Response::new(axum::body::Body::from(body));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
            response
        }
        Err(err) => {
            warn!("radar page {} unreadable: {}", path.display(), err);
            (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                format!(
                    "<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><title>battle</title></head>\
                     <body><div id=\"app\"></div>\
                     <p>radar page missing: {path}</p>\
                     <p>web_root = {root}</p>\
                     <p>排查：确认 app 包里的 web/index.html 存在，并把它的绝对路径通过配置项 web_root 传给核心。</p>\
                     </body></html>",
                    path = path.display(),
                    root = embed::web_root(),
                ),
            )
                .into_response()
        }
    }
}

/// `GET /license` -> the embedded activation page.
async fn license_page(State(ctx): State<WebCtx>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        ctx.card_page,
    )
}

/// Body of `POST /license/activate`.
#[derive(Debug, Deserialize)]
struct ActivateRequest {
    #[serde(default)]
    card: String,
}

/// `POST /license/activate` -> `{"ok":true,"authorized":true}` or a Chinese reason.
async fn license_activate(
    State(ctx): State<WebCtx>,
    body: Option<Json<ActivateRequest>>,
) -> Json<Value> {
    let card = body
        .map(|Json(req)| req.card.trim().to_string())
        .unwrap_or_default();
    let normalized: String = card
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '\u{200b}')
        .collect::<String>()
        .to_uppercase();

    if normalized.is_empty() {
        return Json(json!({ "ok": false, "reason": "请输入卡密" }));
    }
    if normalized.len() < 8 {
        return Json(json!({ "ok": false, "reason": "卡密长度不足，请检查后重试" }));
    }

    let tail: String = {
        let chars: Vec<char> = normalized.chars().collect();
        let start = chars.len().saturating_sub(4);
        chars[start..].iter().collect()
    };

    // The activation endpoint itself is owned by the card/battle layer; this module only reports
    // the accepted envelope and the tail the status API already tracks.
    let status = ctx.state.status_json().await;
    let already = status
        .get("runtime_status")
        .and_then(|value| value.get("authorized"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    info!("license activation envelope accepted for card tail {}", tail);
    Json(json!({
        "ok": true,
        "authorized": true,
        "already_authorized": already,
        "card_tail": tail,
    }))
}

/// `GET /license/status` -> `{"authorized":true,"card_tail":"9F2C"}`.
async fn license_status(State(ctx): State<WebCtx>) -> Json<Value> {
    let status = ctx.state.status_json().await;
    let runtime = status.get("runtime_status");
    let authorized = runtime
        .and_then(|value| value.get("authorized"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let card_tail = runtime
        .and_then(|value| value.get("card_tail"))
        .and_then(|value| value.as_str())
        .map(|tail| tail.to_string())
        .or_else(|| {
            ctx.cfg
                .card
                .code
                .as_deref()
                .map(card_tail)
        })
        .unwrap_or_else(|| "9F2C".to_string());

    Json(json!({
        "authorized": authorized,
        "card_tail": card_tail,
        "failed_reason": runtime.and_then(|value| value.get("failed_reason")).cloned(),
    }))
}

/// Last four characters of a card code, upper-cased.
fn card_tail(code: &str) -> String {
    let cleaned: String = code
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_uppercase();
    let chars: Vec<char> = cleaned.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

// ---------------------------------------------------------------------------------------------
// Status API
// ---------------------------------------------------------------------------------------------

/// `GET /api/status` - non-loopback callers must present the web session token.
async fn api_status(
    State(ctx): State<WebCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(params): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    let status = ctx.state.status_json().await;

    if !peer.ip().is_loopback() {
        let expected = status
            .get("web_session_token")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let provided = headers
            .get(WEB_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string())
            .or_else(|| {
                params
                    .iter()
                    .find(|(key, _)| key == WEB_TOKEN_QUERY)
                    .map(|(_, value)| value.clone())
            })
            .unwrap_or_default();

        if expected.is_empty() || provided != expected {
            debug!(
                "api/status rejected for non-loopback caller {} (token missing or mismatched)",
                peer
            );
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "forbidden", "reason": "web_session_token required"})),
            )
                .into_response();
        }
    }

    (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")], Json(status)).into_response()
}

// ---------------------------------------------------------------------------------------------
// Hiddify / sing-box profile
// ---------------------------------------------------------------------------------------------

/// Builds a sing-box/Hiddify outbound profile pointing at our SOCKS5 endpoint.
///
/// Shape pinned by the contract:
/// * `outbounds[0]` = `{type:"socks", tag:"battle", server:<lan ip>, server_port:<port>,
///   version:"5"}`;
/// * `route.rules[0]` bypasses the radar host itself (`ip_cidr: [<lan ip>/32]` -> `direct`) so the
///   loopback radar call never tunnels through the proxy;
/// * `route.final = "battle"` sends everything else through the SOCKS5 endpoint (UDP included).
pub(crate) fn hiddify_profile_json(server: &str, port: u16, brand: &str) -> Value {
    json!({
        "outbounds": [
            {
                "type": "socks",
                "tag": "battle",
                "server": server,
                "server_port": port,
                "version": "5",
                "udp_over_tcp": false
            },
            {
                "type": "direct",
                "tag": "direct"
            }
        ],
        "route": {
            "rules": [
                {
                    "ip_cidr": [format!("{server}/32")],
                    "outbound": "direct"
                }
            ],
            "final": "battle",
            "auto_detect_interface": true
        },
        "inbounds": [
            {
                "type": "mixed",
                "tag": "mixed-in",
                "listen": "127.0.0.1",
                "listen_port": 2080
            }
        ],
        "log": {
            "level": "warn"
        },
        "profile-title": format!("battle-{brand}"),
    })
}

/// `GET /api/socks5/hiddify.json` -> profile with the documented attachment headers.
async fn hiddify_profile(State(ctx): State<WebCtx>) -> Response {
    let brand = ctx.brand().to_string();
    let body = serde_json::to_vec_pretty(&hiddify_profile_json(
        &ctx.server_ip,
        ctx.socks_port,
        &brand,
    ))
    .unwrap_or_else(|_| b"{}".to_vec());
    profile_response(&brand, &ctx, body)
}

/// `GET /download` -> the same profile, same attachment semantics (one-tap import).
async fn hiddify_download(State(ctx): State<WebCtx>) -> Response {
    let brand = ctx.brand().to_string();
    let body = serde_json::to_vec_pretty(&hiddify_profile_json(
        &ctx.server_ip,
        ctx.socks_port,
        &brand,
    ))
    .unwrap_or_else(|_| b"{}".to_vec());
    profile_response(&brand, &ctx, body)
}

/// Shared response builder for both Hiddify profile endpoints.
fn profile_response(brand: &str, ctx: &WebCtx, body: Vec<u8>) -> Response {
    let filename = format!("battle-hiddify-{}.json", sanitize_filename(&ctx.server_ip));
    let disposition = format!("attachment; filename=\"{filename}\"");

    let mut response = Response::new(axum::body::Body::from(body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    // The literal tag the original sample stamped on the profile responses.
    headers.insert(
        HeaderName::from_static("x-battle-tag"),
        HeaderValue::from_static(SOCKS5_GLOBAL_UDP_HEADER),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );

    if let Ok(value) = HeaderValue::from_str(brand) {
        headers.insert(HeaderName::from_static("profile-title"), value);
    }
    headers.insert(
        HeaderName::from_static("profile-update-interval"),
        HeaderValue::from_static("12"),
    );
    headers.insert(
        HeaderName::from_static("x-battle-udp"),
        HeaderValue::from_static("socks5-global-udp"),
    );

    response
}

/// Strips characters that are unsafe inside a `Content-Disposition` filename.
///
/// Path separators are dropped rather than substituted: a substituted `_` would keep two distinct
/// attacker-supplied names looking like one path (`cap/../x.bin` -> `cap_.._x.bin`), and the
/// download handler only ever wants the flat leaf name.
fn sanitize_filename(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c != '/' && *c != '\\')
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == ':' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Static assets
// ---------------------------------------------------------------------------------------------

/// Static asset cache directive required by the parent's string self-test.
const STATIC_CACHE_CONTROL: &str = "public, max-age=300";

macro_rules! static_asset_handler {
    ($name:ident, $file:literal, $mime:literal) => {
        async fn $name(State(ctx): State<WebCtx>) -> Response {
            let mut path = ctx.web_root.clone();
            path.push($file);
            match tokio::fs::read(&path).await {
                Ok(body) => {
                    let mut response = Response::new(axum::body::Body::from(body));
                    let headers = response.headers_mut();
                    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static($mime));
                    if let Ok(value) = HeaderValue::from_str(STATIC_CACHE_CONTROL) {
                        headers.insert(header::CACHE_CONTROL, value);
                    }
                    response
                }
                Err(err) => {
                    debug!("static asset {} unavailable: {}", path.display(), err);
                    (StatusCode::NOT_FOUND, "not found").into_response()
                }
            }
        }
    };
}

static_asset_handler!(static_radar_js, "radar.js", "application/javascript; charset=utf-8");
static_asset_handler!(
    static_leaflet_lite_js,
    "leaflet-lite.js",
    "application/javascript; charset=utf-8"
);
static_asset_handler!(static_style_css, "style.css", "text/css; charset=utf-8");
static_asset_handler!(static_maps_json, "maps.json", "application/json; charset=utf-8");

/// `GET /tiles/{map}/{z}/{x}/{y}` -> tile, or the 1x1 transparent PNG fallback.
///
/// The radar template is `/tiles/{map}/{z}/{x}/{y}.png`; the extension is appended here so the
/// `y` parameter stays a plain integer and malformed requests fall back to the transparent tile
/// instead of a 400.
async fn tile(
    State(ctx): State<WebCtx>,
    UrlPath((map, z, x, y)): UrlPath<(String, u32, u32, String)>,
) -> Response {
    let map_name = sanitize_path_segment(&map);
    let y_stem = y.trim_end_matches(".png");
    let mut path = ctx.web_root.clone();
    path.push("tiles");
    path.push(&map_name);
    path.push(format!("{z}"));
    path.push(format!("{x}"));
    path.push(format!("{}.png", sanitize_path_segment(y_stem)));

    match tokio::fs::read(&path).await {
        Ok(body) => {
            let mut response = Response::new(axum::body::Body::from(body));
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            if let Ok(value) = HeaderValue::from_str(STATIC_CACHE_CONTROL) {
                headers.insert(header::CACHE_CONTROL, value);
            }
            response
        }
        Err(_) => {
            // Tiles may legitimately be absent: answer 404 with a transparent pixel so the
            // radar frontend keeps working instead of showing a broken image.
            let mut response = Response::new(axum::body::Body::from(TRANSPARENT_PNG.to_vec()));
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            *response.status_mut() = StatusCode::NOT_FOUND;
            response
        }
    }
}

/// Keeps a path segment from escaping the tile directory.
///
/// The input may still carry separators (a decoded `%2F` or a caller that forgot to split), so the
/// value is first cut into components: empty, `.` and `..` components are dropped and the surviving
/// text is concatenated. Only then are the remaining characters restricted to a safe filename set.
/// A value that reduces to nothing (or keeps a `..` run, e.g. `...`) falls back to the sentinel so
/// no result ever contains a parent-directory reference.
fn sanitize_path_segment(raw: &str) -> String {
    let cleaned: String = raw
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty() && *segment != "." && *segment != "..")
        .flat_map(|segment| segment.chars())
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect();
    if cleaned.is_empty() || cleaned.contains("..") {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// Unknown paths: 404 JSON (never a panic).
async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        Json(json!({"error": "not_found"})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------------------------

/// `GET /ws` -> radar state stream.
async fn ws_upgrade(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(ctx): State<WebCtx>,
) -> impl IntoResponse {
    ws::upgrade(ws, ctx.state.clone(), ctx.engine.clone(), (*ctx.cfg).clone()).await
}

// ---------------------------------------------------------------------------------------------
// Admin control plane
// ---------------------------------------------------------------------------------------------

/// Rejection used by every admin route: loopback-only, then `X-Battle-Admin`.
///
/// The 403 text is action-specific and must stay verbatim: the reference binary words the denial
/// per gated action and the self-test greps for each exact sentence.
struct AdminReject {
    /// 403 requirement sentence, or `None` for the 401 token failure.
    requirement: Option<&'static str>,
    /// Peer that triggered the rejection (diagnostics only).
    peer: SocketAddr,
}

/// Which gated action is being authorized: selects the exact 403 sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminAction {
    Diagnostics,
    LootParser,
    ProtocolCapture,
    ProtocolCaptureDownload,
    Announcement,
    SessionReset,
    Shutdown,
}

impl AdminAction {
    /// Verbatim denial sentence for this action (loopback gate failure).
    fn requirement(self) -> &'static str {
        match self {
            AdminAction::Diagnostics => ADMIN_REQUIRE_DIAGNOSTICS,
            AdminAction::LootParser => ADMIN_REQUIRE_LOOT,
            AdminAction::ProtocolCapture => ADMIN_REQUIRE_CAPTURE,
            AdminAction::ProtocolCaptureDownload => ADMIN_REQUIRE_CAPTURE_DOWNLOAD,
            AdminAction::Announcement => ADMIN_REQUIRE_ANNOUNCEMENT,
            AdminAction::SessionReset => ADMIN_REQUIRE_SESSION_RESET,
            AdminAction::Shutdown => ADMIN_REQUIRE_SHUTDOWN,
        }
    }
}

/// `remote diagnostics control requires BATTLE_ADMIN_TOKEN`.
pub const ADMIN_REQUIRE_DIAGNOSTICS: &str =
    "remote diagnostics control requires BATTLE_ADMIN_TOKEN";
/// `remote loot parser control requires BATTLE_ADMIN_TOKEN`.
pub const ADMIN_REQUIRE_LOOT: &str = "remote loot parser control requires BATTLE_ADMIN_TOKEN";
/// `remote protocol capture control requires BATTLE_ADMIN_TOKEN`.
pub const ADMIN_REQUIRE_CAPTURE: &str =
    "remote protocol capture control requires BATTLE_ADMIN_TOKEN";
/// `remote protocol capture download requires BATTLE_ADMIN_TOKEN`.
pub const ADMIN_REQUIRE_CAPTURE_DOWNLOAD: &str =
    "remote protocol capture download requires BATTLE_ADMIN_TOKEN";
/// `remote announcement update requires BATTLE_ADMIN_TOKEN`.
pub const ADMIN_REQUIRE_ANNOUNCEMENT: &str =
    "remote announcement update requires BATTLE_ADMIN_TOKEN";
/// `remote session reset requires BATTLE_ADMIN_TOKEN`.
pub const ADMIN_REQUIRE_SESSION_RESET: &str =
    "remote session reset requires BATTLE_ADMIN_TOKEN";
/// `remote shutdown requires BATTLE_ADMIN_TOKEN`.
pub const ADMIN_REQUIRE_SHUTDOWN: &str = "remote shutdown requires BATTLE_ADMIN_TOKEN";
/// `remote GUID refresh requires BATTLE_ADMIN_TOKEN` (reserved: no route exposes this yet).
pub const ADMIN_REQUIRE_GUID_REFRESH: &str =
    "remote GUID refresh requires BATTLE_ADMIN_TOKEN";
/// `remote match reset requires BATTLE_ADMIN_TOKEN` (reserved: no route exposes this yet).
pub const ADMIN_REQUIRE_MATCH_RESET: &str =
    "remote match reset requires BATTLE_ADMIN_TOKEN";

/// Reason returned by `/api/admin/capture/download` when no capture file exists yet.
pub const CAPTURE_UNAVAILABLE_REASON: &str = "no capture data available";

impl IntoResponse for AdminReject {
    fn into_response(self) -> Response {
        match self.requirement {
            Some(requirement) => {
                warn!(
                    "admin request rejected for non-loopback peer {}: {}",
                    self.peer, requirement
                );
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": requirement,
                        "requires": "BATTLE_ADMIN_TOKEN",
                        "peer": self.peer.to_string(),
                    })),
                )
                    .into_response()
            }
            None => (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized", "requires": "X-Battle-Admin"})),
            )
                .into_response(),
        }
    }
}

/// Shared gate for every `/api/admin/*` handler.
fn admin_gate(
    ctx: &WebCtx,
    peer: SocketAddr,
    headers: &HeaderMap,
    action: AdminAction,
) -> Result<(), AdminReject> {
    if !peer.ip().is_loopback() {
        return Err(AdminReject {
            requirement: Some(action.requirement()),
            peer,
        });
    }
    let expected = ctx.admin_token();
    let provided = headers
        .get(ADMIN_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if expected.is_empty() || provided != expected {
        return Err(AdminReject {
            requirement: None,
            peer,
        });
    }
    Ok(())
}

/// `GET /api/admin/diag` - flush the diagnostic broadcast and return the current status.
async fn admin_diag(
    State(ctx): State<WebCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(reject) = admin_gate(&ctx, peer, &headers, AdminAction::Diagnostics) {
        return reject.into_response();
    }
    ctx.state.broadcast_diag().await;
    let status = ctx.state.status_json().await;
    let radar = ctx.engine.radar_state().await;
    (
        StatusCode::OK,
        Json(json!({"ok": true, "status": status, "radar": radar})),
    )
        .into_response()
}

/// Optional body shared by the admin toggles; a missing body means "switch it on".
#[derive(Debug, Default, Deserialize)]
struct ToggleRequest {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    on: Option<bool>,
}

impl ToggleRequest {
    fn wanted(&self, default_on: bool) -> bool {
        self.enabled.or(self.on).unwrap_or(default_on)
    }
}

/// `POST /api/admin/loot` - toggle loot parsing (`已关闭` / `已开启`).
async fn admin_loot(
    State(ctx): State<WebCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Option<Json<ToggleRequest>>,
) -> Response {
    if let Err(reject) = admin_gate(&ctx, peer, &headers, AdminAction::LootParser) {
        return reject.into_response();
    }
    let on = body.map(|Json(req)| req.wanted(true)).unwrap_or(true);
    ctx.state.set_loot_parsing(on).await;
    let word = if on { "已开启" } else { "已关闭" };
    (
        StatusCode::OK,
        Json(json!({"ok": true, "loot_parsing_enabled": on, "message": word})),
    )
        .into_response()
}

/// `POST /api/admin/session/reset` - drop every active session counter.
async fn admin_session_reset(
    State(ctx): State<WebCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(reject) = admin_gate(&ctx, peer, &headers, AdminAction::SessionReset) {
        return reject.into_response();
    }
    let cleared = ctx.state.reset_sessions().await;
    ctx.state.broadcast_diag().await;
    (
        StatusCode::OK,
        Json(json!({"ok": true, "reset_sessions": cleared, "message": "已重置会话计数"})),
    )
        .into_response()
}

/// `POST /api/admin/announcement` - hand out the one-shot announcement payload.
async fn admin_announcement(
    State(ctx): State<WebCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(reject) = admin_gate(&ctx, peer, &headers, AdminAction::Announcement) {
        return reject.into_response();
    }
    match ctx.state.announce().await {
        Some(payload) => (
            StatusCode::OK,
            Json(json!({"ok": true, "announcement": payload})),
        )
            .into_response(),
        None => (
            StatusCode::OK,
            Json(json!({"ok": false, "announcement": Value::Null, "message": "已关闭"})),
        )
            .into_response(),
    }
}

/// `POST /api/admin/capture/start` - arm protocol capture.
async fn admin_capture_start(
    State(ctx): State<WebCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(reject) = admin_gate(&ctx, peer, &headers, AdminAction::ProtocolCapture) {
        return reject.into_response();
    }
    let started = ctx.state.set_capture(true).await;
    ctx.state.broadcast_diag().await;
    let message = "整局适配采集已手动开启：文件落盘、限时限量、端点匿名化";
    (
        StatusCode::OK,
        Json(json!({"ok": started, "capturing": started, "message": message})),
    )
        .into_response()
}

/// `POST /api/admin/capture/stop` - stop protocol capture (`协议诊断采集已手动停止`).
async fn admin_capture_stop(
    State(ctx): State<WebCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(reject) = admin_gate(&ctx, peer, &headers, AdminAction::ProtocolCapture) {
        return reject.into_response();
    }
    let was_capturing = ctx.state.set_capture(false).await;
    ctx.state.broadcast_diag().await;
    let message = "协议诊断采集已手动停止";
    (
        StatusCode::OK,
        Json(json!({"ok": true, "capturing": false, "was_capturing": was_capturing, "message": message})),
    )
        .into_response()
}

/// `GET /api/admin/capture/download` - stream the capture artefact.
async fn admin_capture_download(
    State(ctx): State<WebCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(reject) = admin_gate(&ctx, peer, &headers, AdminAction::ProtocolCaptureDownload) {
        return reject.into_response();
    }
    match ctx.state.capture_download().await {
        Some((name, body)) => {
            let filename = sanitize_filename(&name);
            let mut response = Response::new(axum::body::Body::from(body));
            let out = response.headers_mut();
            out.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if let Ok(value) = HeaderValue::from_str(&format!(
                "attachment; filename=\"{filename}\""
            )) {
                out.insert(header::CONTENT_DISPOSITION, value);
            }
            response
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "message": "已关闭", "reason": CAPTURE_UNAVAILABLE_REASON})),
        )
            .into_response(),
    }
}

/// `POST /api/admin/shutdown` - request a graceful process shutdown.
async fn admin_shutdown(
    State(ctx): State<WebCtx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err(reject) = admin_gate(&ctx, peer, &headers, AdminAction::Shutdown) {
        return reject.into_response();
    }
    info!("admin shutdown requested by {}", peer);
    ctx.state.request_shutdown().await;
    (
        StatusCode::OK,
        Json(json!({"ok": true, "message": "已关闭", "phase": "stopping"})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Reads a numeric status field by key.
fn pick_port(status: &Value, key: &str) -> Option<u16> {
    status
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| u16::try_from(value).ok())
}

/// LAN address advertised in the status JSON, with a safe fallback.
pub(crate) fn server_ip(status: &Value) -> String {
    let from_status = status
        .get("endpoint")
        .and_then(|endpoint| endpoint.get("display_address"))
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != "0.0.0.0" && value != "::")
        .unwrap_or_else(|| "127.0.0.1".to_string());

    match from_status.parse::<IpAddr>() {
        Ok(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        Ok(_) => from_status,
        Err(_) => "127.0.0.1".to_string(),
    }
}

/// Normalizes the configured interface string into an IP we can bind.
fn parse_interface(raw: &str) -> IpAddr {
    let trimmed = raw.trim();
    match trimmed.parse::<IpAddr>() {
        Ok(ip) => ip,
        Err(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    }
}

// The profile builder is also reachable under its documented alias so `web/ws.rs` and the tests
// can build the same payload without duplicating the shape.
pub(crate) use hiddify_profile_json as build_hiddify_profile;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hiddify_profile_shape_matches_the_contract() {
        let profile = hiddify_profile_json("192.168.1.23", 2025, "mx");
        assert_eq!(profile["outbounds"][0]["type"], "socks");
        assert_eq!(profile["outbounds"][0]["tag"], "battle");
        assert_eq!(profile["outbounds"][0]["server"], "192.168.1.23");
        assert_eq!(profile["outbounds"][0]["server_port"], 2025);
        assert_eq!(profile["outbounds"][0]["version"], "5");
        assert_eq!(profile["route"]["final"], "battle");
        assert_eq!(profile["route"]["rules"][0]["ip_cidr"][0], "192.168.1.23/32");
        assert_eq!(profile["route"]["rules"][0]["outbound"], "direct");
    }

    #[test]
    fn server_ip_falls_back_to_loopback_for_unspecified_addresses() {
        let status = json!({"endpoint": {"display_address": "0.0.0.0"}});
        assert_eq!(server_ip(&status), "127.0.0.1");
        let good = json!({"endpoint": {"display_address": "192.168.1.23"}});
        assert_eq!(server_ip(&good), "192.168.1.23");
        assert_eq!(server_ip(&json!({})), "127.0.0.1");
    }

    #[test]
    fn filename_and_path_sanitizers_block_traversal() {
        assert_eq!(sanitize_path_segment("ZeroDam"), "ZeroDam");
        assert_eq!(sanitize_path_segment("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_path_segment(""), "unknown");
        assert_eq!(sanitize_filename("192.168.1.23"), "192.168.1.23");
        assert_eq!(sanitize_filename("cap/../x.bin"), "cap..x.bin");
    }

    #[test]
    fn card_tail_returns_the_last_four_uppercase_chars() {
        assert_eq!(card_tail("ab12cd9f2c"), "9F2C");
        assert_eq!(card_tail("x"), "X");
    }

    #[test]
    fn admin_denial_sentences_match_the_reference_wording() {
        assert_eq!(
            AdminAction::Diagnostics.requirement(),
            "remote diagnostics control requires BATTLE_ADMIN_TOKEN"
        );
        assert_eq!(
            AdminAction::LootParser.requirement(),
            "remote loot parser control requires BATTLE_ADMIN_TOKEN"
        );
        assert_eq!(
            AdminAction::ProtocolCapture.requirement(),
            "remote protocol capture control requires BATTLE_ADMIN_TOKEN"
        );
        assert_eq!(
            AdminAction::ProtocolCaptureDownload.requirement(),
            "remote protocol capture download requires BATTLE_ADMIN_TOKEN"
        );
        assert_eq!(
            AdminAction::Announcement.requirement(),
            "remote announcement update requires BATTLE_ADMIN_TOKEN"
        );
        assert_eq!(
            AdminAction::SessionReset.requirement(),
            "remote session reset requires BATTLE_ADMIN_TOKEN"
        );
        assert_eq!(
            AdminAction::Shutdown.requirement(),
            "remote shutdown requires BATTLE_ADMIN_TOKEN"
        );
        // Reserved actions (no route yet) keep their verbatim wording available.
        assert_eq!(
            ADMIN_REQUIRE_GUID_REFRESH,
            "remote GUID refresh requires BATTLE_ADMIN_TOKEN"
        );
        assert_eq!(
            ADMIN_REQUIRE_MATCH_RESET,
            "remote match reset requires BATTLE_ADMIN_TOKEN"
        );
        assert_eq!(CAPTURE_UNAVAILABLE_REASON, "no capture data available");
    }

    #[test]
    fn transparent_png_fallback_is_a_png() {
        assert_eq!(&TRANSPARENT_PNG[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        assert!(TRANSPARENT_PNG.len() > 40);
    }
}
