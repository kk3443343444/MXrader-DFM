//! Hand-rolled SOCKS5 greeting / request state machine (RFC 1928 + RFC 1929-free, no auth).
//!
//! No external SOCKS crate is used: the whole negotiation is written on top of
//! `tokio::io::{AsyncReadExt, AsyncWriteExt}` so the byte layout can be kept exactly aligned with
//! what the battle client expects (and so the parser mirrors the original sample).
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
//!   fn shutdown_notify(&self) -> tokio::sync::watch::Receiver<bool>     // <-- ASSUMED
//!
//! crate::battle::BattleEngine
//!   fn feed(&self, session: u64, src: SocketAddr, dst: SocketAddr,
//!           payload: &bytes::Bytes, ts_ms: u64)
//!
//! crate::socks5::relay
//!   async fn relay_tcp(state, engine, session_id, upstream: TcpStream, client: TcpStream)
//!         -> anyhow::Result<()>
//!   async fn relay_udp(state, engine, session_id, assoc: UdpAssociation)
//!         -> anyhow::Result<()>
//!   struct UdpAssociation { id, client_endpoint, socket: Arc<UdpSocket>, bound,
//!                           created_ms, last_activity_ms, bytes_up, bytes_down }
//!   fn new_udp_association(id, client_endpoint, socket, bound, created_ms) -> UdpAssociation
//!   fn deadline_from_ms(ms: u64) -> tokio::time::Instant
//! ```
//!
//! # Protocol constants implemented here
//!
//! * VER `0x05`; NMETHODS + METHOD list; METHOD `0x00` (no auth) accepted, everything else `0xFF`.
//! * CMD `0x01` CONNECT, `0x03` UDP ASSOCIATE, `0x02`/others -> REP `0x07`.
//! * ATYP `0x01` IPv4, `0x04` IPv6, `0x03` domain (length-prefixed), else REP `0x08`.
//! * REP `0x00` success, `0x01` general failure, `0x03` net unreachable, `0x07` command not
//!   supported, `0x08` address type not supported. Replies always carry `BND.ADDR/BND.PORT`.
//! * UDP request header `RSV(2) FRAG(1) ATYP ADDR PORT DATA`, `FRAG != 0` is rejected.
//! * Timeouts: 10 s handshake, 10 s upstream connect. Greeting capped at 512 bytes.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{lookup_host, TcpStream, UdpSocket};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use crate::battle::BattleEngine;
use crate::state::AppState;

use super::relay::{self, UdpAssociation};
use super::{now_ms, HandlerCtx};

// ---------------------------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------------------------

/// SOCKS version this endpoint speaks.
pub const SOCKS_VERSION: u8 = 0x05;
/// Address type: IPv4.
pub const ATYP_IPV4: u8 = 0x01;
/// Address type: domain name.
pub const ATYP_DOMAIN: u8 = 0x03;
/// Address type: IPv6.
pub const ATYP_IPV6: u8 = 0x04;
/// Authentication method: no authentication required.
pub const METHOD_NO_AUTH: u8 = 0x00;
/// Authentication method: no acceptable methods.
pub const METHOD_NONE_ACCEPTABLE: u8 = 0xFF;
/// Command: CONNECT.
pub const CMD_CONNECT: u8 = 0x01;
/// Command: BIND (not supported by this endpoint).
pub const CMD_BIND: u8 = 0x02;
/// Command: UDP ASSOCIATE.
pub const CMD_UDP_ASSOCIATE: u8 = 0x03;

/// Reply code: succeeded.
pub const REP_SUCCEEDED: u8 = 0x00;
/// Reply code: general SOCKS server failure.
pub const REP_GENERAL_FAILURE: u8 = 0x01;
/// Reply code: network unreachable.
pub const REP_NET_UNREACHABLE: u8 = 0x03;
/// Reply code: command not supported.
pub const REP_COMMAND_NOT_SUPPORTED: u8 = 0x07;
/// Reply code: address type not supported.
pub const REP_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;

/// Handshake budget for greeting + request (and the upstream CONNECT).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest accepted greeting: VER + NMETHODS + 510 method bytes.
pub const MAX_GREETING_LEN: usize = 512;
/// Largest SOCKS5 UDP datagram we will look at.
pub const MAX_UDP_DATAGRAM: usize = 65_535;

// ---------------------------------------------------------------------------------------------
// Targets and UDP framing
// ---------------------------------------------------------------------------------------------

/// A SOCKS5 target address, in whichever form the client sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocksTarget {
    /// ATYP 0x01.
    Ip(SocketAddr),
    /// ATYP 0x03, not yet resolved.
    Domain(String, u16),
}

impl SocksTarget {
    /// Destination port, regardless of address form.
    pub fn port(&self) -> u16 {
        match self {
            SocksTarget::Ip(addr) => addr.port(),
            SocksTarget::Domain(_, port) => *port,
        }
    }

    /// Resolved socket address. Domain targets use `tokio::net::lookup_host`.
    pub async fn to_socket_addr(&self) -> Result<SocketAddr> {
        match self {
            SocksTarget::Ip(addr) => Ok(*addr),
            SocksTarget::Domain(host, port) => {
                let mut iter = lookup_host((host.as_str(), *port))
                    .await
                    .map_err(|err| anyhow!("DNS resolution for {host}:{port} failed: {err}"))?;
                iter.next()
                    .ok_or_else(|| anyhow!("DNS resolution for {host}:{port} returned no address"))
            }
        }
    }

