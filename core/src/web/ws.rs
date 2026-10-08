//! Radar WebSocket endpoint: `hello` on connect, then a coalesced stream of engine state, a 30 s
//! `diag` heartbeat, `subscribe`/`unsubscribe` control messages and `bye` on shutdown.
//!
//! # Assumed shared API (owned by other modules)
//!
//! ```text
//! crate::config::Config
//!   brand: String, read_only_radar: bool
//!
//! crate::state::AppState
//!   async fn counters(&self) -> SessionCounters
//!   fn read_only(&self) -> bool
//!   async fn broadcast_diag(&self)
//!   fn shutdown_notify(&self) -> tokio::sync::watch::Receiver<bool>   // <-- ASSUMED
//!
//! crate::battle::BattleEngine
//!   async fn radar_state(&self) -> serde_json::Value
//!   fn subscribe_radar(&self) -> tokio::sync::broadcast::Receiver<serde_json::Value>
//! ```
//!
//! # Client contract
//!
//! * Outbound: `{"type":"hello"}`, `{"type":"state"}` (at most [`MAX_STATE_PER_SECOND`] per second,
//!   bursts coalesced so only the newest snapshot survives), `{"type":"diag"}` whenever no state
//!   was sent for [`HEARTBEAT_INTERVAL`], and `{"type":"bye"}` on shutdown.
//! * Inbound: `{"type":"subscribe"}` / `{"type":"unsubscribe"}`; anything else is ignored.
//! * Fairness: one single-task queue of at most [`MAX_OUTBOUND_QUEUE`] messages whose **oldest**
//!   entry is dropped first, so a stalled TCP peer can neither grow memory nor block the engine.
//!   The WebSocket itself is never split, and the queue writer is `poll_ready`-gated, which keeps
//!   backpressure on the engine stream without ever blocking the writer task.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use futures_util::sink::Sink;
use futures_util::stream::Stream;
use axum::response::IntoResponse;
use tokio::sync::broadcast;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, trace, warn};

use crate::battle::BattleEngine;
use crate::config::Config;
use crate::state::AppState;

use super::battle_view::{radar_bye, radar_diag, radar_hello_with_snapshot, radar_state};

/// Maximum `state` messages pushed to one client per second.
pub const MAX_STATE_PER_SECOND: u32 = 20;
/// Smallest gap between two `state` messages for one client.
pub const STATE_MIN_INTERVAL: Duration = Duration::from_millis(1000 / MAX_STATE_PER_SECOND as u64);
/// Heartbeat: emit `{"type":"diag"}` when nothing was sent for this long.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Per-client outbound queue cap; the oldest message is dropped when full.
pub const MAX_OUTBOUND_QUEUE: usize = 64;

/// `GET /ws` handler: upgrades the request and hands the socket to the per-client loop.
pub async fn upgrade(
    ws: WebSocketUpgrade,
    state: AppState,
    engine: BattleEngine,
    cfg: Config,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state, engine, cfg))
}

