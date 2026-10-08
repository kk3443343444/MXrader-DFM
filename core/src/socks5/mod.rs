//! SOCKS5 plaintext endpoint: one TCP listener plus one UDP socket on the same port.
//!
//! # Matched shared API (verified against the sibling modules in this crate)
//!
//! ```text
//! crate::config::Config
//!   data_directory: PathBuf
//!   brand: String
//!   endpoint: EndpointConfig { interface: String, ports: PortsConfig { range: [u16; 2] } }
//!   transport: TransportConfig {
//!       socks5: Socks5TransportConfig {
//!           tcp_connect: bool, udp_associate: bool, udp_same_port: bool,
//!           udp_single_direction_burst: u32, udp_concurrent_associations: u32,
//!           udp_relay_mode: UdpRelayMode },
//!       udp_nat_mapping: UdpNatMapping, authentication: String, encryption: String }
//!   session_model: SessionModel, parser_async: bool, read_only_radar: bool,
//!   loot_parsing_enabled: bool, collection_policy: CollectionPolicy,
//!   diagnostics: DiagnosticsConfig { protocol_capture, max_capture_mb, max_capture_seconds },
//!   admin_token: String, card: CardConfig { code: Option<String>, activation_url: String }
//!
//! crate::config::UdpRelayMode::{PerAssociationEphemeral, SharedPort}
//! crate::config::UdpNatMapping::{PerClientEndpointIsolated, PerClientEndpointSeparateDualStack}
//! ```
//!
//! This module reads only `config.endpoint.*` and `config.transport.*`; the transport knobs are
//! normalized by [`TransportFlags`] so the handshake and both relays cannot disagree.
//!
//! ```text
//! crate::state::AppState  (cheaply cloneable Arc handle)
//!   async fn add_tcp_session(&self, peer: SocketAddr) -> u64
//!   async fn remove_session(&self, id: u64)
//!   async fn counters(&self) -> SessionCounters
//!   async fn note_udp_up(&self, n: usize)
//!   async fn note_udp_down(&self, n: usize)
//!   async fn note_invalid_packet(&self)
//!   async fn note_tcp_relay_failure(&self)
//!   async fn note_udp_relay(&self, bytes: usize)
//!   async fn status_json(&self) -> serde_json::Value
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
//!   fn shutdown_notify(&self) -> tokio::sync::watch::Receiver<bool>
//! ```
//!
//! ```text
//! crate::battle::BattleEngine
//!   fn feed(&self, session: u64, src: SocketAddr, dst: SocketAddr,
//!           payload: &bytes::Bytes, ts_ms: u64)                     // non-blocking queue push
//!   fn feed_dir(&self, session, src, dst, payload, ts_ms, c2s: bool) // direction-aware variant
//!   async fn radar_state(&self) -> serde_json::Value
//!   fn subscribe_radar(&self) -> tokio::sync::broadcast::Receiver<serde_json::Value>
//! ```
//!
//! Shutdown contract: the endpoint listens on `state.shutdown_notify()`; if that channel is not
//! wired yet, the internal [`CancellationToken`] (fired by [`Socks5Listener::shutdown`] or by
//! dropping the handle) still stops every task. No `unwrap()` sits on a network path: a single
//! broken client degrades to a log line and the listeners keep running.

pub mod handshake;
pub mod relay;

pub use handshake::*;
pub use relay::*;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::battle::BattleEngine;
use crate::config::{Config, UdpNatMapping, UdpRelayMode};
use crate::state::AppState;

/// Broadcast channel depth for coordinating shutdown across the listener tasks.
const SHUTDOWN_BROADCAST_CAPACITY: usize = 32;

/// Fallback bind interface when the configured one does not parse as an IP literal.
const DEFAULT_INTERFACE: &str = "0.0.0.0";

/// Hard ceiling for one UDP datagram (the protocol limit, independent of the configured burst).
pub const MAX_UDP_DATAGRAM: usize = 65_535;

/// Milliseconds since the UNIX epoch, clamped to 0 for pre-epoch clocks.
pub(crate) fn now_ms() -> u64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(_) => 0,
    }
}

