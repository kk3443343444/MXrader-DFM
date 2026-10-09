//! 运行时共享状态：`battle_proxy::state`。
//!
//! 一个便宜的 `Arc` 句柄，被 socks5 监听器、UDP 关联、HTTP/WS 路由、
//! 以及 C ABI 的 status 查询共同持有。所有计数器用 atomic，避免在网络热路径上抢锁。
//!
//! 对外 API 是 `socks5` / `web` 两个模块编译所依赖的契约（见各方法文档）。

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};

/// 启动阶段，直接决定 Swift 启动页显示哪一行文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    /// 正在确认联网权限
    Preflight,
    /// 正在选择可用端口并启动接收器
    Starting,
    Running,
    Stopping,
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Preflight => "preflight",
            Phase::Starting => "starting",
            Phase::Running => "running",
            Phase::Stopping => "stopping",
            Phase::Failed => "failed",
        }
    }
    /// 启动页文案（与样本字符串一致）。
    pub fn startup_line(self) -> &'static str {
        match self {
            Phase::Preflight => "正在确认联网权限",
            Phase::Starting => "正在选择可用端口并启动接收器",
            Phase::Running => "端口已绑定，正在等待雷达页面",
            _ => "",
        }
    }
}

/// 计数器快照（HTTP `/api/status`、WS `diag`、Swift 诊断页共用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCounters {
    pub active_sessions: usize,
    pub total_sessions: u64,
    pub udp_packets_up: u64,
    pub udp_packets_down: u64,
    pub udp_invalid_packets: u64,
    pub udp_outbound_sockets: u64,
    pub tcp_relay_failures: u64,
    pub udp_relay_bytes: u64,
    pub loot_payloads_skipped: u64,
    pub parse_queue_depth: u64,
    pub parsed_packets: u64,
    pub matched_entities: u64,
}

/// 单个活跃会话（来源端口 → 玩家）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u64,
    pub peer: SocketAddr,
    pub opened_at_ms: u64,
    pub last_activity_ms: u64,
    pub tcp: bool,
    pub udp_associations: usize,
    pub udp_outbound_sockets: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    /// 该会话是否已经识别出本地玩家角色。
    pub has_local_character: bool,
    pub player_uuid: Option<String>,
}

/// 对外状态快照，序列化后即为 docs/INTERFACES.md §3。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub phase: String,
    pub mode: String,
    pub version: String,
    pub socks_port: u16,
    pub web_port: u16,
    pub primary_port: u16,
    pub data_directory: String,
    pub web_session_token: String,
    pub endpoint: EndpointStatus,
    pub runtime_status: RuntimeStatus,
    #[serde(flatten)]
    pub counters: SessionCounters,
    /// 最近访问的目标地址（TCP CONNECT 目标 + UDP 转发的每个 dest），按包数降序取前
    /// [`crate::census::STATUS_DESTINATION_LIMIT`] 条。用户靠它自己读出"游戏服务器 IP"，
    /// 从而写出"只代理游戏"的分流规则（见 `crate::census`）。
    pub destinations: Vec<crate::census::DestinationStat>,
    /// 见过的命中总量（含已被淘汰的目标）—— 有界内存下唯一能反映"总共访问过多少"的数字。
    pub destinations_total: u64,
    pub last_health_check: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointStatus {
    pub interface: String,
    pub display_address: String,
    pub radar_display_address: String,
    pub socks_url: String,
    pub radar_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    /// 卡密授权状态。
    pub authorized: bool,
    pub card_tail: Option<String>,
    pub failed_reason: Option<String>,
    /// 网络诊断：内部错误累计。
    pub last_error: Option<String>,
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        Self { authorized: true, card_tail: None, failed_reason: None, last_error: None }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 采集状态（`/api/admin/capture/*`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureStatus {
    pub enabled: bool,
    pub started_at_ms: Option<u64>,
    pub retained_bytes: usize,
    pub endpoint_anonymized: bool,
    pub max_capture_mb: u64,
    pub max_capture_seconds: u64,
}

/// 运行时共享状态句柄。
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    phase: RwLock<Phase>,
    error: RwLock<Option<String>>,
    primary_port: AtomicUsize,
    web_port: AtomicUsize,
    interface: RwLock<IpAddr>,
    display_address: RwLock<Option<IpAddr>>,
    data_directory: PathBuf,
    brand: String,
    web_session_token: String,
    admin_token: String,
    read_only: AtomicBool,
    loot_parsing: AtomicBool,
    parser_async: AtomicBool,
    collection_policy: crate::config::CollectionPolicy,
    runtime_status: RwLock<RuntimeStatus>,
    counters: Arc<Counters>,
    sessions: DashMap<u64, SessionInfo>,
    next_session: AtomicU64,
    last_health_check: RwLock<u64>,
    diag_tx: broadcast::Sender<serde_json::Value>,
    shutdown_tx: watch::Sender<bool>,
    capture: RwLock<CaptureStatus>,
    announcement: RwLock<Option<serde_json::Value>>,
    /// 目标地址统计（有界：最多 128 条，见 `crate::census`）。
    destinations: Arc<crate::census::DestinationCensus>,
    created_ms: u64,
}

