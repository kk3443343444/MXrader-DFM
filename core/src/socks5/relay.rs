//! Bidirectional TCP relay with a peek-and-forward tap, plus the per-association UDP relay.
//!
//! # Assumed shared API
//!
//! ```text
//! crate::state::AppState
//!   async fn add_tcp_session(&self, peer: SocketAddr) -> u64
//!   async fn remove_session(&self, id: u64)
//!   async fn note_udp_up(&self, n: usize)
//!   async fn note_udp_down(&self, n: usize)
//!   async fn note_invalid_packet(&self)
//!   async fn note_tcp_relay_failure(&self)
//!   async fn note_udp_relay(&self, bytes: usize)
//!
//! crate::battle::BattleEngine
//!   fn feed(&self, session: u64, src: SocketAddr, dst: SocketAddr,
//!           payload: &bytes::Bytes, ts_ms: u64)                     // non-blocking, queue push
//!   fn feed_dir(&self, session: u64, src: SocketAddr, dst: SocketAddr,
//!               payload: &bytes::Bytes, ts_ms: u64, c2s: bool)      // direction-aware
//! ```
//!
//! # Contract of this module
//!
//! * [`relay_tcp`] copies both directions with two manual `tokio::select!`-driven loops instead of
//!   a single `copy_bidirectional`, so every chunk read from the client toward the upstream is
//!   handed to `engine.feed(...)` before it is forwarded: **先转发，后解析**. The tap hands over a
//!   `Bytes` copy and returns immediately, so a slow parser cannot stall the relay, and
//!   backpressure stays correct because the writer awaits `write_all`.
//! * `src`/`dst` identify the direction: `src == client endpoint` is client-to-server, so the
//!   engine can tell C2S from S2C on both transports.
//! * [`relay_udp_with`] runs one loop per [`UdpAssociation`]: client datagrams have the SOCKS5 UDP
//!   header stripped before reaching the real destination, and upstream datagrams come back with a
//!   freshly encoded `RSV(2) FRAG(1) ATYP ADDR PORT` header addressed to the last known client
//!   endpoint. The shared SOCKS-port socket is used send-only here: its reads belong to the demux
//!   loop in `socks5::mod`.
//! * [`reap_idle`] / [`idle_reaper_loop`] cancel associations idle for at least
//!   [`UDP_ASSOC_IDLE_TIMEOUT_MS`] (300 s).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use crate::battle::BattleEngine;
use crate::state::AppState;

use super::handshake::{encode_udp_response, parse_udp_request, UdpAssociationTable};
use super::now_ms;

/// Idle timeout for a UDP association: 300 s without traffic and it is reaped.
pub const UDP_ASSOC_IDLE_TIMEOUT_MS: u64 = 300_000;
/// How often the background reaper sweeps the association table.
pub const UDP_REAPER_INTERVAL_MS: u64 = 15_000;
/// Read chunk used by both TCP relay directions.
const RELAY_CHUNK: usize = 16 * 1024;
/// Receive buffer for one UDP datagram.
const UDP_CHUNK: usize = 65_535;

/// Process-global counter of UDP association relay loops ever started.
static UDP_RELAYS: AtomicU64 = AtomicU64::new(0);

/// Number of UDP association relay loops started in this process.
pub fn udp_relay_count() -> u64 {
    UDP_RELAYS.load(Ordering::Relaxed)
}

/// Number of destination-facing UDP sockets currently opened.
pub fn udp_outbound_sockets(table: &UdpAssociationTable) -> u64 {
    table.udp_outbound_sockets()
}

/// Converts a millisecond epoch stamp into a `tokio::time::Instant` (reaper deadlines, tests).
pub fn deadline_from_ms(ms: u64) -> tokio::time::Instant {
    tokio::time::Instant::now() - Duration::from_millis(now_ms().saturating_sub(ms))
}

/// One UDP ASSOCIATE session.
///
/// `client_endpoint` is the endpoint the client's SOCKS5 UDP datagrams arrive from; `socket` is
/// the socket used to reach real destinations (ephemeral per association, or the SOCKS-port socket
/// when the mode asks for a shared listener); `bound` is that socket's local address.
#[derive(Debug)]
pub struct UdpAssociation {
    /// Session / association id (the TCP session id that created it).
    pub id: u64,
    /// Client endpoint that owns this association.
    pub client_endpoint: SocketAddr,
    /// 客户端 UDP 数据报的**实际**来源（学习值优先，否则退回 `client_endpoint`）。
    ///
    /// 为什么不能只用 `client_endpoint`：那是 TCP 控制连接的对端地址，而客户端发 UDP
    /// 用的是另一个 socket（源端口必然不同），RFC 1928 还允许客户端在 ASSOCIATE 请求里
    /// 填 0.0.0.0:0。按 IP:端口 严格比较会把**每一个数据报都丢掉**，而且回程也发到错的
    /// 端口 —— 真机症状正是"代理连上了、游戏一直提示网络异常"。
    /// 用第一个数据报的来源学习真实地址；`Arc` 保证学习结果对所有克隆可见。
    learned_client: Arc<std::sync::Mutex<Option<SocketAddr>>>,
    /// Destination-facing socket.
    pub socket: Arc<UdpSocket>,
    /// Local address of `socket`.
    pub bound: SocketAddr,
    /// Creation time in UNIX milliseconds.
    pub created_ms: u64,
    /// Last activity in UNIX milliseconds (bumped by the demux and by the relay).
    pub last_activity_ms: AtomicU64,
    /// Payload bytes forwarded toward the destination.
    pub bytes_up: AtomicU64,
    /// Payload bytes forwarded back to the client.
    pub bytes_down: AtomicU64,
    /// Payload bytes, both directions, for the `udp_relay_bytes` diagnostic.
    pub udp_relay_bytes: AtomicU64,
    /// Number of client datagrams routed by this association.
    pub datagrams_up: AtomicU64,
    /// Number of upstream datagrams routed back to the client.
    pub datagrams_down: AtomicU64,
    /// Per-association cancellation handle (fired by the control connection or the reaper).
    cancel: CancellationToken,
}