/// Per-client loop: the single owner of the socket.
pub async fn handle_socket(mut socket: WebSocket, state: AppState, engine: BattleEngine, cfg: Config) {
    // hello carries the engine's current world block when a snapshot is already available.
    let snapshot = engine.radar_state().await;
    let hello = radar_hello_with_snapshot(&state, &cfg, &snapshot);
    if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
        debug!("radar websocket client disappeared before hello");
        return;
    }

    let mut radar = engine.subscribe_radar();
    let mut shutdown = state.shutdown_notify();
    let mut subscribed = true;

    // Outbound queue: newest state replaces the pending slot, diagnostics queue up to the cap.
    let mut queue: VecDeque<Message> = VecDeque::with_capacity(MAX_OUTBOUND_QUEUE);
    // Message handed to the socket arm; it stays here until the sink reports ready.
    let mut outgoing: Option<Message> = None;
    let mut pending_state: Option<String> = None;
    let mut last_state_at = Instant::now() - STATE_MIN_INTERVAL;
    let mut last_send_at = Instant::now();

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut state_pacer = tokio::time::interval(STATE_MIN_INTERVAL);
    state_pacer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first tick of a tokio interval fires immediately; consume it so the pacer starts quiet.
    state_pacer.tick().await;

    debug!("radar websocket client connected (brand={})", cfg.brand);

    loop {
        // Park one queued message for the write side. The socket arm only writes it once the sink
        // reports ready, so a stalled peer leaves the message here instead of blocking this task.
        if outgoing.is_none() && !queue.is_empty() {
            outgoing = queue.pop_front();
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    debug!("radar websocket closing for shutdown");
                    break;
                }
            }
            // Socket arm: one future drives both directions — the parked message goes out when the
            // sink is writable, inbound frames are polled otherwise — so the socket is borrowed
            // exactly once no matter how many messages are queued.
            step = SocketStep { socket: &mut socket, outgoing: &mut outgoing } => {
                match step {
                    SocketPoll::Written => last_send_at = Instant::now(),
                    SocketPoll::WriteFailed(err) => {
                        debug!("radar websocket send failed: {}", err);
                        break;
                    }
                    SocketPoll::Incoming(Some(Ok(Message::Text(text)))) => {
                        if !handle_client_message(&text, &mut subscribed) {
                            trace!("radar websocket client sent an unhandled message");
                        }
                    }
                    SocketPoll::Incoming(Some(Ok(Message::Close(_)))) => break,
                    SocketPoll::Incoming(Some(Ok(Message::Ping(_))))
                    | SocketPoll::Incoming(Some(Ok(Message::Pong(_)))) => {}
                    SocketPoll::Incoming(Some(Ok(_))) => {}
                    SocketPoll::Incoming(Some(Err(err))) => {
                        debug!("radar websocket receive error: {}", err);
                        break;
                    }
                    SocketPoll::Incoming(None) => break,
                }
            }
            update = radar.recv() => {
                match update {
                    Ok(snapshot) => {
                        if subscribed {
                            // Coalesce: only the newest snapshot is kept, never a backlog.
                            pending_state = Some(radar_state(snapshot, &state).to_string());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!("radar broadcast lagged, skipped {} snapshots", skipped);
                        // Re-arm so this client resynchronizes with the live stream.
                        radar = engine.subscribe_radar();
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        debug!("radar broadcast closed");
                        break;
                    }
                }
            }
            _ = state_pacer.tick() => {
                if let Some(payload) = pending_state.take() {
                    if last_state_at.elapsed() >= STATE_MIN_INTERVAL {
                        push_outbound(&mut queue, Message::Text(payload.into()));
                        last_state_at = Instant::now();
                    } else {
                        // Too soon: keep it pending and let the next pacer tick send it.
                        pending_state = Some(payload);
                    }
                }
            }
            _ = heartbeat.tick() => {
                if last_send_at.elapsed() >= HEARTBEAT_INTERVAL && queue.is_empty() {
                    let counters = state.counters().await;
                    push_outbound(&mut queue, Message::Text(radar_diag(counters).to_string().into()));
                    last_send_at = Instant::now();
                    state.broadcast_diag().await;
                }
            }
        }
    }

    let _ = socket.send(Message::Text(radar_bye("shutdown").to_string().into())).await;
    let _ = socket.send(Message::Close(None)).await;
    debug!("radar websocket client disconnected");
}

/// What one poll of the socket produced.
enum SocketPoll {
    /// The parked message left the write slot.
    Written,
    /// The write half failed.
    WriteFailed(axum::Error),
    /// The read half reported a frame, an error, or the end of the stream.
    Incoming(Option<Result<Message, axum::Error>>),
}

/// One step of socket I/O: the message parked in `outgoing` is handed to the sink as soon as the
/// sink reports ready, otherwise the socket is polled for inbound frames.
///
/// The write side is `poll_ready`-gated rather than awaited, so a stalled peer can neither block
/// this task nor drop the parked message, and both directions share one future because the socket
/// has to be borrowed exactly once inside `tokio::select!`.
struct SocketStep<'a> {
    socket: &'a mut WebSocket,
    outgoing: &'a mut Option<Message>,
}

