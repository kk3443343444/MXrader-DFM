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
use tracing::{debug, trace, warn};

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

        // Only the per-association socket is read here; shared-socket reads belong to the demux.
        let client_read = async {
            if owner.is_shared() {
                std::future::pending::<std::io::Result<(usize, SocketAddr)>>().await
            } else {
                owner.recv_from(&mut client_buf).await
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
                        if from != assoc.client_endpoint {
                            // A stray sender on a per-association socket is not the client.
                            trace!("UDP relay assoc {} ignoring datagram from {}", assoc.id, from);
                            continue;
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

                        let payload = &client_buf[dest.header_len()..];
                        match assoc.socket.send_to(payload, target).await {
                            Ok(written) => {
                                assoc.add_bytes_up(written);
                                state.note_udp_relay(written).await;
                            }
                            Err(err) => {
                                debug!("UDP relay=forward to {} failed: {}", target, err);
                                state.note_tcp_relay_failure().await;
                            }
                        }
                    }
                    Err(err) => {
                        debug!("UDP relay assoc {} client read failed: {}", assoc.id, err);
                        break;
                    }
                }
            }
            read = assoc.socket.recv_from(&mut upstream_buf) => {
                match read {
                    Ok((len, from)) => {
                        if len == 0 {
                            state.note_invalid_packet().await;
                            continue;
                        }
                        if from == assoc.client_endpoint {
                            // The client's own datagram looped back: not a response.
                            trace!("UDP relay assoc {} saw client-origin datagram", assoc.id);
                            continue;
                        }
                        let framed = encode_udp_response(from, &upstream_buf[..len]);
                        assoc.touch(now_ms());
                        match owner.send_to(&framed, assoc.client_endpoint).await {
                            Ok(written) => {
                                assoc.add_bytes_down(len);
                                state.note_udp_down(len).await;
                                state.note_udp_relay(written).await;
                                trace!(
                                    "] UDP response send_to {} (assoc {}, {} bytes)",
                                    assoc.client_endpoint,
                                    assoc.id,
                                    len
                                );
                            }
                            Err(err) => {
                                debug!("UDP response send_to {} failed: {}", assoc.client_endpoint, err);
                                state.note_tcp_relay_failure().await;
                            }
                        }
                    }
                    Err(err) => {
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
}