/// 手写 `Clone`：`AtomicU64` 不实现 `Clone`，所以逐字段搬运当前值（克隆体拥有
/// 独立的计数器，但共享同一个 destination socket 与取消令牌）——这正是多任务
/// 引用同一关联时想要的语义。
impl Clone for UdpAssociation {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            client_endpoint: self.client_endpoint,
            learned_client: Arc::clone(&self.learned_client),
            socket: Arc::clone(&self.socket),
            bound: self.bound,
            created_ms: self.created_ms,
            last_activity_ms: AtomicU64::new(self.last_activity_ms.load(Ordering::Relaxed)),
            bytes_up: AtomicU64::new(self.bytes_up.load(Ordering::Relaxed)),
            bytes_down: AtomicU64::new(self.bytes_down.load(Ordering::Relaxed)),
            udp_relay_bytes: AtomicU64::new(self.udp_relay_bytes.load(Ordering::Relaxed)),
            datagrams_up: AtomicU64::new(self.datagrams_up.load(Ordering::Relaxed)),
            datagrams_down: AtomicU64::new(self.datagrams_down.load(Ordering::Relaxed)),
            cancel: self.cancel.clone(),
        }
    }
}

impl UdpAssociation {
    /// 回程要发往的客户端地址：以学习到的真实来源为准，没有就用关联创建时声明的地址。
    pub fn effective_client_endpoint(&self) -> SocketAddr {
        self.learned_client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(self.client_endpoint)
    }

    /// 记下第一个真正的客户端来源。返回 true 表示发生了学习（调用方可以打日志）。
    pub fn learn_client_endpoint(&self, from: SocketAddr) -> bool {
        let mut slot = self.learned_client.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(from);
            return true;
        }
        false
    }

    /// 已学习到的客户端真实来源（还没学到就是 `None`）。
    pub fn learned_endpoint(&self) -> Option<SocketAddr> {
        *self.learned_client.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bumps the activity stamp seen by the reaper.
    pub fn touch(&self, now: u64) {
        self.last_activity_ms.store(now, Ordering::Relaxed);
    }
    /// Milliseconds since the last activity.
    pub fn idle_ms(&self, now: u64) -> u64 {
        now.saturating_sub(self.last_activity_ms.load(Ordering::Relaxed))
    }

    /// True when the association has been idle for at least `timeout_ms`.
    pub fn is_idle(&self, now: u64, timeout_ms: u64) -> bool {
        self.idle_ms(now) >= timeout_ms
    }

    /// Cancels the association relay loop.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Cancellation token shared with the relay loop.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Cloneable copy of the cancellation token.
    pub fn clone_cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// True once the association was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Adds to the forwarded-upstream byte counter and returns the new total.
    pub fn add_bytes_up(&self, n: usize) -> u64 {
        self.udp_relay_bytes.fetch_add(n as u64, Ordering::Relaxed);
        self.datagrams_up.fetch_add(1, Ordering::Relaxed);
        self.bytes_up.fetch_add(n as u64, Ordering::Relaxed) + n as u64
    }

    /// Adds to the forwarded-to-client byte counter and returns the new total.
    pub fn add_bytes_down(&self, n: usize) -> u64 {
        self.udp_relay_bytes.fetch_add(n as u64, Ordering::Relaxed);
        self.datagrams_down.fetch_add(1, Ordering::Relaxed);
        self.bytes_down.fetch_add(n as u64, Ordering::Relaxed) + n as u64
    }

    /// `(bytes_up, bytes_down)`.
    pub fn counters(&self) -> (u64, u64) {
        (
            self.bytes_up.load(Ordering::Relaxed),
            self.bytes_down.load(Ordering::Relaxed),
        )
    }

    /// `(datagrams_up, datagrams_down)`.
    pub fn datagram_counts(&self) -> (u64, u64) {
        (
            self.datagrams_up.load(Ordering::Relaxed),
            self.datagrams_down.load(Ordering::Relaxed),
        )
    }

    /// Total relayed payload bytes in both directions.
    pub fn relayed_bytes(&self) -> u64 {
        self.udp_relay_bytes.load(Ordering::Relaxed)
    }
}