    /// ATYP byte for this target.
    pub fn atyp(&self) -> u8 {
        match self {
            SocksTarget::Ip(SocketAddr::V4(_)) => ATYP_IPV4,
            SocksTarget::Ip(SocketAddr::V6(_)) => ATYP_IPV6,
            SocksTarget::Domain(_, _) => ATYP_DOMAIN,
        }
    }

    /// `ATYP ADDR PORT` encoding without the leading RSV/FRAG.
    pub fn encode_addr_port(&self) -> Vec<u8> {
        match self {
            SocksTarget::Ip(SocketAddr::V4(addr)) => {
                let mut out = Vec::with_capacity(7);
                out.push(ATYP_IPV4);
                out.extend_from_slice(&addr.ip().octets());
                out.extend_from_slice(&addr.port().to_be_bytes());
                out
            }
            SocksTarget::Ip(SocketAddr::V6(addr)) => {
                let mut out = Vec::with_capacity(19);
                out.push(ATYP_IPV6);
                out.extend_from_slice(&addr.ip().octets());
                out.extend_from_slice(&addr.port().to_be_bytes());
                out
            }
            SocksTarget::Domain(host, port) => {
                let bytes = host.as_bytes();
                let trimmed = if bytes.len() > u8::MAX as usize {
                    &bytes[..u8::MAX as usize]
                } else {
                    bytes
                };
                let mut out = Vec::with_capacity(2 + trimmed.len() + 2);
                out.push(ATYP_DOMAIN);
                out.push(trimmed.len() as u8);
                out.extend_from_slice(trimmed);
                out.extend_from_slice(&port.to_be_bytes());
                out
            }
        }
    }
}

/// Decoded SOCKS5 UDP request datagram (`RSV(2) FRAG(1) ATYP ADDR PORT DATA`).
#[derive(Debug, Clone)]
pub struct UdpRequest<'a> {
    /// Destination carried in the header.
    pub target: SocksTarget,
    /// Payload after the header.
    pub payload: &'a [u8],
    /// Number of header bytes (RSV+FRAG+ATYP+ADDR+PORT).
    pub header_bytes: usize,
}

impl<'a> UdpRequest<'a> {
    /// Header length in bytes.
    pub fn header_len(&self) -> usize {
        self.header_bytes
    }
}

/// Decodes a SOCKS5 UDP request header. `FRAG != 0` and unsupported ATYPs are errors.
pub fn parse_udp_request(buf: &[u8]) -> Result<UdpRequest<'_>> {
    if buf.len() < 4 {
        return Err(anyhow!("datagram too short for a SOCKS5 UDP header"));
    }
    if buf[2] != 0 {
        return Err(anyhow!("fragmented SOCKS5 UDP datagrams are not supported"));
    }

    let atyp = buf[3];
    let mut offset = 4usize;

    let target = match atyp {
        ATYP_IPV4 => {
            if buf.len() < offset + 4 + 2 {
                return Err(anyhow!("truncated IPv4 UDP header"));
            }
            let ip = Ipv4Addr::new(buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]);
            offset += 4;
            let port = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
            offset += 2;
            SocksTarget::Ip(SocketAddr::new(IpAddr::V4(ip), port))
        }
        ATYP_IPV6 => {
            if buf.len() < offset + 16 + 2 {
                return Err(anyhow!("truncated IPv6 UDP header"));
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&buf[offset..offset + 16]);
            offset += 16;
            let port = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
            offset += 2;
            SocksTarget::Ip(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(octets)),
                port,
            ))
        }
        ATYP_DOMAIN => {
            if buf.len() < offset + 1 {
                return Err(anyhow!("truncated domain length in UDP header"));
            }
            let name_len = buf[offset] as usize;
            offset += 1;
            if buf.len() < offset + name_len + 2 {
                return Err(anyhow!("truncated domain name in UDP header"));
            }
            let host = match std::str::from_utf8(&buf[offset..offset + name_len]) {
                Ok(host) => host.to_string(),
                Err(_) => return Err(anyhow!("domain in UDP header is not valid UTF-8")),
            };
            offset += name_len;
            let port = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
            offset += 2;
            SocksTarget::Domain(host, port)
        }
        other => {
            return Err(anyhow!("unsupported ATYP 0x{:02X} in UDP header", other));
        }
    };

    Ok(UdpRequest {
        target,
        payload: &buf[offset..],
        header_bytes: offset,
    })
}

/// Prepends the SOCKS5 UDP request header to an upstream datagram (reply path, FRAG = 0).
pub fn encode_udp_response(sender: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 22);
    out.extend_from_slice(&[0x00, 0x00, 0x00]);
    out.extend_from_slice(&SocksTarget::Ip(sender).encode_addr_port());
    out.extend_from_slice(payload);
    out
}

// ---------------------------------------------------------------------------------------------
// Greeting / request parsing
// ---------------------------------------------------------------------------------------------

/// Parsed client greeting (`VER NMETHODS METHODS...`).
#[derive(Debug, Clone)]
pub struct Greeting {
    /// Version byte as sent by the client.
    pub version: u8,
    /// Offered authentication methods.
    pub methods: Vec<u8>,
}

/// Parsed SOCKS5 request (`VER CMD RSV ATYP ADDR PORT`).
#[derive(Debug, Clone)]
pub struct SocksRequest {
    /// Version byte as sent by the client.
    pub version: u8,
    /// CMD byte (0x01 / 0x02 / 0x03).
    pub command: u8,
    /// Requested target (or the client hint address for UDP ASSOCIATE).
    pub target: SocksTarget,
}