/// Transport knobs shared by the handshake and the relays.
///
/// Normalized once at startup from `config.transport`, so every task clones a small value and the
/// handshake, the TCP relay and the UDP relay can never disagree about the active policy.
#[derive(Debug, Clone)]
pub struct TransportFlags {
    /// `transport.socks5.tcp_connect` - serve CMD 0x01.
    pub tcp_connect: bool,
    /// `transport.socks5.udp_associate` - serve CMD 0x03.
    pub udp_associate: bool,
    /// `transport.socks5.udp_relay_mode`: one ephemeral upstream socket per association
    /// (`per_association_ephemeral`) or a shared listener socket (`shared_port`).
    pub per_association_ephemeral: bool,
    /// Human readable relay mode for logs (`per_association_ephemeral` / `shared_port`).
    pub udp_relay_mode: &'static str,
    /// `transport.socks5.udp_same_port` - when true the ASSOCIATE reply reuses the SOCKS port.
    pub udp_same_port: bool,
    /// `transport.udp_nat_mapping`: keep every client endpoint isolated
    /// (`per_client_endpoint_isolated`) or split v4/v6 per client
    /// (`per_client_endpoint_separate_dual_stack`).
    pub client_endpoint_isolated: bool,
    /// True when the mapping asks for a separate dual-stack client endpoint per client.
    pub separate_dual_stack: bool,
    /// `transport.socks5.udp_concurrent_associations` - hard cap on live associations.
    pub max_udp_associations: u32,
    /// `transport.socks5.udp_single_direction_burst` - per-datagram burst ceiling in bytes.
    pub udp_single_direction_burst: u32,
    /// `transport.udp_nat_mapping` for logs.
    pub nat_mapping_label: &'static str,
}

impl Default for TransportFlags {
    fn default() -> Self {
        Self {
            tcp_connect: true,
            udp_associate: true,
            per_association_ephemeral: true,
            udp_relay_mode: "per_association_ephemeral",
            udp_same_port: false,
            client_endpoint_isolated: true,
            separate_dual_stack: false,
            max_udp_associations: 8,
            udp_single_direction_burst: 512 * 1024,
            nat_mapping_label: "per_client_endpoint_isolated",
        }
    }
}

impl TransportFlags {
    /// Reads the transport knobs out of the config. This is the single place that decides the
    /// active UDP policy, and it can only ever copy what the config says.
    pub fn from_config(cfg: &Config) -> Self {
        let socks5 = &cfg.transport.socks5;

        let (per_association_ephemeral, relay_mode) = match socks5.udp_relay_mode {
            UdpRelayMode::PerAssociationEphemeral => (true, "per_association_ephemeral"),
            UdpRelayMode::SharedPort => (false, "shared_port"),
        };
        let (separate_dual_stack, nat_mapping) = match cfg.transport.udp_nat_mapping {
            UdpNatMapping::PerClientEndpointIsolated => {
                (false, "per_client_endpoint_isolated")
            }
            UdpNatMapping::PerClientEndpointSeparateDualStack => {
                (true, "per_client_endpoint_separate_dual_stack")
            }
        };

        Self {
            tcp_connect: socks5.tcp_connect,
            udp_associate: socks5.udp_associate,
            per_association_ephemeral,
            udp_relay_mode: relay_mode,
            udp_same_port: socks5.udp_same_port,
            client_endpoint_isolated: true,
            separate_dual_stack,
            max_udp_associations: socks5.udp_concurrent_associations.max(1),
            udp_single_direction_burst: socks5.udp_single_direction_burst.max(1024),
            nat_mapping_label: nat_mapping,
        }
    }

    /// Largest accepted UDP payload (header included) for one datagram.
    pub fn max_udp_datagram(&self) -> usize {
        (self.udp_single_direction_burst as usize).clamp(1024, MAX_UDP_DATAGRAM)
    }
}

/// Runtime context handed to every TCP connection handler and to the UDP demultiplexer.
#[derive(Clone)]
pub(crate) struct HandlerCtx {
    pub(crate) state: AppState,
    pub(crate) engine: BattleEngine,
    #[allow(dead_code)]
    pub(crate) cfg: Arc<Config>,
    pub(crate) flags: Arc<TransportFlags>,
    /// The single UDP socket bound to the SOCKS port: the client-facing receive path and also
    /// the source address used for every reply sent back to a client endpoint.
    pub(crate) shared_udp: Arc<UdpSocket>,
    /// Association table shared by the handshake (insert) and the relays (lookup/reap).
    pub(crate) assoc: Arc<UdpAssociationTable>,
    /// Lifetime demultiplexing counters for the shared UDP socket (diagnostics only).
    pub(crate) stats: Arc<std::sync::Mutex<handshake::UdpDemuxStats>>,
    /// Bound interface, used for IPv4/IPv6 mirroring when spawning ephemeral sockets.
    pub(crate) interface: IpAddr,
    /// Bound SOCKS port (TCP and UDP).
    pub(crate) port: u16,
}