impl Drop for UdpAssociation {
    fn drop(&mut self) {
        // Never leave a relay loop behind when the last handle goes away.
        self.cancel.cancel();
    }
}

/// 判断一个 UDP 读错误是不是"上一次发包引发的 ICMP 回报"，这类错误必须忽略。
///
/// 典型来源：把数据报发给一个没有监听者的 UDP 端口，对端回 ICMP port unreachable，
/// 操作系统把它挂在 socket 上，下一次 `recv_from` 就返回错误。
///   * Windows: `WSAECONNRESET (10054)` / `WSAENETRESET (10052)` / `WSAECONNABORTED (10053)`
///   * Unix/macOS: `ECONNREFUSED` / `ECONNRESET`（仅 connected socket 会报）
/// 若把它当作致命错误退出循环，这条关联就再也不会转发任何数据。
fn is_benign_udp_error(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        err.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionAborted
            | ErrorKind::NetworkUnreachable
            | ErrorKind::HostUnreachable
            | ErrorKind::TimedOut
            | ErrorKind::WouldBlock
            | ErrorKind::Interrupted
    )
}

/// Builds a UDP association with a fresh per-association cancellation token.
pub fn new_udp_association(
    id: u64,
    client_endpoint: SocketAddr,
    socket: Arc<UdpSocket>,
    bound: SocketAddr,
    created_ms: u64,
) -> UdpAssociation {
    UDP_RELAYS.fetch_add(1, Ordering::Relaxed);
    debug!(
        "relay=per_association_ephemeral UDP association {} created: client={} bound={}",
        id, client_endpoint, bound
    );
    UdpAssociation {
        id,
        client_endpoint,
        learned_client: Arc::new(std::sync::Mutex::new(None)),
        socket,
        bound,
        created_ms,
        last_activity_ms: AtomicU64::new(created_ms),
        bytes_up: AtomicU64::new(0),
        bytes_down: AtomicU64::new(0),
        udp_relay_bytes: AtomicU64::new(0),
        datagrams_up: AtomicU64::new(0),
        datagrams_down: AtomicU64::new(0),
        cancel: CancellationToken::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// TCP relay
// ---------------------------------------------------------------------------------------------

/// Bidirectional TCP relay with a parse tap on both directions.
///
/// Returns `Ok(())` when either side reaches EOF, and an error when a copy failed - the caller
/// turns that into a `tcp_relay_failures` bump plus the `] TCP relay ended: upstream=` log line.
pub async fn relay_tcp(
    state: AppState,
    engine: BattleEngine,
    session_id: u64,
    upstream: TcpStream,
    client: TcpStream,
) -> Result<()> {
    let client_peer = socket_peer(&client);
    let upstream_peer = socket_peer(&upstream);

    debug!(
        "] TCP relay begin: session={} client={} upstream={}",
        session_id, client_peer, upstream_peer
    );

    let cancel = CancellationToken::new();
    let (mut client_rx, mut client_tx) = client.into_split();
    let (mut upstream_rx, mut upstream_tx) = upstream.into_split();

    // Client -> upstream, tapped for the parser: `src` is the client, `dst` the upstream.
    let c2s_cancel = cancel.clone();
    let c2s_engine = engine.clone();
    let c2s: JoinHandle<Result<u64>> = tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(RELAY_CHUNK);
        loop {
            let read = tokio::select! {
                biased;
                _ = c2s_cancel.clone().cancelled_owned() => return Ok(0u64),
                read = client_rx.read_buf(&mut buf) => read,
            };
            match read {
                Ok(0) => return Ok(0u64),
                Ok(_) => {}
                Err(err) => return Err(anyhow!("client->upstream read failed: {err}")),
            }
            if buf.is_empty() {
                continue;
            }
            // The tap sees the bytes before they leave; feed_dir only queues them and flags the
            // direction (c2s = true) so the engine never has to compare addresses.
            let snapshot: Bytes = Bytes::copy_from_slice(&buf);
            c2s_engine.feed_dir(session_id, client_peer, upstream_peer, &snapshot, now_ms(), true);

            let written = tokio::select! {
                biased;
                _ = c2s_cancel.clone().cancelled_owned() => return Ok(0u64),
                written = upstream_tx.write_all(&buf) => written,
            };
            match written {
                Ok(()) => buf.clear(),
                Err(err) => return Err(anyhow!("client->upstream write failed: {err}")),
            }
        }
    });

    // Upstream -> client, tapped with the direction reversed so C2S and S2C stay distinguishable.
    let s2c_cancel = cancel.clone();
    let s2c_engine = engine.clone();
    let s2c: JoinHandle<Result<u64>> = tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(RELAY_CHUNK);
        loop {
            let read = tokio::select! {
                biased;
                _ = s2c_cancel.clone().cancelled_owned() => return Ok(0u64),
                read = upstream_rx.read_buf(&mut buf) => read,
            };
            match read {
                Ok(0) => return Ok(0u64),
                Ok(_) => {}
                Err(err) => return Err(anyhow!("upstream->client read failed: {err}")),
            }
            if buf.is_empty() {
                continue;
            }
            let snapshot: Bytes = Bytes::copy_from_slice(&buf);
            s2c_engine.feed_dir(session_id, upstream_peer, client_peer, &snapshot, now_ms(), false);

            let written = tokio::select! {
                biased;
                _ = s2c_cancel.clone().cancelled_owned() => return Ok(0u64),
                written = client_tx.write_all(&buf) => written,
            };
            match written {
                Ok(()) => buf.clear(),
                Err(err) => return Err(anyhow!("upstream->client write failed: {err}")),
            }
        }
    });

    // The first half to finish (EOF or error) cancels the sibling and decides the outcome.
    let mut c2s = c2s;
    let mut s2c = s2c;
    let first = tokio::select! {
        joined = &mut c2s => ("client->upstream", joined),
        joined = &mut s2c => ("upstream->client", joined),
    };

    // Unblock and collect the sibling so no task is left mid-copy.
    cancel.cancel();
    let (direction, joined) = first;
    let result = flatten_join(joined, direction);
    let sibling = if direction == "client->upstream" {
        flatten_join(s2c.await, "upstream->client")
    } else {
        flatten_join(c2s.await, "client->upstream")
    };

    let mut failure: Option<anyhow::Error> = None;
    if let Err(err) = result {
        failure = Some(err);
    }
    if let Err(err) = sibling {
        debug!("] TCP relay ended: upstream={} sibling={}", upstream_peer, err);
        if failure.is_none() {
            failure = Some(err);
        }
    }

    match failure {
        Some(err) => {
            state.note_tcp_relay_failure().await;
            debug!(
                "] TCP relay ended: upstream={} session={} direction={} error={}",
                upstream_peer, session_id, direction, err
            );
            Err(err)
        }
        None => {
            debug!(
                "] TCP relay ended: upstream={} session={} direction={} eof",
                upstream_peer, session_id, direction
            );
            Ok(())
        }
    }
}