/// Reads and validates the greeting. Enforces the 512-byte cap and the version byte.
pub async fn read_greeting<R>(reader: &mut R) -> Result<Greeting>
where
    R: AsyncReadExt + Unpin,
{
    let mut head = [0u8; 2];
    reader
        .read_exact(&mut head)
        .await
        .map_err(|err| anyhow!("reading greeting header: {err}"))?;

    let version = head[0];
    let nmethods = head[1] as usize;

    if version != SOCKS_VERSION {
        warn!("unsupported SOCKS version: {} (0x{:02X})", version, version);
        return Err(anyhow!("unsupported SOCKS version: {}", version));
    }
    if nmethods == 0 {
        return Err(anyhow!("greeting offered no authentication methods"));
    }
    if nmethods + 2 > MAX_GREETING_LEN {
        return Err(anyhow!(
            "greeting exceeds {} bytes (NMETHODS = {})",
            MAX_GREETING_LEN,
            nmethods
        ));
    }

    let mut methods = vec![0u8; nmethods];
    reader
        .read_exact(&mut methods)
        .await
        .map_err(|err| anyhow!("reading greeting methods: {err}"))?;

    trace!("%SOCKS5 greeting version={} methods={:?}", version, methods);
    Ok(Greeting { version, methods })
}

/// Chooses the authentication method: no-auth when offered, otherwise `0xFF`.
pub fn negotiate_method(greeting: &Greeting) -> u8 {
    if greeting.methods.contains(&METHOD_NO_AUTH) {
        METHOD_NO_AUTH
    } else {
        METHOD_NONE_ACCEPTABLE
    }
}

/// Reads one SOCKS5 request. Returns the request even for unsupported CMDs so the caller can
/// answer with `REP_COMMAND_NOT_SUPPORTED` instead of dropping the socket silently.
pub async fn read_request<R>(reader: &mut R) -> Result<SocksRequest>
where
    R: AsyncReadExt + Unpin,
{
    let mut head = [0u8; 4];
    reader
        .read_exact(&mut head)
        .await
        .map_err(|err| anyhow!("reading request header: {err}"))?;

    let version = head[0];
    let command = head[1];
    // head[2] is RSV and has to be 0x00 per RFC 1928; be lenient like the original sample.
    let atyp = head[3];

    if version != SOCKS_VERSION {
        warn!("unsupported SOCKS version: {}", version);
        return Err(anyhow!("unsupported SOCKS version: {}", version));
    }

    let target = match atyp {
        ATYP_IPV4 => {
            let mut octets = [0u8; 4];
            let mut port = [0u8; 2];
            reader
                .read_exact(&mut octets)
                .await
                .map_err(|err| anyhow!("reading request IPv4 address: {err}"))?;
            reader
                .read_exact(&mut port)
                .await
                .map_err(|err| anyhow!("reading request IPv4 port: {err}"))?;
            SocksTarget::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(octets)),
                u16::from_be_bytes(port),
            ))
        }
        ATYP_IPV6 => {
            let mut octets = [0u8; 16];
            let mut port = [0u8; 2];
            reader
                .read_exact(&mut octets)
                .await
                .map_err(|err| anyhow!("reading request IPv6 address: {err}"))?;
            reader
                .read_exact(&mut port)
                .await
                .map_err(|err| anyhow!("reading request IPv6 port: {err}"))?;
            SocksTarget::Ip(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(octets)),
                u16::from_be_bytes(port),
            ))
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            reader
                .read_exact(&mut len)
                .await
                .map_err(|err| anyhow!("reading request domain length: {err}"))?;
            let name_len = len[0] as usize;
            let mut name = vec![0u8; name_len];
            reader
                .read_exact(&mut name)
                .await
                .map_err(|err| anyhow!("reading request domain name: {err}"))?;
            let mut port = [0u8; 2];
            reader
                .read_exact(&mut port)
                .await
                .map_err(|err| anyhow!("reading request domain port: {err}"))?;
            let host = match String::from_utf8(name) {
                Ok(host) => host,
                Err(_) => return Err(anyhow!("request domain name is not valid UTF-8")),
            };
            SocksTarget::Domain(host, u16::from_be_bytes(port))
        }
        other => {
            return Err(anyhow!("unsupported address type: 0x{:02X}", other));
        }
    };

    Ok(SocksRequest {
        version,
        command,
        target,
    })
}

/// Encodes a SOCKS5 reply (`VER REP RSV ATYP BND.ADDR BND.PORT`).
pub fn encode_reply(reply: u8, bound: SocketAddr) -> Vec<u8> {
    let mut out = Vec::with_capacity(22);
    out.push(SOCKS_VERSION);
    out.push(reply);
    out.push(0x00);
    out.extend_from_slice(&SocksTarget::Ip(bound).encode_addr_port());
    out
}

/// Writes a SOCKS5 reply, mapping write failures to a log-and-continue error.
pub async fn send_reply<W>(writer: &mut W, reply: u8, bound: SocketAddr) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let bytes = encode_reply(reply, bound);
    writer
        .write_all(&bytes)
        .await
        .map_err(|err| anyhow!("writing SOCKS5 reply: {err}"))
}

// ---------------------------------------------------------------------------------------------
// UDP association table
// ---------------------------------------------------------------------------------------------

/// Demultiplexing counters for the shared SOCKS-port UDP socket (diagnostics only).
#[derive(Debug, Default, Clone, Copy)]
pub struct UdpDemuxStats {
    /// Datagrams attributed by exact client endpoint match.
    pub exact_hits: u64,
    /// Datagrams attributed by single-client IP fallback.
    pub ip_fallback_hits: u64,
    /// Datagrams dropped because the source IP has more than one association.
    pub ambiguous_drops: u64,
    /// Datagrams dropped because nothing matched.
    pub unmatched_drops: u64,
}