#[derive(Default)]
pub struct Counters {
    pub active_sessions: AtomicUsize,
    pub total_sessions: AtomicU64,
    pub udp_packets_up: AtomicU64,
    pub udp_packets_down: AtomicU64,
    pub udp_invalid_packets: AtomicU64,
    pub udp_outbound_sockets: AtomicU64,
    pub tcp_relay_failures: AtomicU64,
    pub udp_relay_bytes: AtomicU64,
    pub loot_payloads_skipped: AtomicU64,
    pub parse_queue_depth: AtomicU64,
    pub parsed_packets: AtomicU64,
    pub matched_entities: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> SessionCounters {
        SessionCounters {
            active_sessions: self.active_sessions.load(Ordering::Relaxed),
            total_sessions: self.total_sessions.load(Ordering::Relaxed),
            udp_packets_up: self.udp_packets_up.load(Ordering::Relaxed),
            udp_packets_down: self.udp_packets_down.load(Ordering::Relaxed),
            udp_invalid_packets: self.udp_invalid_packets.load(Ordering::Relaxed),
            udp_outbound_sockets: self.udp_outbound_sockets.load(Ordering::Relaxed),
            tcp_relay_failures: self.tcp_relay_failures.load(Ordering::Relaxed),
            udp_relay_bytes: self.udp_relay_bytes.load(Ordering::Relaxed),
            loot_payloads_skipped: self.loot_payloads_skipped.load(Ordering::Relaxed),
            parse_queue_depth: self.parse_queue_depth.load(Ordering::Relaxed),
            parsed_packets: self.parsed_packets.load(Ordering::Relaxed),
            matched_entities: self.matched_entities.load(Ordering::Relaxed),
        }
    }
}

impl AppState {
    pub fn new(cfg: &crate::config::Config, web_session_token: String) -> Self {
        let (diag_tx, _) = broadcast::channel(256);
        let (shutdown_tx, _) = watch::channel(false);
        let iface: IpAddr = cfg.endpoint.interface.parse().unwrap_or(IpAddr::from([0, 0, 0, 0]));
        Self {
            inner: Arc::new(Inner {
                phase: RwLock::new(Phase::Idle),
                error: RwLock::new(None),
                primary_port: AtomicUsize::new(0),
                web_port: AtomicUsize::new(0),
                interface: RwLock::new(iface),
                display_address: RwLock::new(None),
                data_directory: cfg.data_directory.clone(),
                brand: cfg.brand.clone(),
                web_session_token,
                admin_token: cfg.admin_token.clone(),
                read_only: AtomicBool::new(cfg.read_only_radar),
                loot_parsing: AtomicBool::new(cfg.loot_parsing_enabled),
                parser_async: AtomicBool::new(cfg.parser_async),
                collection_policy: cfg.collection_policy,
                runtime_status: RwLock::new(RuntimeStatus::default()),
                counters: Arc::new(Counters::default()),
                sessions: DashMap::new(),
                next_session: AtomicU64::new(1),
                last_health_check: RwLock::new(now_ms()),
                diag_tx,
                shutdown_tx,
                capture: RwLock::new(CaptureStatus {
                    enabled: false,
                    started_at_ms: None,
                    retained_bytes: 0,
                    endpoint_anonymized: true,
                    max_capture_mb: cfg.diagnostics.max_capture_mb,
                    max_capture_seconds: cfg.diagnostics.max_capture_seconds,
                }),
                announcement: RwLock::new(None),
                destinations: Arc::new(crate::census::DestinationCensus::new()),
                created_ms: now_ms(),
            }),
        }
    }