/// Unwraps a join result into the copy result with a readable message.
fn flatten_join(joined: Result<Result<u64, anyhow::Error>, tokio::task::JoinError>, direction: &str) -> Result<u64> {
    match joined {
        Ok(inner) => inner,
        Err(err) => Err(anyhow!("{direction} relay task failed: {err}")),
    }
}

/// Best-effort peer address of a TCP stream; never panics.
fn socket_peer(stream: &TcpStream) -> SocketAddr {
    stream
        .peer_addr()
        .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
}

/// Outbound-socket owner used by the UDP relay so both relay modes share one code path.
#[derive(Debug, Clone)]
pub enum UdpClientSocket {
    /// The single socket bound to the SOCKS port. Send-only here: reads belong to the demux loop.
    Shared(Arc<UdpSocket>),
    /// A per-association ephemeral socket.
    Ephemeral(Arc<UdpSocket>),
}

impl UdpClientSocket {
    /// True when reads are handled elsewhere (the shared demux loop).
    pub fn is_shared(&self) -> bool {
        matches!(self, UdpClientSocket::Shared(_))
    }

    /// Underlying socket.
    pub fn socket(&self) -> &Arc<UdpSocket> {
        match self {
            UdpClientSocket::Shared(sock) => sock,
            UdpClientSocket::Ephemeral(sock) => sock,
        }
    }

    /// Sends one datagram.
    pub async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> std::io::Result<usize> {
        self.socket().send_to(buf, dst).await
    }

    /// Receives one datagram. Only valid for the ephemeral variant.
    pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.socket().recv_from(buf).await
    }
}

/// Convenience entry point for an association whose token already exists and whose
/// destination-facing socket is the ephemeral one.
pub async fn relay_udp(
    state: AppState,
    engine: BattleEngine,
    session_id: u64,
    assoc: Arc<UdpAssociation>,
) -> Result<()> {
    let table = Arc::new(UdpAssociationTable::new());
    let owner = UdpClientSocket::Ephemeral(assoc.socket.clone());
    let until = assoc.clone_cancel_token();
    let _ = session_id;
    relay_udp_with(state, engine, table, assoc, owner, until).await
}

/// 判定一个来源是"客户端数据报"还是"上游响应"。
///
/// 为什么必须有它：`per_association_ephemeral` 下客户端按 BND.ADDR:BND.PORT 把数据报发到
/// 本关联的临时端口，而转发也是从**同一个** socket 出去的，于是上游响应回到同一个 socket ——
/// 两个方向只能在一个读分支里按来源分流。分错的后果就是真机上那种"UDP 只出不进"：
/// 客户端数据报被当成上游响应打回控制连接的端口，真正的响应又被当成客户端数据报丢掉。
///
/// 规则：学习到真实来源之后就只认它的**精确地址**（同一个 IP 上的上游服务器不能被当成
/// 客户端）；还没学过时，客户端在 ASSOCIATE 请求里填了 0.0.0.0:0（RFC 1928 允许）就接受
/// 第一个来源，否则要求同地址或至少同 IP。
fn is_client_source(assoc: &UdpAssociation, from: SocketAddr) -> bool {
    match assoc.learned_endpoint() {
        Some(learned) => from == learned,
        None => {
            let declared = assoc.client_endpoint;
            declared.ip().is_unspecified() || from == declared || from.ip() == declared.ip()
        }
    }
}