#[derive(Debug)]
struct ClientIpIndex {
    endpoint: SocketAddr,
    ambiguous: bool,
}

/// All live UDP associations, indexed for both demultiplexing directions.
///
/// * `shared` - packets arriving on the SOCKS-port UDP socket, keyed by client endpoint and by
///   client IP with a single-client safety check.
/// * `ephemeral` - packets arriving on a per-association ephemeral socket.
pub struct UdpAssociationTable {
    shared: std::sync::Mutex<HashMap<SocketAddr, Arc<UdpAssociation>>>,
    by_client_ip: std::sync::Mutex<HashMap<IpAddr, ClientIpIndex>>,
    ephemeral: std::sync::Mutex<HashMap<SocketAddr, Arc<UdpAssociation>>>,
    ambiguous_ip_count: AtomicU64,
    /// Highest observed number of sockets talking to real destinations (diagnostics).
    udp_outbound_sockets: AtomicU64,
}

impl Default for UdpAssociationTable {
    fn default() -> Self {
        Self::new()
    }
}

impl UdpAssociationTable {
    /// Empty table.
    pub fn new() -> Self {
        Self {
            shared: std::sync::Mutex::new(HashMap::new()),
            by_client_ip: std::sync::Mutex::new(HashMap::new()),
            ephemeral: std::sync::Mutex::new(HashMap::new()),
            ambiguous_ip_count: AtomicU64::new(0),
            udp_outbound_sockets: AtomicU64::new(0),
        }
    }

    /// Registers an association and returns the number of live associations.
    pub fn insert(&self, assoc: UdpAssociation) -> usize {
        let client_endpoint = assoc.client_endpoint;
        let client_ip = client_endpoint.ip();
        let bound = assoc.bound;
        let arc = Arc::new(assoc);

        let count = {
            let mut shared = match self.shared.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            shared.insert(client_endpoint, arc.clone());
            shared.len()
        };

        {
            let mut ip_index = match self.by_client_ip.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match ip_index.get_mut(&client_ip) {
                Some(entry) if entry.endpoint != client_endpoint => {
                    entry.ambiguous = true;
                    self.ambiguous_ip_count.fetch_add(1, Ordering::Relaxed);
                    debug!(
                        "; UDP relays are per-association: client IP {} now has several endpoints",
                        client_ip
                    );
                }
                Some(_) => {}
                None => {
                    ip_index.insert(
                        client_ip,
                        ClientIpIndex {
                            endpoint: client_endpoint,
                            ambiguous: false,
                        },
                    );
                }
            }
        }

        {
            let mut ephemeral = match self.ephemeral.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            ephemeral.insert(bound, arc.clone());
        }

        let live = self.udp_outbound_sockets.load(Ordering::Relaxed).max(count as u64);
        self.udp_outbound_sockets.store(live, Ordering::Relaxed);
        count
    }

    /// Removes an association by client endpoint.
    pub fn remove(&self, client_endpoint: SocketAddr) -> Option<Arc<UdpAssociation>> {
        let (removed, remaining) = {
            let mut shared = match self.shared.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let removed = shared.remove(&client_endpoint);
            (removed, shared.len())
        };

        if let Some(assoc) = removed.as_ref() {
            let mut ephemeral = match self.ephemeral.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            ephemeral.remove(&assoc.bound);

            let mut ip_index = match self.by_client_ip.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(entry) = ip_index.get(&client_endpoint.ip()) {
                if entry.endpoint == client_endpoint {
                    ip_index.remove(&client_endpoint.ip());
                }
            }
        }

        self.udp_outbound_sockets
            .store(remaining as u64, Ordering::Relaxed);
        removed
    }

    /// Removes an association by its id (used by the idle reaper).
    pub fn remove_by_id(&self, id: u64) -> Option<SocketAddr> {
        let endpoint = {
            let shared = match self.shared.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            shared
                .iter()
                .find(|(_, assoc)| assoc.id == id)
                .map(|(endpoint, _)| *endpoint)
        };
        match endpoint {
            Some(endpoint) => self.remove(endpoint).map(|_| endpoint),
            None => None,
        }
    }