    // ---- 阶段 ----
    pub fn set_phase(&self, phase: Phase) {
        *self.inner.phase.write() = phase;
        tracing::info!(phase = phase.as_str(), line = phase.startup_line(), "phase changed");
    }
    pub fn phase(&self) -> Phase {
        *self.inner.phase.read()
    }
    pub fn fail(&self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::error!(error = %msg, "receiver failed");
        *self.inner.error.write() = Some(msg.clone());
        *self.inner.runtime_status.write() = RuntimeStatus {
            last_error: Some(msg),
            ..self.inner.runtime_status.read().clone()
        };
        self.set_phase(Phase::Failed);
    }
    pub fn error(&self) -> Option<String> {
        self.inner.error.read().clone()
    }

    // ---- 端口 / 地址 ----
    pub fn set_ports(&self, primary: u16, web: u16) {
        self.inner.primary_port.store(primary as usize, Ordering::Relaxed);
        self.inner.web_port.store(web as usize, Ordering::Relaxed);
    }
    pub fn primary_port(&self) -> u16 {
        self.inner.primary_port.load(Ordering::Relaxed) as u16
    }
    pub fn web_port(&self) -> u16 {
        self.inner.web_port.load(Ordering::Relaxed) as u16
    }
    pub fn set_display_address(&self, ip: IpAddr) {
        *self.inner.display_address.write() = Some(ip);
    }
    pub fn display_address(&self) -> Option<IpAddr> {
        *self.inner.display_address.read()
    }
    pub fn interface(&self) -> IpAddr {
        *self.inner.interface.read()
    }
    pub fn web_session_token(&self) -> &str {
        &self.inner.web_session_token
    }
    pub fn brand(&self) -> &str {
        &self.inner.brand
    }
    pub fn data_directory(&self) -> &Path {
        &self.inner.data_directory
    }
    pub fn admin_token(&self) -> &str {
        &self.inner.admin_token
    }
    pub fn read_only(&self) -> bool {
        self.inner.read_only.load(Ordering::Relaxed)
    }
    pub fn loot_parsing_enabled(&self) -> bool {
        self.inner.loot_parsing.load(Ordering::Relaxed)
    }
    pub fn parser_async(&self) -> bool {
        self.inner.parser_async.load(Ordering::Relaxed)
    }
    pub fn collection_policy(&self) -> crate::config::CollectionPolicy {
        self.inner.collection_policy
    }

    /// 本机访问地址：优先已探测到的局域网 IP。
    pub fn base_url(&self) -> String {
        let port = self.primary_port();
        match self.display_address() {
            Some(ip) => format!("http://{ip}:{port}"),
            None => format!("http://127.0.0.1:{port}"),
        }
    }

    // ---- 会话 ----
    /// 新建 TCP 会话，返回会话 id（`one_port_one_player` 时复用同源端口会话）。
    pub async fn add_tcp_session(&self, peer: SocketAddr) -> u64 {
        if let Some(existing) = self
            .inner
            .sessions
            .iter()
            .find(|e| e.value().peer == peer && e.value().tcp)
            .map(|e| e.value().id)
        {
            self.touch_session(existing);
            return existing;
        }
        let id = self.inner.next_session.fetch_add(1, Ordering::Relaxed);
        let ts = now_ms();
        self.inner.sessions.insert(
            id,
            SessionInfo {
                id,
                peer,
                opened_at_ms: ts,
                last_activity_ms: ts,
                tcp: true,
                udp_associations: 0,
                udp_outbound_sockets: 0,
                bytes_up: 0,
                bytes_down: 0,
                has_local_character: false,
                player_uuid: None,
            },
        );
        self.inner.counters.active_sessions.fetch_add(1, Ordering::Relaxed);
        self.inner.counters.total_sessions.fetch_add(1, Ordering::Relaxed);
        tracing::info!(session = id, %peer, "session opened");
        id
    }

    /// 登记一个 UDP 关联到会话（`session_model = one_port_one_player` 时
    /// 以源端口取模复用同一会话）。
    pub fn register_udp_association(&self, session: u64, outbound_sockets: u64) {
        if let Some(mut s) = self.inner.sessions.get_mut(&session) {
            s.udp_associations += 1;
            s.udp_outbound_sockets += outbound_sockets;
            s.last_activity_ms = now_ms();
        }
        self.inner
            .counters
            .udp_outbound_sockets
            .fetch_add(outbound_sockets, Ordering::Relaxed);
    }