/// The SOCKS5 endpoint created by [`run`].
///
/// Dropping the handle cancels the endpoint even when [`Socks5Listener::shutdown`] is not called.
pub struct Socks5Listener {
    port: u16,
    local_addr: SocketAddr,
    cancel: CancellationToken,
    accept_task: Mutex<Option<JoinHandle<()>>>,
}

impl Socks5Listener {
    /// The port that actually got bound (first bindable port in `ports.range`).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Local address of the TCP listener, e.g. `0.0.0.0:2025`.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// A cloneable handle that cancels the endpoint from anywhere.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Cancels the endpoint and waits (bounded) for the accept task to finish.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        let handle = self.accept_task.lock().await.take();
        if let Some(handle) = handle {
            if tokio::time::timeout(std::time::Duration::from_secs(5), handle)
                .await
                .is_err()
            {
                warn!("SOCKS5 listener task did not stop within 5s; leaving it detached");
            }
        }
    }
}

impl Drop for Socks5Listener {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Binds the dual TCP+UDP SOCKS5 endpoint on `0.0.0.0:<primary_port>` and serves until shutdown.
///
/// Port selection walks `cfg.endpoint.ports.range` in ascending order and keeps the first port
/// where BOTH the TCP listener and the UDP socket bind. A half-open port is released and the
/// search continues, so `primary_port` always carries both transports.
pub async fn run(state: AppState, engine: BattleEngine, cfg: Config) -> Result<Socks5Listener> {
    let [range_lo, range_hi] = cfg.endpoint.ports.range;

    if range_lo == 0 {
        return Err(anyhow!(
            "endpoint.ports.range starts at 0; refusing to bind an ephemeral port"
        ));
    }

    let interface = parse_interface(&cfg.endpoint.interface);
    let candidates = port_candidates(range_lo, range_hi);

    let mut bound: Option<(TcpListener, Arc<UdpSocket>, SocketAddr)> = None;
    let mut last_error: Option<String> = None;

    for port in &candidates {
        let bind_addr = SocketAddr::new(interface, *port);

        let tcp = match TcpListener::bind(bind_addr).await {
            Ok(listener) => listener,
            Err(err) => {
                debug!("SOCKS5 TCP port {} unavailable: {}", port, err);
                last_error = Some(format!("TCP bind {bind_addr}: {err}"));
                continue;
            }
        };

        // The UDP socket for the SAME port. No SO_REUSEPORT: a failed UDP bind rejects the port.
        let udp = match UdpSocket::bind(bind_addr).await {
            Ok(sock) => sock,
            Err(err) => {
                debug!("SOCKS5 UDP port {} unavailable: {}", port, err);
                last_error = Some(format!("UDP bind {bind_addr}: {err}"));
                drop(tcp);
                continue;
            }
        };

        let local = match tcp.local_addr() {
            Ok(addr) => addr,
            Err(err) => {
                return Err(anyhow!("reading SOCKS5 listener local address: {err}"));
            }
        };

        bound = Some((tcp, Arc::new(udp), local));
        break;
    }

    let (tcp_listener, shared_udp, local_addr) = match bound {
        Some(parts) => parts,
        None => {
            let detail = last_error.unwrap_or_else(|| "no attempt recorded".to_string());
            return Err(anyhow!(
                "no bindable port for the SOCKS5 endpoint in {range_lo}..={range_hi} ({detail})"
            ));
        }
    };

    let port = local_addr.port();
    let flags = Arc::new(TransportFlags::from_config(&cfg));

    let ctx = HandlerCtx {
        state: state.clone(),
        engine: engine.clone(),
        cfg: Arc::new(cfg),
        flags: flags.clone(),
        shared_udp: shared_udp.clone(),
        assoc: Arc::new(UdpAssociationTable::new()),
        stats: Arc::new(std::sync::Mutex::new(handshake::UdpDemuxStats::default())),
        interface,
        port,
    };

    // Shutdown plumbing: a watch channel from AppState (preferred) plus a CancellationToken and
    // an internal broadcast that every loop selects on.
    let cancel = CancellationToken::new();
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(SHUTDOWN_BROADCAST_CAPACITY);
    let watch_rx = state.shutdown_notify();

    info!(
        "3SOCKS5 plaintext endpoint listening on TCP 0.0.0.0:{}",
        port
    );
    info!("UDP ASSOCIATE on SOCKS port {}", port);
    info!(
        "SOCKS5 endpoint armed: tcp_connect={} udp_associate={} relay_mode={} nat_mapping={} same_port={}",
        flags.tcp_connect,
        flags.udp_associate,
        flags.udp_relay_mode,
        flags.nat_mapping_label,
        flags.udp_same_port
    );

    let tcp_ctx = ctx.clone();
    let tcp_cancel = cancel.clone();
    let accept_tx = shutdown_tx.clone();
    let accept_task = tokio::spawn(async move {
        let until = accept_tx.subscribe();
        tcp_accept_loop(tcp_listener, tcp_ctx, tcp_cancel, until).await;
    });

    let udp_ctx = ctx.clone();
    let udp_cancel = cancel.clone();
    let udp_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        let until = udp_tx.subscribe();
        udp_demux_loop(udp_ctx, udp_cancel, until).await;
    });

    let reaper_ctx = ctx.clone();
    let reaper_cancel = cancel.clone();
    let reaper_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        let until = reaper_tx.subscribe();
        relay::idle_reaper_loop(
            reaper_ctx.state,
            reaper_ctx.assoc,
            relay::UDP_ASSOC_IDLE_TIMEOUT_MS,
            relay::UDP_REAPER_INTERVAL_MS,
            reaper_cancel,
            until,
        )
        .await;
    });

    // Bridge the AppState watch channel onto the internal broadcast so a state-driven shutdown
    // is race-free against the local token and every loop only listens to one signal.
    let shutdown_bridge = shutdown_tx.clone();
    let bridge_cancel = cancel.clone();
    let mut bridge_watch = watch_rx;
    tokio::spawn(async move {
        loop {
            if *bridge_watch.borrow() {
                break;
            }
            tokio::select! {
                changed = bridge_watch.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                _ = bridge_cancel.cancelled() => break,
            }
        }
        let _ = shutdown_bridge.send(());
    });

    Ok(Socks5Listener {
        port,
        local_addr,
        cancel,
        accept_task: Mutex::new(Some(accept_task)),
    })
}