/// 回程：给上游数据报套上 `RSV FRAG ATYP SRC PORT` 头，发往客户端**真实**的 UDP 来源。
///
/// 为什么用 `effective_client_endpoint` 而不是 `client_endpoint`：后者是 TCP 控制连接的对端，
/// 客户端发 UDP 用的是另一个 socket（源端口必然不同）。发到 TCP 端口的话客户端永远收不到回包，
/// Windows 还会把 ICMP 端口不可达回报到那个发包的 socket 上（`os error 10054`），
/// 看起来像"读 socket 报错了"。
async fn send_upstream_datagram_to_client(
    state: &AppState,
    assoc: &UdpAssociation,
    owner: &UdpClientSocket,
    from: SocketAddr,
    datagram: &[u8],
) {
    let client = assoc.effective_client_endpoint();
    let framed = encode_udp_response(from, datagram);
    assoc.touch(now_ms());
    match owner.send_to(&framed, client).await {
        Ok(written) => {
            assoc.add_bytes_down(datagram.len());
            state.note_udp_down(datagram.len()).await;
            state.note_udp_relay(written).await;
            trace!(
                "] UDP response send_to {} (assoc {}, {} bytes)",
                client,
                assoc.id,
                datagram.len()
            );
        }
        Err(err) => {
            debug!("UDP response send_to {} failed: {}", client, err);
            state.note_tcp_relay_failure().await;
        }
    }
}