    pub fn touch_session(&self, session: u64) {
        if let Some(mut s) = self.inner.sessions.get_mut(&session) {
            s.last_activity_ms = now_ms();
        }
    }

    pub fn note_session_bytes(&self, session: u64, up: u64, down: u64) {
        if let Some(mut s) = self.inner.sessions.get_mut(&session) {
            s.bytes_up += up;
            s.bytes_down += down;
        }
    }

    /// 解析器识别出本地玩家后回填，供诊断页显示。
    pub fn set_session_player(&self, session: u64, uuid: Option<String>) {
        if let Some(mut s) = self.inner.sessions.get_mut(&session) {
            s.has_local_character = uuid.is_some();
            s.player_uuid = uuid;
        }
    }

    pub async fn remove_session(&self, id: u64) {
        if self.inner.sessions.remove(&id).is_some() {
            let active = self.inner.counters.active_sessions.load(Ordering::Relaxed);
            self.inner
                .counters
                .active_sessions
                .store(active.saturating_sub(1), Ordering::Relaxed);
            tracing::info!(session = id, "session closed");
        }
    }

    pub fn session_snapshot(&self) -> Vec<SessionInfo> {
        self.inner.sessions.iter().map(|e| e.value().clone()).collect()
    }

    /// `/api/admin/session/reset`：清空会话但不动其它状态，返回清理数量。
    pub async fn reset_sessions(&self) -> usize {
        let ids: Vec<u64> = self.inner.sessions.iter().map(|e| *e.key()).collect();
        let n = ids.len();
        for id in ids {
            self.inner.sessions.remove(&id);
        }
        self.inner.counters.active_sessions.store(0, Ordering::Relaxed);
        tracing::warn!(cleared_session_count = n, "cleared_session_count");
        n
    }