/// TCP accept loop. `biased` makes cancellation win over a pending accept.
async fn tcp_accept_loop(
    listener: TcpListener,
    ctx: HandlerCtx,
    cancel: CancellationToken,
    mut until: broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                info!("SOCKS5 TCP accept loop cancelled");
                break;
            }
            _ = until.recv() => {
                info!("SOCKS5 TCP accept loop exiting on shutdown");
                break;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        if let Err(err) = stream.set_nodelay(true) {
                            debug!("SOCKS5 set_nodelay failed for {}: {}", peer, err);
                        }
                        let session = ctx.state.add_tcp_session(peer).await;
                        debug!("%SOCKS5 accepted TCP session {} from {}", session, peer);
                        let ctx = ctx.clone();
                        let child = cancel.child_token();
                        tokio::spawn(async move {
                            handshake::serve_client(ctx, session, peer, stream, child).await;
                        });
                    }
                    Err(err) => {
                        // A failed accept must never kill the listener.
                        error!("SOCKS5 TCP accept error: {}", err);
                        ctx.state.note_tcp_relay_failure().await;
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                }
            }
        }
    }
}

/// Single receive loop over the shared SOCKS-port UDP socket.
///
/// Datagrams are attributed to a registered association first; only then is the SOCKS5 UDP
/// request header decoded, the datagram handed to the engine and the payload forwarded.
async fn udp_demux_loop(
    ctx: HandlerCtx,
    cancel: CancellationToken,
    mut until: broadcast::Receiver<()>,
) {
    let mut buf = vec![0u8; MAX_UDP_DATAGRAM];

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                info!("UDP ASSOCIATE receive loop cancelled");
                break;
            }
            _ = until.recv() => {
                info!("UDP ASSOCIATE receive loop exiting on shutdown");
                break;
            }
            received = ctx.shared_udp.recv_from(&mut buf) => {
                let (len, from) = match received {
                    Ok(pair) => pair,
                    Err(err) => {
                        error!("UDP receive error on SOCKS port {}: {}", ctx.port, err);
                        ctx.state.note_tcp_relay_failure().await;
                        continue;
                    }
                };

                if len == 0 {
                    ctx.state.note_invalid_packet().await;
                    continue;
                }

                let assoc = {
                    // The demux counters are diagnostic only; a poisoned lock must not stop traffic.
                    let mut stats = match ctx.stats.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    ctx.assoc.resolve_shared_source(from, &mut stats)
                };
                let assoc = match assoc {
                    Some(assoc) => assoc,
                    None => {
                        // Not a registered UDP-ASSOCIATE client: count it, never forward it.
                        debug!(
                            "; UDP relays are per-association, dropping datagram from unregistered endpoint {}",
                            from
                        );
                        ctx.state.note_invalid_packet().await;
                        continue;
                    }
                };
                let (dest, header_len) = match parse_udp_request(&buf[..len]) {
                    Ok(request) => {
                        // Read the header length before the target moves out of the request.
                        let header_len = request.header_len();
                        (request.target, header_len)
                    }
                    Err(reason) => {
                        debug!("invalid SOCKS5 UDP request from {}: {}", from, reason);
                        ctx.state.note_invalid_packet().await;
                        continue;
                    }
                };

                assoc.touch(now_ms());

                let target_addr = match dest.to_socket_addr().await {
                    Ok(addr) => addr,
                    Err(err) => {
                        debug!("SOCKS5 UDP destination is not routable: {}", err);
                        ctx.state.note_invalid_packet().await;
                        continue;
                    }
                };

                // Hand the whole datagram (header included) to the battle parser. feed_dir pushes
                // onto an async queue by contract, so parsing never delays forwarding below.
                let datagram = bytes::Bytes::copy_from_slice(&buf[..len]);
                let src = from;
                ctx.engine
                    .feed_dir(assoc.id, src, target_addr, &datagram, now_ms(), true);

                ctx.state.note_udp_up(len).await;

                // "先转发，后解析": the forwarding itself is off the receive path so a slow or
                // unreachable destination cannot stall other associations.
                let socket = assoc.socket.clone();
                let state = ctx.state.clone();
                let payload = datagram.slice(header_len..);
                tokio::spawn(async move {
                    match socket.send_to(&payload, target_addr).await {
                        Ok(written) => state.note_udp_relay(written).await,
                        Err(err) => {
                            debug!("UDP relay forward to {} failed: {}", target_addr, err);
                            state.note_tcp_relay_failure().await;
                        }
                    }
                });
            }
        }
    }
}