    /// Live association count.
    pub fn len(&self) -> usize {
        match self.shared.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// True when no association is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of client IPs that currently have more than one association.
    pub fn ambiguous_ip_count(&self) -> u64 {
        self.ambiguous_ip_count.load(Ordering::Relaxed)
    }

    /// Highest observed count of outbound (ephemeral) UDP sockets.
    pub fn udp_outbound_sockets(&self) -> u64 {
        self.udp_outbound_sockets.load(Ordering::Relaxed)
    }

    /// All live associations, oldest first by creation time.
    pub fn snapshot(&self) -> Vec<Arc<UdpAssociation>> {
        let shared = match self.shared.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut list: Vec<Arc<UdpAssociation>> = shared.values().cloned().collect();
        list.sort_by_key(|assoc| assoc.created_ms);
        list
    }

    /// Resolves a datagram source seen on the shared SOCKS-port UDP socket.
    ///
    /// Exact endpoint match first (this survives a client rebinding its local port later), then a
    /// single-client IP fallback, which is disabled as soon as a second client from that IP
    /// registers (that is the `per_client_endpoint_isolated` guarantee).
    pub fn resolve_shared_source(
        &self,
        src: SocketAddr,
        stats: &mut UdpDemuxStats,
    ) -> Option<Arc<UdpAssociation>> {
        if let Some(assoc) = self.get(&src) {
            stats.exact_hits = stats.exact_hits.saturating_add(1);
            return Some(assoc);
        }

        let fallback = {
            let ip_index = match self.by_client_ip.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match ip_index.get(&src.ip()) {
                Some(entry) if !entry.ambiguous => Some(entry.endpoint),
                Some(_) => {
                    stats.ambiguous_drops = stats.ambiguous_drops.saturating_add(1);
                    None
                }
                None => None,
            }
        };

        match fallback {
            Some(endpoint) => {
                let assoc = self.get(&endpoint);
                if assoc.is_some() {
                    stats.ip_fallback_hits = stats.ip_fallback_hits.saturating_add(1);
                } else {
                    stats.unmatched_drops = stats.unmatched_drops.saturating_add(1);
                }
                assoc
            }
            None => {
                if stats.ambiguous_drops == 0 {
                    stats.unmatched_drops = stats.unmatched_drops.saturating_add(1);
                }
                None
            }
        }
    }

    /// Resolves a datagram source seen on a per-association ephemeral socket.
    pub fn resolve_ephemeral_source(&self, src: SocketAddr) -> Option<Arc<UdpAssociation>> {
        let endpoint = {
            let ip_index = match self.by_client_ip.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match ip_index.get(&src.ip()) {
                Some(entry) if !entry.ambiguous => Some(entry.endpoint),
                _ => None,
            }
        }?;
        self.get(&endpoint)
    }

    /// Looks an association up by client endpoint.
    pub fn get(&self, client_endpoint: &SocketAddr) -> Option<Arc<UdpAssociation>> {
        let shared = match self.shared.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        shared.get(client_endpoint).cloned()
    }

    /// Looks an association up by the ephemeral socket it owns.
    pub fn get_by_bound(&self, bound: &SocketAddr) -> Option<Arc<UdpAssociation>> {
        let ephemeral = match self.ephemeral.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        ephemeral.get(bound).cloned()
    }
}

// ---------------------------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------------------------

/// Outcome of handling one request, so the caller owns the relay lifetime.
enum RequestOutcome {
    /// CONNECT accepted: the upstream socket must be relayed.
    Connected(TcpStream),
    /// Reply already sent (or the client is gone); nothing more to do.
    Replied,
}

/// Serves one accepted TCP client: greeting, method negotiation, request, relay.
///
/// Never panics and never propagates a client-caused failure to the listener - everything is
/// logged and the session is cleaned up.
pub async fn serve_client(
    ctx: HandlerCtx,
    session_id: u64,
    peer: SocketAddr,
    mut client: TcpStream,
    cancel: CancellationToken,
) {
    let result = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handle_client(&ctx, session_id, peer, &mut client, &cancel),
    )
    .await;

    match result {
        Ok(Ok(())) => debug!("%SOCKS5 session {} finished", session_id),
        Ok(Err(err)) => debug!("%SOCKS5 session {} ended: {}", session_id, err),
        Err(_) => warn!(
            "%SOCKS5 session {} timed out after {:?}",
            session_id, HANDSHAKE_TIMEOUT
        ),
    }

    let _ = client.shutdown().await;
    // Unregister the UDP association (if any) and the session counter.
    detach_association(&ctx, peer);
    ctx.state.remove_session(session_id).await;
}

/// Removes any UDP association registered for this client endpoint and stops its relay.
fn detach_association(ctx: &HandlerCtx, peer: SocketAddr) {
    if let Some(assoc) = ctx.assoc.remove(peer) {
        assoc.cancel();
        debug!(
            "UDP association {} detached from client {}",
            assoc.id, peer
        );
    }
}

/// Handshake body; the outer [`serve_client`] wraps it in the 10 s budget.
async fn handle_client(
    ctx: &HandlerCtx,
    session_id: u64,
    peer: SocketAddr,
    client: &mut TcpStream,
    cancel: &CancellationToken,
) -> Result<()> {
    let greeting = match read_greeting(client).await {
        Ok(greeting) => greeting,
        Err(err) => {
            debug!("%SOCKS5 greeting from {} rejected: {}", peer, err);
            return Err(err);
        }
    };

    let method = negotiate_method(&greeting);
    client
        .write_all(&[SOCKS_VERSION, method])
        .await
        .map_err(|err| anyhow!("writing method selection: {err}"))?;

    if method == METHOD_NONE_ACCEPTABLE {
        // Rejected: the caller closes the socket right after this returns.
        debug!(
            "%SOCKS5 no acceptable auth method for {} (offered {:?})",
            peer, greeting.methods
        );
        return Ok(());
    }

    let request = match read_request(client).await {
        Ok(request) => request,
        Err(err) => {
            // An unsupported ATYP is answerable; anything else just tears the session down.
            let reply = if err.to_string().starts_with("unsupported address type") {
                REP_ADDRESS_TYPE_NOT_SUPPORTED
            } else {
                REP_GENERAL_FAILURE
            };
            let _ = send_reply(client, reply, local_addr_of(client, ctx)).await;
            return Err(err);
        }
    };

    match request.command {
        CMD_CONNECT => {
            if !ctx.flags.tcp_connect {
                send_reply(client, REP_COMMAND_NOT_SUPPORTED, local_addr_of(client, ctx)).await?;
                return Ok(());
            }

            match connect_upstream(ctx, client, &request.target).await {
                Ok(RequestOutcome::Connected(upstream)) => {
                    debug!(
                        "] TCP relay start: session={} peer={} target={:?}",
                        session_id, peer, request.target
                    );
                    // Hand the relay an owned client socket so both halves live in the relay
                    // task; the session socket itself is shut down by the caller afterwards.
                    let client_owned = try_clone_stream(client)
                        .map_err(|err| anyhow!("cloning client socket for relay: {err}"))?;
                    if let Err(err) = relay::relay_tcp(
                        ctx.state.clone(),
                        ctx.engine.clone(),
                        session_id,
                        upstream,
                        client_owned,
                    )
                    .await
                    {
                        warn!("] TCP relay ended: session={} error={}", session_id, err);
                        ctx.state.note_tcp_relay_failure().await;
                    }
                    Ok(())
                }
                Ok(RequestOutcome::Replied) => Ok(()),
                Err(err) => {
                    debug!("%SOCKS5 CONNECT from {} failed: {}", peer, err);
                    Ok(())
                }
            }
        }
        CMD_UDP_ASSOCIATE => handle_udp_associate(ctx, session_id, peer, client, cancel).await,
        CMD_BIND => {
            debug!("%SOCKS5 BIND is not supported (session {})", session_id);
            send_reply(client, REP_COMMAND_NOT_SUPPORTED, local_addr_of(client, ctx)).await?;
            Ok(())
        }
        other => {
            debug!("%SOCKS5 unknown command 0x{:02X} (session {})", other, session_id);
            send_reply(client, REP_COMMAND_NOT_SUPPORTED, local_addr_of(client, ctx)).await?;
            Ok(())
        }
    }
}