    // ---- 计数器 ----
    pub async fn counters(&self) -> SessionCounters {
        self.inner.counters.snapshot()
    }
    pub fn counters_ref(&self) -> Arc<Counters> {
        self.inner.counters.clone()
    }
    pub async fn note_udp_up(&self, n: usize) {
        self.inner.counters.udp_packets_up.fetch_add(1, Ordering::Relaxed);
        self.inner.counters.udp_relay_bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    pub async fn note_udp_down(&self, n: usize) {
        self.inner.counters.udp_packets_down.fetch_add(1, Ordering::Relaxed);
        self.inner.counters.udp_relay_bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    pub async fn note_invalid_packet(&self) {
        self.inner.counters.udp_invalid_packets.fetch_add(1, Ordering::Relaxed);
    }
    pub async fn note_tcp_relay_failure(&self) {
        self.inner.counters.tcp_relay_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub async fn note_udp_relay(&self, bytes: usize) {
        self.inner.counters.udp_relay_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
    pub async fn note_loot_skipped(&self) {
        self.inner.counters.loot_payloads_skipped.fetch_add(1, Ordering::Relaxed);
    }

    // ---- 目标地址统计 ----
    //
    // 调用点在网络热路径上（每个 UDP 数据报一次），所以这里刻意是**同步**方法：
    // 内部只有一个 DashMap 查找 + 两个原子操作，没有任何 await/锁竞争。
    /// 记一次目标命中（TCP CONNECT 目标、UDP 转发的 dest）。
    pub fn note_destination(&self, ip: IpAddr, ts_ms: u64) {
        self.inner.destinations.observe(ip, ts_ms);
    }

    /// `SocketAddr` 版本：只取 IP（分流规则要的是 IP，端口对它是噪音）。
    pub fn note_destination_addr(&self, addr: SocketAddr, ts_ms: u64) {
        self.note_destination(addr.ip(), ts_ms);
    }

    /// 前 `limit` 条目标（按包数降序）。
    pub fn destination_snapshot(&self, limit: usize) -> Vec<crate::census::DestinationStat> {
        self.inner.destinations.snapshot(limit)
    }

    /// 整体统计（跟踪条数 / 上限 / 总量 / 淘汰次数）。
    pub fn destination_stats(&self) -> crate::census::CensusStats {
        self.inner.destinations.stats()
    }

    // ---- 诊断 / WS ----
    pub fn diag_sender(&self) -> broadcast::Sender<serde_json::Value> {
        self.inner.diag_tx.clone()
    }
    pub fn subscribe_diag(&self) -> broadcast::Receiver<serde_json::Value> {
        self.inner.diag_tx.subscribe()
    }
    /// 向所有 WS 客户端广播一次计数器快照。
    pub async fn broadcast_diag(&self) {
        let payload = serde_json::json!({
            "type": "diag",
            "ts": now_ms(),
            "counters": self.inner.counters.snapshot(),
        });
        let _ = self.inner.diag_tx.send(payload);
    }

    pub fn touch_health(&self) {
        *self.inner.last_health_check.write() = now_ms();
    }
    pub fn uptime_ms(&self) -> u64 {
        now_ms().saturating_sub(self.inner.created_ms)
    }

    // ---- 采集控制 ----
    pub fn capture_status(&self) -> CaptureStatus {
        self.inner.capture.read().clone()
    }
    /// `/api/admin/capture/start|stop`；返回切换后的开关状态。
    pub async fn set_capture(&self, on: bool) -> bool {
        {
            let mut c = self.inner.capture.write();
            c.enabled = on;
            c.started_at_ms = if on { Some(now_ms()) } else { None };
            if !on {
                c.retained_bytes = 0;
            }
        }
        let msg = if on {
            "整局适配采集已手动开启：文件落盘、限时限量、端点匿名化"
        } else {
            "协议诊断采集已手动停止"
        };
        tracing::warn!(capture = on, "{msg}");
        let _ = self.inner.diag_tx.send(serde_json::json!({
            "type": "diag", "ts": now_ms(), "capture": on, "message": msg
        }));
        on
    }
    pub fn capture_enabled(&self) -> bool {
        self.inner.capture.read().enabled
    }
    pub fn note_capture_bytes(&self, n: usize) {
        let mut c = self.inner.capture.write();
        c.retained_bytes += n;
        let cap = (c.max_capture_mb as usize) * 1024 * 1024;
        if c.retained_bytes > cap {
            c.enabled = false;
        }
    }
    /// `/api/admin/capture/download`：返回 (文件名, 内容)。
    pub async fn capture_download(&self) -> Option<(String, Vec<u8>)> {
        let path = self.inner.data_directory.join(self.capture_file_name()?);
        let bytes = std::fs::read(&path).ok()?;
        Some((self.capture_file_name()?, bytes))
    }
    pub fn capture_file_name(&self) -> Option<String> {
        let dir = std::fs::read_dir(&self.inner.data_directory).ok()?;
        let mut best: Option<(String, std::time::SystemTime)> = None;
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("battle-full-capture-") && !name.starts_with("battle-parse-") {
                continue;
            }
            let t = e.metadata().and_then(|m| m.modified()).ok()?;
            if best.as_ref().map(|(_, bt)| t > *bt).unwrap_or(true) {
                best = Some((name, t));
            }
        }
        best.map(|(n, _)| n)
    }

    // ---- 公告 ----
    pub fn set_announcement(&self, value: Option<serde_json::Value>) {
        *self.inner.announcement.write() = value;
    }
    pub async fn announce(&self) -> Option<serde_json::Value> {
        self.inner.announcement.read().clone()
    }

    // ---- 解析开关 ----
    pub async fn set_loot_parsing(&self, on: bool) {
        self.inner.loot_parsing.store(on, Ordering::Relaxed);
        tracing::warn!(enabled = on, loot_parsing_enabled = on, "loot parser toggled");
        let _ = self.inner.diag_tx.send(serde_json::json!({
            "type": "diag", "ts": now_ms(), "loot_parsing_enabled": on
        }));
    }

    // ---- 授权 ----
    pub fn set_authorized(&self, authorized: bool, card_tail: Option<String>, reason: Option<String>) {
        let mut rs = self.inner.runtime_status.write();
        rs.authorized = authorized;
        rs.card_tail = card_tail;
        rs.failed_reason = reason;
    }
    pub fn runtime_status(&self) -> RuntimeStatus {
        self.inner.runtime_status.read().clone()
    }

    // ---- 关闭 ----
    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.inner.shutdown_tx.subscribe()
    }
    /// 保持与 socks5/web 模块约定的名字一致。
    pub fn shutdown_notify(&self) -> watch::Receiver<bool> {
        self.inner.shutdown_tx.subscribe()
    }
    pub async fn request_shutdown(&self) {
        tracing::warn!("shutting_down");
        let _ = self.inner.shutdown_tx.send(true);
    }
    pub fn is_shutdown(&self) -> bool {
        *self.inner.shutdown_tx.borrow()
    }

    // ---- 状态快照 ----
    pub async fn status_json(&self) -> serde_json::Value {
        serde_json::to_value(self.status_snapshot()).unwrap_or_else(|_| {
            serde_json::json!({"phase":"failed","error":"status serialization failed","socks_port":0,"web_port":0})
        })
    }

    pub fn status_snapshot(&self) -> StatusSnapshot {
        let port = self.primary_port();
        // 雷达页面跑在 **web 端口**上，而它是区间里与 SOCKS5 不同的另一个端口。
        // 以前这里两个 URL 都用 primary(=SOCKS5) 端口，结果 iOS 的 WebView 去连
        // SOCKS5 的端口 → "雷达页面加载失败：无法连接本机雷达端口"。
        // web 还没绑定时（启动早期）退回 primary，免得 URL 里出现 :0。
        let web = match self.web_port() {
            0 => port,
            w => w,
        };
        let addr = match self.display_address() {
            Some(ip) => ip.to_string(),
            None => "127.0.0.1".to_string(),
        };
        let brand = &self.inner.brand;
        StatusSnapshot {
            phase: self.phase().as_str().to_string(),
            mode: "ios_receiver".to_string(),
            version: crate::VERSION.to_string(),
            socks_port: port,
            web_port: web,
            primary_port: port,
            data_directory: self.inner.data_directory.display().to_string(),
            web_session_token: self.inner.web_session_token.clone(),
            endpoint: EndpointStatus {
                interface: self.interface().to_string(),
                display_address: addr.clone(),
                radar_display_address: format!("http://{addr}:{web}/battle.html?brand={brand}"),
                socks_url: format!("socks5://{addr}:{port}"),
                radar_url: format!("http://127.0.0.1:{web}/battle.html?brand={brand}"),
            },
            runtime_status: self.runtime_status(),
            counters: self.inner.counters.snapshot(),
            // 目标地址：诊断页与雷达页靠它显示"你自己读得出来的游戏服务器 IP"。
            destinations: self
                .inner
                .destinations
                .snapshot(crate::census::STATUS_DESTINATION_LIMIT),
            destinations_total: self.inner.destinations.total_packets(),
            last_health_check: now_rfc3339(),
            error: self.error(),
        }
    }
}

/// 枚举本机局域网 IPv4（IPv4 only，排除回环），用于「刷新局域网地址」。
pub fn private_ipv4_addresses() -> Vec<IpAddr> {
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs_lite() {
        for ip in ifaces {
            if is_private_ipv4(&ip) && !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

pub fn is_private_ipv4(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let private = o[0] == 10
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168);
            let link_local = o[0] == 169 && o[1] == 254;
            private && !v4.is_loopback() && !link_local
        }
        IpAddr::V6(_) => false,
    }
}

/// 用 UDP connect 技巧拿到「出口网卡 IP」，不需要枚举接口（iOS 沙箱更稳）。
pub fn primary_outbound_ipv4() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:53").ok()?;
    match sock.local_addr().ok()?.ip() {
        ip @ IpAddr::V4(_) => Some(ip),
        _ => None,
    }
}

/// getifaddrs 的轻量封装；失败时返回错误，由调用方决定是否回落到 UDP 技巧。
#[cfg(unix)]
fn if_addrs_lite() -> anyhow::Result<Vec<IpAddr>> {
    let mut out = Vec::new();
    // SAFETY: 只读遍历内核返回的链表，addr 字段按 sa_family 判定后再取。
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            anyhow::bail!("getifaddrs failed");
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
                out.push(ip);
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    Ok(out)
}

#[cfg(not(unix))]
fn if_addrs_lite() -> anyhow::Result<Vec<IpAddr>> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_lines_match_sample_strings() {
        assert_eq!(Phase::Preflight.startup_line(), "正在确认联网权限");
        assert_eq!(Phase::Starting.startup_line(), "正在选择可用端口并启动接收器");
        assert_eq!(Phase::Running.startup_line(), "端口已绑定，正在等待雷达页面");
    }