/// Normalizes the configured interface string into an IP we can bind.
fn parse_interface(raw: &str) -> IpAddr {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    }
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return ip;
    }
    if let Some(host) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return ip;
        }
    }
    // Hostnames such as `localhost` are resolved once at startup.
    if let Some(ip) = (trimmed, 0u16)
        .to_socket_addrs()
        .ok()
        .and_then(|mut iter| iter.next())
        .map(|addr| addr.ip())
    {
        return ip;
    }
    warn!(
        "endpoint.interface {:?} is not an IP literal; falling back to {}",
        raw, DEFAULT_INTERFACE
    );
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

/// Ascending list of candidate ports, with the range boundaries normalized.
pub(crate) fn port_candidates(lo: u16, hi: u16) -> Vec<u16> {
    let (start, end) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    (start..=end).collect()
}

/// Loopback radar URL for a given port (used by the iOS shell probe and by tests).
pub fn loopback_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/battle.html")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_candidates_is_ascending_and_normalized() {
        assert_eq!(port_candidates(2025, 2028), vec![2025, 2026, 2027, 2028]);
        assert_eq!(port_candidates(2028, 2025), vec![2025, 2026, 2027, 2028]);
        assert_eq!(port_candidates(2025, 2025), vec![2025]);
    }

    #[test]
    fn interface_parsing_handles_literals_and_garbage() {
        assert_eq!(parse_interface("0.0.0.0"), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(parse_interface("::1"), "::1".parse::<IpAddr>().expect("ipv6"));
        assert_eq!(parse_interface("[::1]"), "::1".parse::<IpAddr>().expect("ipv6"));
        // Garbage falls back to the wildcard IPv4 interface instead of failing.
        assert!(parse_interface("definitely-not-an-interface").is_ipv4());
    }

    #[test]
    fn transport_flag_defaults_track_interfaces_md() {
        let flags = TransportFlags::default();
        assert!(flags.tcp_connect);
        assert!(flags.udp_associate);
        assert!(flags.per_association_ephemeral, "one ephemeral upstream socket per association");
        assert!(flags.client_endpoint_isolated);
        assert!(!flags.udp_same_port);
        assert!(!flags.separate_dual_stack);
        assert_eq!(flags.udp_relay_mode, "per_association_ephemeral");
        assert_eq!(flags.nat_mapping_label, "per_client_endpoint_isolated");
        assert_eq!(flags.max_udp_associations, 8);
        assert!(flags.max_udp_datagram() >= 1024);
    }
}