/// Per-association UDP relay loop.
///
/// * client -> destination: strip the SOCKS5 UDP header, tap the datagram, forward it.
/// * destination -> client: prepend the header for the sender address, send to the last known
///   client endpoint.
pub async fn relay_udp_with(
    state: AppState,
    engine: BattleEngine,
    table: Arc<UdpAssociationTable>,
    assoc: Arc<UdpAssociation>,
    owner: UdpClientSocket,
    until: CancellationToken,
) -> Result<()> {
    let mut client_buf = vec![0u8; UDP_CHUNK];
    let mut upstream_buf = vec![0u8; UDP_CHUNK];
    let idle_timeout = Duration::from_millis(UDP_ASSOC_IDLE_TIMEOUT_MS);
    let mut sweep = tokio::time::interval(Duration::from_secs(1));

    debug!(
        "; UDP relays are per-association, session={} assoc={} client={} bound={} relays={}",
        assoc.id,
        assoc.id,
        assoc.client_endpoint,
        assoc.bound,
        udp_relay_count()
    );

    loop {
        if assoc.is_idle(now_ms(), UDP_ASSOC_IDLE_TIMEOUT_MS) {
            debug!("UDP relay ended: association {} idle", assoc.id);
            break;
        }

        // 客户端面与目标面在 `per_association_ephemeral` 下是**同一个**临时 socket
        // （客户端按 BND.ADDR:BND.PORT 发到它，转发也从它出去），所以这个 socket 只允许
        // 一个分支读：两个方向在分支内部按来源分流（见 `is_client_source`）。
        // 两条分支同时读同一个 socket 会互相抢包 —— 谁先 poll 到就归谁，于是客户端数据报
        // 被当成上游响应打回控制连接的端口，回程自然永远到不了客户端。
        // `shared_port` 下客户端面是 SOCKS 端口那个 socket、它的读取归 demux loop
        // （见 socks5::mod），这里只读目标面。
        let shared_client_face = owner.is_shared();

        let client_read = async {
            if shared_client_face {
                std::future::pending::<std::io::Result<(usize, SocketAddr)>>().await
            } else {
                owner.recv_from(&mut client_buf).await
            }
        };
        let upstream_read = async {
            if shared_client_face {
                assoc.socket.recv_from(&mut upstream_buf).await
            } else {
                std::future::pending::<std::io::Result<(usize, SocketAddr)>>().await
            }
        };

        tokio::select! {
            biased;
            _ = until.clone().cancelled_owned() => break,
            _ = sweep.tick() => {}
            read = client_read => {
                match read {
                    Ok((len, from)) => {
                        if len == 0 {
                            state.note_invalid_packet().await;
                            continue;
                        }
                        if !is_client_source(&assoc, from) {
                            // 同一个 socket 上回来的上游响应：直接走回程，别去解析 SOCKS5 头。
                            send_upstream_datagram_to_client(
                                &state,
                                &assoc,
                                &owner,
                                from,
                                &client_buf[..len],
                            )
                            .await;
                            continue;
                        }
                        if from != assoc.effective_client_endpoint()
                            && assoc.learn_client_endpoint(from)
                        {
                            // 客户端发 UDP 用的 socket 与 TCP 控制连接不是同一个（源端口不同），
                            // RFC 1928 也允许它在请求里填 0.0.0.0:0。所以第一个数据报的来源就是
                            // "回程该发到哪里"的真相，学下来。
                            info!(
                                assoc = assoc.id,
                                learned = %from,
                                declared = %assoc.client_endpoint,
                                "UDP relay learned the client endpoint (回程将发往此处)"
                            );
                        }
                        let dest = match parse_udp_request(&client_buf[..len]) {
                            Ok(request) => request,
                            Err(reason) => {
                                debug!("invalid SOCKS5 UDP request on assoc {}: {}", assoc.id, reason);
                                state.note_invalid_packet().await;
                                continue;
                            }
                        };
                        let target = match dest.target.to_socket_addr().await {
                            Ok(addr) => addr,
                            Err(err) => {
                                debug!("UDP relay assoc {} target not routable: {}", assoc.id, err);
                                state.note_invalid_packet().await;
                                continue;
                            }
                        };
                        assoc.touch(now_ms());

                        let datagram = Bytes::copy_from_slice(&client_buf[..len]);
                        engine.feed_dir(assoc.id, from, target, &datagram, now_ms(), true);

                        // 必须切到 `len`：直接用 `[header_len..]` 会把整个 64KB 接收缓冲
                        // （含上一包的残渣）都发出去，Windows 直接回 WSAEMSGSIZE(10040)
                        // "消息大于内部消息缓冲区"，转发永远出不去 —— 真机症状同样是
                        // "客户端一个包都到不了服务器"。
                        let payload = &client_buf[dest.header_len()..len];
                        match assoc.socket.send_to(payload, target).await {
                            Ok(written) => {
                                assoc.add_bytes_up(written);
                                state.note_udp_up(len).await;
                                state.note_udp_relay(written).await;
                            }
                            Err(err) => {
                                debug!("UDP relay=forward to {} failed: {}", target, err);
                                state.note_tcp_relay_failure().await;
                            }
                        }
                    }
                    Err(err) => {
                        // 这条 socket 同时是转发出去的 socket，所以"上一次发包"引发的 ICMP
                        // 回报（Windows os error 10054）也会落在它上面。当成致命错误会让这条
                        // 关联直接停止转发，客户端之后所有数据报都不会再被读取。
                        if is_benign_udp_error(&err) {
                            trace!("UDP relay assoc {} client recv ignored benign error: {}", assoc.id, err);
                            continue;
                        }
                        debug!("UDP relay assoc {} client read failed: {}", assoc.id, err);
                        break;
                    }
                }
            }
            read = upstream_read => {
                match read {
                    Ok((len, from)) => {
                        if len == 0 {
                            state.note_invalid_packet().await;
                            continue;
                        }
                        let client = assoc.effective_client_endpoint();
                        if from == client {
                            // The client's own datagram looped back: not a response.
                            trace!("UDP relay assoc {} saw client-origin datagram", assoc.id);
                            continue;
                        }
                        send_upstream_datagram_to_client(
                            &state,
                            &assoc,
                            &owner,
                            from,
                            &upstream_buf[..len],
                        )
                        .await;
                    }
                    Err(err) => {
                        // UDP 上的"连接被重置/拒绝"几乎都是**上一次发包**引发的 ICMP 端口
                        // 不可达被操作系统回报上来（Windows 是 os error 10054）。这不是
                        // 致命错误：把它当成"循环结束"会让这条关联**永久停止转发**
                        // （客户端之后所有数据报都不会再被读取），真机表现就是游戏中途断网。
                        // 正确做法是忽略它、继续读下一包。
                        if is_benign_udp_error(&err) {
                            trace!("UDP relay assoc {} upstream recv ignored benign error: {}", assoc.id, err);
                            continue;
                        }
                        debug!("UDP relay assoc {} upstream read failed: {}", assoc.id, err);
                        break;
                    }
                }
            }
        }
    }

    table.remove(assoc.client_endpoint);
    debug!(
        "UDP relay ended: assoc={} client={} up={} down={}",
        assoc.id,
        assoc.client_endpoint,
        assoc.bytes_up.load(Ordering::Relaxed),
        assoc.bytes_down.load(Ordering::Relaxed)
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Idle reaper
// ---------------------------------------------------------------------------------------------

/// Cancels and removes every association idle for at least `timeout_ms`; returns the reclaimed ids.
pub async fn reap_idle(state: &AppState, table: &UdpAssociationTable, timeout_ms: u64) -> Vec<u64> {
    let now = now_ms();
    let mut reaped = Vec::new();

    for assoc in table.snapshot() {
        if !assoc.is_idle(now, timeout_ms) {
            continue;
        }
        assoc.cancel();
        let _ = table.remove(assoc.client_endpoint);
        state.note_invalid_packet().await;
        warn!(
            "UDP relay ended: reaped idle association {} (client {}, idle {} ms)",
            assoc.id,
            assoc.client_endpoint,
            assoc.idle_ms(now)
        );
        reaped.push(assoc.id);
    }

    reaped
}

/// Background sweep that keeps the association table free of dead peers.
pub async fn idle_reaper_loop(
    state: AppState,
    table: Arc<UdpAssociationTable>,
    timeout_ms: u64,
    interval_ms: u64,
    cancel: CancellationToken,
    mut until: tokio::sync::broadcast::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("UDP association reaper cancelled");
                break;
            }
            _ = until.recv() => {
                debug!("UDP association reaper exiting on shutdown");
                break;
            }
            _ = ticker.tick() => {
                let reaped = reap_idle(&state, &table, timeout_ms).await;
                if !reaped.is_empty() {
                    debug!(
                        "UDP association reaper reclaimed {} association(s): {:?}",
                        reaped.len(),
                        reaped
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::socks5::handshake::{SocksTarget, ATYP_IPV4};
    use tokio::net::TcpListener;

    /// 这个用例会真跑解析引擎（内部有 worker）并用到进程级计数器，和别的重测试并行容易
    /// 互相干扰；`ios_bridge` 里的 `SINGLETON_LOCK` 是同样的做法。
    static SERIAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
        SERIAL_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 带超时的 `recv_from`：用例卡住时立刻失败，而不是把 cargo test 挂死。
    async fn recv_within(sock: &UdpSocket, buf: &mut [u8]) -> (usize, SocketAddr) {
        tokio::time::timeout(Duration::from_secs(3), sock.recv_from(buf))
            .await
            .expect("3 秒内没收到数据报")
            .expect("recv_from 失败")
    }

    fn assoc(id: u64, endpoint: &str, bound: &str, created: u64) -> UdpAssociation {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind udp");
        socket.set_nonblocking(true).expect("nonblocking");
        let socket =
            UdpSocket::from_std(socket).expect("tokio udp socket from a bound std socket");
        let bound: SocketAddr = bound.parse().expect("bound");
        new_udp_association(
            id,
            endpoint.parse().expect("endpoint"),
            Arc::new(socket),
            bound,
            created,
        )
    }

    #[tokio::test]
    async fn idle_detection_uses_the_last_activity_stamp() {
        let a = assoc(1, "192.168.1.5:40000", "0.0.0.0:2025", 1_000);
        assert_eq!(a.idle_ms(1_000), 0);
        assert!(!a.is_idle(1_000 + UDP_ASSOC_IDLE_TIMEOUT_MS - 1, UDP_ASSOC_IDLE_TIMEOUT_MS));
        assert!(a.is_idle(1_000 + UDP_ASSOC_IDLE_TIMEOUT_MS, UDP_ASSOC_IDLE_TIMEOUT_MS));
        a.touch(2_000);
        assert_eq!(a.idle_ms(2_500), 500);
    }

    #[tokio::test]
    async fn byte_counters_accumulate_both_directions() {
        let a = assoc(2, "192.168.1.6:40001", "0.0.0.0:2025", 0);
        a.add_bytes_up(100);
        a.add_bytes_down(40);
        a.add_bytes_up(1);
        assert_eq!(a.counters(), (101, 40));
        assert_eq!(a.datagram_counts(), (2, 1));
        assert_eq!(a.relayed_bytes(), 141);
    }

    #[tokio::test]
    async fn dropping_the_association_cancels_the_relay_token() {
        let token = {
            let a = assoc(3, "192.168.1.7:40002", "0.0.0.0:2025", 0);
            let token = a.clone_cancel_token();
            assert!(!token.is_cancelled());
            token
        };
        assert!(token.is_cancelled(), "dropping the association cancels the loop");
    }

    #[test]
    fn deadline_from_ms_is_in_the_past_for_old_stamps() {
        let now = tokio::time::Instant::now();
        let past = deadline_from_ms(now_ms().saturating_sub(5_000));
        assert!(past < now);
    }

    #[test]
    fn idle_timeout_matches_the_documented_300s() {
        assert_eq!(UDP_ASSOC_IDLE_TIMEOUT_MS, 300_000);
        assert_eq!(UDP_REAPER_INTERVAL_MS, 15_000);
    }

    /// 端到端回归：客户端用**与 TCP 控制连接不同的源端口**发 UDP，回程必须发到那个 UDP 来源。
    ///
    /// 真机症状：B 机的游戏"代理连上了但一直网络异常"，客户端一个回程报文都收不到。
    /// `per_association_ephemeral` 下客户端按 BND.ADDR:BND.PORT 把数据报发到本关联的临时
    /// 端口，而上游响应也回到同一个 socket，于是三件事都必须成立：
    ///   1) 这个 socket 只能有一个读分支，两个方向在分支里按来源分流（两个分支抢同一个
    ///      socket 会把客户端数据报当成上游响应，直接打回控制连接的端口）；
    ///   2) 回程目标必须是数据报的**真实来源**，不能是 TCP 对端（源端口不同）；
    ///   3) 转发的 payload 必须按实际长度截断（整个 64KB 缓冲发出去 = Windows WSAEMSGSIZE）。
    /// 任何一条破掉，这个用例都会红。
    #[tokio::test]
    async fn udp_relay_replies_to_the_learned_client_endpoint() {
        let _guard = serial_lock();

        // "游戏服务器"：收到什么就回 `ECHO:` 什么。
        let upstream = UdpSocket::bind("127.0.0.1:0").await.expect("bind upstream");
        let upstream_addr = upstream.local_addr().expect("upstream addr");

        // 客户端面 socket —— 就是 BND.ADDR:BND.PORT 广告出去、客户端真正发到的那个。
        let client_face = UdpSocket::bind("127.0.0.1:0").await.expect("bind client face");
        let bnd = client_face.local_addr().expect("bnd");

        // 关联里声明的客户端地址是 TCP 控制连接的对端：与客户端的 UDP 来源端口不同。
        let control = TcpListener::bind("127.0.0.1:0").await.expect("bind control");
        let declared = control.local_addr().expect("control addr");

        let assoc = Arc::new(new_udp_association(
            7,
            declared,
            Arc::new(client_face),
            bnd,
            now_ms(),
        ));
        let cfg = Config {
            // 这台开发机禁止子进程往 %TEMP% 写东西，所以数据目录必须落在工作区内。
            data_directory: crate::testutil::scratch_dir("udp-reply"),
            ..Default::default()
        };
        let state = AppState::new(&cfg, "tok".into());
        let engine = BattleEngine::new(state.clone(), cfg.clone());
        let table = Arc::new(UdpAssociationTable::new());

        // 与生产接线一致：`per_association_ephemeral` 下客户端面 = 本关联的临时 socket。
        let relay = tokio::spawn(relay_udp_with(
            state.clone(),
            engine,
            table,
            assoc.clone(),
            UdpClientSocket::Ephemeral(assoc.socket.clone()),
            assoc.clone_cancel_token(),
        ));

        let client = UdpSocket::bind("127.0.0.1:0").await.expect("bind client");
        let client_addr = client.local_addr().expect("client addr");
        assert_ne!(client_addr, declared, "用例前提：UDP 源端口与 TCP 对端不同");

        // 客户端 -> BND：标准 SOCKS5 UDP 报文（RSV/FRAG/ATYP/DST/PORT + payload）。
        let mut request = vec![0x00, 0x00, 0x00];
        request.extend_from_slice(&SocksTarget::Ip(upstream_addr).encode_addr_port());
        request.extend_from_slice(b"hello-radar");
        client.send_to(&request, bnd).await.expect("client -> bnd");

        // 上游必须收到**只有 payload** 的那 11 个字节（顺带钉住"按长度截断"那条）。
        let mut buf = [0u8; 512];
        let (n, reply_to) = recv_within(&upstream, &mut buf).await;
        assert_eq!(&buf[..n], b"hello-radar", "转发出去的必须正好是 payload");
        upstream
            .send_to(b"ECHO:hello-radar", reply_to)
            .await
            .expect("upstream reply");

        // 回程必须落到客户端的 UDP 源端口，并且是套了 SOCKS5 头的上游响应。
        let (n, _) = recv_within(&client, &mut buf).await;
        assert_eq!(
            &buf[..n],
            &encode_udp_response(upstream_addr, b"ECHO:hello-radar")[..],
            "回程报文必须带 SOCKS5 头，来源地址是上游"
        );
        assert_eq!(buf[3], ATYP_IPV4);
        assert_eq!(
            assoc.effective_client_endpoint(),
            client_addr,
            "回程目标 = 客户端真实 UDP 来源，而不是 TCP 对端"
        );
        assert_eq!(assoc.counters(), (11, 16), "上行 11 字节、下行 16 字节");

        assoc.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), relay).await;
    }

    /// 分流规则：学过真实来源之后就只认精确地址。
    ///
    /// 为什么必须这么严：上游服务器常常与客户端同 IP（局域网里更是必然），放宽成"同 IP 就算
    /// 客户端"会让上游回包被当成客户端数据报，回程永远发不出去。
    #[tokio::test]
    async fn learned_client_endpoint_is_matched_exactly() {
        let a = assoc(9, "127.0.0.1:40000", "0.0.0.0:2025", 0);

        // 还没学习：同 IP 的另一个端口也当作客户端（RFC 1928 允许客户端在请求里填 0.0.0.0:0）
        assert!(is_client_source(&a, "127.0.0.1:40111".parse().unwrap()));
        assert!(!is_client_source(&a, "10.0.0.1:40111".parse().unwrap()));

        // 学习之后：只认这个地址
        assert!(a.learn_client_endpoint("127.0.0.1:40111".parse().unwrap()));
        assert!(is_client_source(&a, "127.0.0.1:40111".parse().unwrap()));
        assert!(!is_client_source(&a, "127.0.0.1:40112".parse().unwrap()));
        assert_eq!(
            a.learned_endpoint(),
            Some("127.0.0.1:40111".parse().unwrap())
        );
    }
}