    #[test]
    fn private_ipv4_filter() {
        assert!(is_private_ipv4(&"192.168.1.23".parse().unwrap()));
        assert!(is_private_ipv4(&"10.0.0.5".parse().unwrap()));
        assert!(is_private_ipv4(&"172.20.3.4".parse().unwrap()));
        assert!(!is_private_ipv4(&"127.0.0.1".parse().unwrap()));
        assert!(!is_private_ipv4(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ipv4(&"169.254.1.2".parse().unwrap()));
    }

    /// 回归测试：雷达 URL 必须用 **web 端口**，不是 SOCKS5 端口。
    ///
    /// 真机症状（两部 iPhone 上都复现）：状态里 phase=running、primary_port/web_port
    /// 都是 2026，但 WebView 连 127.0.0.1:2026 只能打到 SOCKS5 监听器，于是弹
    /// "雷达页面加载失败：无法连接本机雷达端口"。根因是 web::serve 其实是自己在端口
    /// 区间里另找一个可用端口（2027 之类），而我们上报的还是 SOCKS5 那个号。
    #[test]
    fn radar_url_uses_the_web_port_not_the_socks_port() {
        let cfg = crate::config::Config::default();
        let st = AppState::new(&cfg, "tok".into());
        st.set_display_address("192.168.1.50".parse().unwrap());

        // 只绑了 SOCKS5 时（web 还没起来）：退回 primary，但绝不能出现 :0
        st.set_ports(2025, 0);
        let snap = st.status_snapshot();
        assert_eq!(snap.web_port, 2025, "web 未绑定时退回 primary，避免 :0");
        assert!(snap.endpoint.radar_url.contains(":2025/"), "{}", snap.endpoint.radar_url);

        // 两个端口都绑上：雷达用 2027（web），SOCKS5 仍报 2026
        st.set_ports(2026, 2027);
        let snap = st.status_snapshot();
        assert_eq!(snap.socks_port, 2026);
        assert_eq!(snap.primary_port, 2026);
        assert_eq!(snap.web_port, 2027);
        assert!(
            snap.endpoint.radar_url.ends_with(":2027/battle.html?brand=mx"),
            "radar_url 必须指向 web 端口: {}",
            snap.endpoint.radar_url
        );
        assert!(
            snap.endpoint.radar_display_address.contains(":2027/"),
            "局域网雷达地址同样用 web 端口: {}",
            snap.endpoint.radar_display_address
        );
        assert!(
            snap.endpoint.socks_url.ends_with(":2026"),
            "socks_url 必须指向 SOCKS5 端口: {}",
            snap.endpoint.socks_url
        );
    }

    #[tokio::test]
    async fn sessions_count_up_and_down() {
        let cfg = crate::config::Config::default();
        let st = AppState::new(&cfg, "tok".into());
        let id = st.add_tcp_session("192.168.1.9:40000".parse().unwrap()).await;
        assert_eq!(st.counters().await.active_sessions, 1);
        st.remove_session(id).await;
        assert_eq!(st.counters().await.active_sessions, 0);
    }

    #[tokio::test]
    async fn status_json_is_serializable_and_flattened() {
        let cfg = crate::config::Config::default();
        let st = AppState::new(&cfg, "tok".into());
        st.set_ports(2025, 2025);
        let j = st.status_json().await;
        assert_eq!(j["phase"], "idle");
        assert_eq!(j["socks_port"], 2025);
        assert!(j.get("error").is_some());
    }

    /// 目标地址统计要跟着状态 JSON 一起出去 —— 用户就是在诊断页里读这个数组，
    /// 抄出游戏服务器 IP 来写"只代理游戏"的分流规则。
    #[tokio::test]
    async fn status_json_reports_the_destination_census() {
        let cfg = crate::config::Config::default();
        let st = AppState::new(&cfg, "tok".into());
        st.set_ports(2025, 2025);

        st.note_destination("203.0.113.7".parse().unwrap(), 1_000);
        st.note_destination("203.0.113.7".parse().unwrap(), 2_000);
        st.note_destination_addr("198.51.100.4:2025".parse().unwrap(), 1_500);

        let j = st.status_json().await;
        let dests = j["destinations"].as_array().expect("destinations 必须是数组");
        assert_eq!(dests.len(), 2);
        assert_eq!(dests[0]["ip"], "203.0.113.7");
        assert_eq!(dests[0]["packets"], 2, "同一个 IP 的多次命中要聚合");
        assert_eq!(dests[0]["last_ms"], 2_000);
        assert_eq!(dests[1]["ip"], "198.51.100.4");
        assert_eq!(j["destinations_total"], 3, "总量含所有命中");
    }
}