impl Future for SocketStep<'_> {
    type Output = SocketPoll;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<SocketPoll> {
        let this = self.get_mut();
        let mut socket = Pin::new(&mut *this.socket);
        let mut wrote = false;

        if this.outgoing.is_some() {
            match socket.as_mut().poll_ready(cx) {
                Poll::Ready(Ok(())) => {
                    // Ready means the sink accepts a message right now, so moving the parked
                    // message out of the slot cannot lose it.
                    if let Some(message) = this.outgoing.take() {
                        if let Err(err) = socket.as_mut().start_send(message) {
                            return Poll::Ready(SocketPoll::WriteFailed(err));
                        }
                        wrote = true;
                    }
                }
                Poll::Ready(Err(err)) => return Poll::Ready(SocketPoll::WriteFailed(err)),
                Poll::Pending => {}
            }
        }

        // Drain whatever the sink still buffers; this also arms the write waker.
        if let Poll::Ready(Err(err)) = socket.as_mut().poll_flush(cx) {
            return Poll::Ready(SocketPoll::WriteFailed(err));
        }
        if wrote {
            return Poll::Ready(SocketPoll::Written);
        }

        socket.as_mut().poll_next(cx).map(SocketPoll::Incoming)
    }
}

/// Enqueues one outbound message, dropping the oldest entry when the queue is at its cap.
fn push_outbound(queue: &mut VecDeque<Message>, message: Message) {
    if queue.len() >= MAX_OUTBOUND_QUEUE {
        warn!(
            "radar websocket outbound queue full ({} messages); dropping the oldest",
            MAX_OUTBOUND_QUEUE
        );
        queue.pop_front();
    }
    queue.push_back(message);
}

/// Handles `subscribe` / `unsubscribe`; returns false for messages we do not act on.
pub fn handle_client_message(text: &str, subscribed: &mut bool) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    match value.get("type").and_then(|kind| kind.as_str()) {
        Some("subscribe") => {
            *subscribed = true;
            true
        }
        Some("unsubscribe") => {
            *subscribed = false;
            true
        }
        _ => false,
    }
}

/// Builds the heartbeat payload for a counter snapshot (also used by the parent's self-test).
pub fn heartbeat_message(counters: crate::state::SessionCounters) -> String {
    radar_diag(counters).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_rate_limit_is_twenty_per_second() {
        assert_eq!(MAX_STATE_PER_SECOND, 20);
        assert_eq!(STATE_MIN_INTERVAL, Duration::from_millis(50));
        assert_eq!(HEARTBEAT_INTERVAL, Duration::from_secs(30));
        assert_eq!(MAX_OUTBOUND_QUEUE, 64);
    }

    #[test]
    fn subscribe_and_unsubscribe_toggle_the_flag() {
        let mut subscribed = true;
        assert!(handle_client_message(r#"{"type":"unsubscribe"}"#, &mut subscribed));
        assert!(!subscribed);
        assert!(handle_client_message(r#"{"type":"subscribe"}"#, &mut subscribed));
        assert!(subscribed);
        assert!(!handle_client_message(r#"{"type":"nope"}"#, &mut subscribed));
        assert!(!handle_client_message("not json", &mut subscribed));
    }

    #[test]
    fn outbound_queue_drops_the_oldest_message_at_the_cap() {
        let mut queue: VecDeque<Message> = VecDeque::new();
        for index in 0..MAX_OUTBOUND_QUEUE {
            push_outbound(&mut queue, Message::Text(format!("m{index}").into()));
        }
        assert_eq!(queue.len(), MAX_OUTBOUND_QUEUE);

        push_outbound(&mut queue, Message::Text("newest".to_string().into()));
        assert_eq!(queue.len(), MAX_OUTBOUND_QUEUE);
        match queue.front() {
            Some(Message::Text(first)) => assert_eq!(first.as_str(), "m1"),
            other => panic!("unexpected front: {other:?}"),
        }
        match queue.back() {
            Some(Message::Text(last)) => assert_eq!(last.as_str(), "newest"),
            other => panic!("unexpected back: {other:?}"),
        }
    }
}