/// Duplicates a connected socket so the relay can own one handle while the session keeps using the
/// original. `tokio::net::TcpStream` exposes no `try_clone`, so the handle is duplicated through
/// `std::net::TcpStream` and wrapped back into the reactor.
fn try_clone_stream(stream: &TcpStream) -> std::io::Result<TcpStream> {
    // `from_raw_*` takes ownership of a handle that is only borrowed here: `forget` keeps the
    // original socket open, so the duplicate owns the only new handle.
    #[cfg(unix)]
    let duplicated = {
        use std::os::unix::io::{AsRawFd, FromRawFd};
        let borrowed = unsafe { std::net::TcpStream::from_raw_fd(stream.as_raw_fd()) };
        let duplicated = borrowed.try_clone();
        std::mem::forget(borrowed);
        duplicated
    };
    #[cfg(windows)]
    let duplicated = {
        use std::os::windows::io::{AsRawSocket, FromRawSocket};
        let borrowed = unsafe { std::net::TcpStream::from_raw_socket(stream.as_raw_socket()) };
        let duplicated = borrowed.try_clone();
        std::mem::forget(borrowed);
        duplicated
    };

    let duplicated = duplicated?;
    // The duplicate refers to the same socket, but tokio requires non-blocking handles.
    duplicated.set_nonblocking(true)?;
    TcpStream::from_std(duplicated)
}

/// Best-effort reply filler: the local address of the client socket, then the bound interface.
fn local_addr_of(client: &TcpStream, ctx: &HandlerCtx) -> SocketAddr {
    match client.local_addr() {
        Ok(addr) => addr,
        Err(_) => SocketAddr::new(ctx.interface, ctx.port),
    }
}

/// 把"通配地址"换成客户端能真正打到的本机地址，端口保持原样。
///
/// 只做端口/地址的替换，不碰别的：BND.PORT 必须是客户端真正该发的那个端口
/// （同端口模式下是 SOCKS 端口，否则是临时端口），所以不能被覆盖。
fn reachable_bnd_addr(client_facing: SocketAddr, fallback: SocketAddr) -> SocketAddr {
    if client_facing.ip().is_unspecified() && !fallback.ip().is_unspecified() {
        SocketAddr::new(fallback.ip(), client_facing.port())
    } else {
        client_facing
    }
}

/// Dials the requested CONNECT target and answers with the matching reply code.
async fn connect_upstream(
    ctx: &HandlerCtx,
    client: &mut TcpStream,
    target: &SocksTarget,
) -> Result<RequestOutcome> {
    let reply_addr = local_addr_of(client, ctx);

    let resolved = match target.to_socket_addr().await {
        Ok(addr) => addr,
        Err(err) => {
            debug!("; CONNECT target resolution failed: {}", err);
            let _ = send_reply(client, REP_NET_UNREACHABLE, reply_addr).await;
            return Err(err);
        }
    };

    match tokio::time::timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(resolved)).await {
        Ok(Ok(stream)) => {
            if let Err(err) = stream.set_nodelay(true) {
                trace!("CONNECT upstream set_nodelay failed: {}", err);
            }
            let bound = stream.local_addr().unwrap_or(reply_addr);
            send_reply(client, REP_SUCCEEDED, bound).await?;
            Ok(RequestOutcome::Connected(stream))
        }
        Ok(Err(err)) => {
            debug!("; CONNECT to {} refused: {}", resolved, err);
            let _ = send_reply(client, REP_NET_UNREACHABLE, reply_addr).await;
            Err(anyhow!("connecting upstream {resolved}: {err}"))
        }
        Err(_) => {
            debug!("; CONNECT to {} timed out", resolved);
            let _ = send_reply(client, REP_NET_UNREACHABLE, reply_addr).await;
            Err(anyhow!("connecting upstream {resolved} timed out"))
        }
    }
}

/// RFC 1928 UDP ASSOCIATE: pick the client-facing port, register the association, relay.
async fn handle_udp_associate(
    ctx: &HandlerCtx,
    session_id: u64,
    peer: SocketAddr,
    client: &mut TcpStream,
    cancel: &CancellationToken,
) -> Result<()> {
    if !ctx.flags.udp_associate {
        send_reply(client, REP_COMMAND_NOT_SUPPORTED, local_addr_of(client, ctx)).await?;
        return Ok(());
    }

    // `transport.socks5.udp_concurrent_associations` is a hard cap: refuse politely instead of
    // silently stealing a slot from a live session.
    if ctx.assoc.len() >= ctx.flags.max_udp_associations as usize {
        warn!(
            "UDP ASSOCIATE refused for {}: association cap {} reached",
            peer, ctx.flags.max_udp_associations
        );
        send_reply(client, REP_GENERAL_FAILURE, local_addr_of(client, ctx)).await?;
        return Ok(());
    }

    // Upstream (destination-facing) socket. `per_association_ephemeral` spawns one per
    // association; any other mode shares the SOCKS-port socket through the same abstraction.
    let upstream_socket = if ctx.flags.per_association_ephemeral {
        match bind_udp_like(&ctx.shared_udp).await {
            Ok(sock) => Arc::new(sock),
            Err(err) => {
                warn!("UDP ASSOCIATE could not open an ephemeral socket: {}", err);
                send_reply(client, REP_GENERAL_FAILURE, local_addr_of(client, ctx)).await?;
                return Ok(());
            }
        }
    } else {
        ctx.shared_udp.clone()
    };

    let bound = match upstream_socket.local_addr() {
        Ok(addr) => addr,
        Err(err) => {
            warn!("UDP ASSOCIATE local_addr failed: {}", err);
            send_reply(client, REP_GENERAL_FAILURE, local_addr_of(client, ctx)).await?;
            return Ok(());
        }
    };

    // BND.ADDR/BND.PORT is where the client sends its datagrams. With `udp_same_port` the SOCKS
    // port is reused; otherwise the ephemeral port may differ from the SOCKS port.
    let client_facing = if ctx.flags.udp_same_port || !ctx.flags.per_association_ephemeral {
        ctx.shared_udp.local_addr().unwrap_or(bound)
    } else {
        bound
    };

    // BND.ADDR 绝不能是通配地址（0.0.0.0 / ::）。
    // 很多 iOS 客户端（小火箭、Hiddify 等）**直接拿 BND.ADDR 当 UDP 发送目标**，
    // 回 0.0.0.0 的话包会在客户端自己那台机器上打转，永远到不了本机 —— 真机症状是
    // "代理连上了，但游戏一直提示网络异常"。这里换成"客户端这条 TCP 连接到达本机时的
    // 地址"（本机局域网 IP + 同一端口），对"照抄 BND.ADDR"和"用连接时的服务器地址"
    // 两类客户端都能正常工作。
    let client_facing = reachable_bnd_addr(client_facing, local_addr_of(client, ctx));

    let assoc = relay::new_udp_association(session_id, peer, upstream_socket, bound, now_ms());
    if ctx.flags.separate_dual_stack {
        debug!(
            "; UDP relays are per-association, dual-stack client endpoint {} isolated per client",
            peer
        );
    }
    if !ctx.flags.client_endpoint_isolated {
        debug!(
            "; UDP relays are per-association, shared NAT mapping requested for {}",
            peer
        );
    }

    let live = ctx.assoc.insert(assoc.clone());
    debug!(
        "relay={} UDP association {} bound {} client {} (live={}, mode={})",
        ctx.flags.udp_relay_mode, session_id, bound, peer, live, ctx.flags.nat_mapping_label
    );

    send_reply(client, REP_SUCCEEDED, client_facing).await?;

    // The association lives exactly as long as the control connection (or until reaped).
    let relay_state = ctx.state.clone();
    let relay_engine = ctx.engine.clone();
    let relay_table = ctx.assoc.clone();
    // The relay task owns one handle to the table; the cleanup below still needs ours.
    let relay_table_task = relay_table.clone();
    let shared = ctx.shared_udp.clone();
    let relay_assoc = Arc::new(assoc.clone());
    let relay_cancel = cancel.child_token();
    let relay_until = relay_assoc.clone_cancel_token();
    let relay_handle = tokio::spawn(async move {
        let result = relay::relay_udp_with(
            relay_state.clone(),
            relay_engine,
            relay_table_task.clone(),
            relay_assoc,
            relay::UdpClientSocket::Shared(shared),
            relay_until,
        )
        .await;
        if let Err(err) = result {
            debug!("UDP relay ended: {} (session {})", err, session_id);
        }
    });

    // The control TCP connection carries no payload once the association exists; reading it is
    // how we learn that the client went away.
    let mut probe = [0u8; 256];
    let shutdown = ctx.state.shutdown_notify();
    let mut shutdown = shutdown;
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = cancel.clone().cancelled_owned() => break,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            read = client.read(&mut probe) => {
                match read {
                    Ok(0) => break,
                    Ok(n) => {
                        // Datagrams are expected on UDP, not on the control connection: ignore.
                        trace!("%SOCKS5 UDP control connection got {} stray bytes", n);
                    }
                    Err(_) => break,
                }
            }
        }
    }

    assoc.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), relay_handle).await;
    let _ = relay_table.remove(peer);
    debug!("UDP relay ended: client {} closed the control connection", peer);
    Ok(())
}

/// Opens a UDP socket mirroring the address family of a reference socket.
pub(crate) async fn bind_udp_like(reference: &UdpSocket) -> Result<UdpSocket> {
    let local = reference
        .local_addr()
        .map_err(|err| anyhow!("reading reference UDP address: {err}"))?;

    let bind_addr = match local {
        SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };

    UdpSocket::bind(bind_addr)
        .await
        .map_err(|err| anyhow!("binding ephemeral UDP socket on {bind_addr}: {err}"))
}

/// Number of outbound (destination-facing) UDP sockets currently opened by the table.
pub fn udp_outbound_sockets(table: &UdpAssociationTable) -> u64 {
    table.udp_outbound_sockets()
}

// ---------------------------------------------------------------------------------------------
// Tests: framing is pure, so it can be verified without a network.
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_prefers_no_auth_and_rejects_the_rest() {
        let greet = Greeting {
            version: SOCKS_VERSION,
            methods: vec![0x02, 0x00],
        };
        assert_eq!(negotiate_method(&greet), METHOD_NO_AUTH);

        let rejected = Greeting {
            version: SOCKS_VERSION,
            methods: vec![0x02, 0x80],
        };
        assert_eq!(negotiate_method(&rejected), METHOD_NONE_ACCEPTABLE);
    }

    #[test]
    fn udp_request_round_trips_ipv4() {
        let datagram = [
            0x00, 0x00, 0x00, // RSV RSV FRAG
            ATYP_IPV4, 192, 168, 1, 23, 0x1F, 0x90, // ADDR PORT
            0xDE, 0xAD, 0xBE, 0xEF,
        ];
        let parsed = parse_udp_request(&datagram).expect("valid header");
        assert_eq!(
            parsed.target,
            SocksTarget::Ip("192.168.1.23:8080".parse().expect("addr"))
        );
        assert_eq!(parsed.header_len(), 10);
        assert_eq!(parsed.payload, &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn udp_request_rejects_fragments_and_bad_atyp() {
        let fragmented = [0x00, 0x00, 0x01, ATYP_IPV4, 1, 2, 3, 4, 0, 53];
        assert!(parse_udp_request(&fragmented).is_err());

        let bad_atyp = [0x00, 0x00, 0x00, 0x09, 1, 2, 3, 4, 0, 53];
        assert!(parse_udp_request(&bad_atyp).is_err());

        let short = [0x00, 0x00, 0x00];
        assert!(parse_udp_request(&short).is_err());
    }

    #[test]
    fn udp_request_decodes_domain_targets() {
        let mut datagram = vec![0x00, 0x00, 0x00, ATYP_DOMAIN, 11];
        datagram.extend_from_slice(b"example.com");
        datagram.extend_from_slice(&443u16.to_be_bytes());
        datagram.extend_from_slice(b"hi");

        let parsed = parse_udp_request(&datagram).expect("valid header");
        assert_eq!(
            parsed.target,
            SocksTarget::Domain("example.com".to_string(), 443)
        );
        assert_eq!(parsed.header_len(), 18);
        assert_eq!(parsed.payload, b"hi");
    }

    #[test]
    fn udp_response_prepends_the_sender_header() {
        let sender: SocketAddr = "10.0.0.7:2025".parse().expect("addr");
        let framed = encode_udp_response(sender, b"xyz");
        assert_eq!(&framed[..3], &[0x00, 0x00, 0x00]);
        assert_eq!(framed[3], ATYP_IPV4);
        assert_eq!(&framed[4..8], &[10, 0, 0, 7]);
        assert_eq!(&framed[8..10], &2025u16.to_be_bytes());
        assert_eq!(&framed[10..], b"xyz");
    }

    #[test]
    fn reply_encoding_fills_bnd_addr_and_port() {
        let bound: SocketAddr = "192.168.1.23:2025".parse().expect("addr");
        let reply = encode_reply(REP_SUCCEEDED, bound);
        assert_eq!(reply[0], SOCKS_VERSION);
        assert_eq!(reply[1], REP_SUCCEEDED);
        assert_eq!(reply[2], 0x00);
        assert_eq!(reply[3], ATYP_IPV4);
        assert_eq!(&reply[4..8], &[192, 168, 1, 23]);
        assert_eq!(&reply[8..10], &2025u16.to_be_bytes());
        assert_eq!(reply.len(), 10);
    }

    /// 回归测试：UDP ASSOCIATE 的 BND.ADDR 不能是通配地址。
    ///
    /// 真机症状：B 机的游戏"连上了代理但一直网络异常"。原因是我们回的 BND.ADDR 是
    /// 0.0.0.0（共享 UDP socket / 临时 socket 都绑在通配地址上），而 iOS 客户端会直接
    /// 拿这个地址当 UDP 发送目标 → 包在 B 机自己身上打转，永远到不了接收器。
    #[test]
    fn bnd_addr_never_advertises_the_wildcard_address() {
        let port = 2026u16;
        let reachable: SocketAddr = "192.168.1.50:2025".parse().unwrap();

        // 通配 → 换成本机可达地址，端口必须是"客户端该发的那个端口"
        let fixed = reachable_bnd_addr(format!("0.0.0.0:{port}").parse().unwrap(), reachable);
        assert_eq!(fixed, format!("192.168.1.50:{port}").parse::<SocketAddr>().unwrap());
        let fixed6 = reachable_bnd_addr(format!("[::]:{port}").parse().unwrap(), reachable);
        assert_eq!(fixed6, format!("192.168.1.50:{port}").parse::<SocketAddr>().unwrap());

        // 已经是具体地址 → 原样返回，不做任何改写
        let concrete: SocketAddr = "10.0.0.9:40000".parse().unwrap();
        assert_eq!(reachable_bnd_addr(concrete, reachable), concrete);

        // 连 fallback 都是通配（极端情况）→ 至少别 panic，保持原值
        let both: SocketAddr = "0.0.0.0:2025".parse().unwrap();
        assert_eq!(reachable_bnd_addr(both, both), both);
    }

    #[test]
    fn greeting_length_cap_is_enforced() {
        // NMETHODS = 0xFF would need 257 bytes; the cap only rejects a bogus huge count.
        assert_eq!(MAX_GREETING_LEN, 512);
        // The documented maximum is 510 method bytes, so every NMETHODS a single length byte can
        // express (0..=255) stays inside the cap.
        assert!((MAX_GREETING_LEN - 2) >= u8::MAX as usize);
    }
}
