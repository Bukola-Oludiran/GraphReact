//! **M16-Backend Phase 7.** WebSocket + SSE live event stream
//! substrate. Two pieces:
//!
//! 1. [`WebSocketSink`] — a [`TelemetrySink`] that fans every scheduler
//!    event onto a `tokio::sync::broadcast::Sender<TelemetryEvent>` so
//!    one-or-more axum handlers can subscribe and forward.
//! 2. [`ws_handler`] / [`sse_handler`] — axum route handlers that bind
//!    a fresh [`broadcast::Receiver`], decode `?node` / `?type` /
//!    `?since` filters, and forward matching events to the connected
//!    client. Slow consumers see [`broadcast::error::RecvError::Lagged`]
//!    → server emits a [`TelemetryEvent::Resync`] divider, increments
//!    `grx_observer_subscribers_kicked_total`, and closes the
//!    connection. The client refetches state via REST and reconnects.
//!
//! ## Hot-path discipline
//!
//! [`WebSocketSink::on_*`] runs from the scheduler hot path. Each call:
//!
//! - One owned-event allocation (the [`TelemetryEvent`] variant —
//!   typically 2-4 `String` clones plus an enum tag).
//! - One `broadcast::Sender::send` — atomic on the shared ring; never
//!   blocks even if every receiver is slow (broadcast drops oldest).
//! - One `Counter::inc` for `grx_observer_subscribers_kicked_total` if
//!   the broadcast reports a `Lagged` event ON THE SUBSCRIBER side
//!   (handled inside the handler, not the sink — sink is fire-and-forget).
//!
//! Per-event allocation cost is the trade-off for owned events; the
//! plan-locked broadcast capacity (1024) gives subscribers slack to
//! drain at WS-frame rate without dropping under bursty workloads.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::Stream;
use graphreact_core::telemetry::{
    HotReload, MessageDelivered, NodeEvaluateEnd, NodeEvaluateStart, NodeMounted, NodeUnmounted,
    QueueDepths, RemoteMessage, SchedulerTickEnd, SchedulerTickStart, SupervisorDecisionMade,
    TelemetrySink, TopologyEventOccurred,
};
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::events::{
    from_hot_reload, from_message_delivered, from_node_evaluate_end, from_node_evaluate_start,
    from_node_mounted, from_node_unmounted, from_queue_depths, from_remote_message,
    from_supervisor_decision, from_tick_end, from_tick_start, from_topology_event, TelemetryEvent,
};
use crate::management_api::ApiState;

/// Plan-locked broadcast channel capacity. Sized for ~10 ms of typical
/// scheduler throughput at 100 events/ms before slow subscribers are
/// kicked. M-Prod scope tunes per deploy.
pub const BROADCAST_CAPACITY: usize = 1024;

/// `WebSocketSink: TelemetrySink` — fan-out fanin point. Owns the
/// `broadcast::Sender` and the kicked-subscriber counter.
///
/// Cloning is cheap (`Arc`-backed) so the daemon installs one sink and
/// hands clones to every consumer that needs to subscribe directly.
#[derive(Clone)]
pub struct WebSocketSink {
    inner: Arc<Inner>,
}

struct Inner {
    tx: broadcast::Sender<TelemetryEvent>,
    /// Counter of subscribers kicked due to broadcast lag — exposed
    /// to Prometheus via `grx_observer_subscribers_kicked_total`.
    /// Per-event cost is one atomic store on the slow-consumer path
    /// only; steady-state sink has zero atomic touch.
    kicked: AtomicU64,
    /// Counter of currently-connected subscribers. Increments on
    /// connect, decrements on disconnect. Exposed via
    /// `grx_observer_subscribers_active`.
    active: AtomicU64,
}

impl Default for WebSocketSink {
    fn default() -> Self {
        Self::new(BROADCAST_CAPACITY)
    }
}

impl WebSocketSink {
    /// Build with a custom broadcast capacity. Plan-locked default at
    /// [`BROADCAST_CAPACITY`]; tests may want a smaller capacity to
    /// trigger lag deterministically.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self {
            inner: Arc::new(Inner {
                tx,
                kicked: AtomicU64::new(0),
                active: AtomicU64::new(0),
            }),
        }
    }

    /// Subscribe a fresh receiver. Each handler invocation builds one
    /// of these.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<TelemetryEvent> {
        self.inner.tx.subscribe()
    }

    /// Cumulative slow-subscriber kick counter (for Prometheus
    /// `grx_observer_subscribers_kicked_total`).
    #[must_use]
    pub fn kicked_total(&self) -> u64 {
        self.inner.kicked.load(Ordering::Relaxed)
    }

    /// Subscriber count gauge value.
    #[must_use]
    pub fn active_subscribers(&self) -> u64 {
        self.inner.active.load(Ordering::Relaxed)
    }

    /// Hand-roll a [`TelemetryEvent::Resync`] event onto the broadcast.
    /// Mostly useful for tests; production resync flows through the
    /// per-handler kick path.
    pub fn announce_resync(&self, missed: u64) {
        let _ = self.inner.tx.send(TelemetryEvent::Resync {
            ts_ms: unix_ms(),
            missed,
        });
    }

    fn record_kick(&self) {
        self.inner.kicked.fetch_add(1, Ordering::Relaxed);
    }

    fn enter_active(&self) -> ActiveSubscriberGuard {
        self.inner.active.fetch_add(1, Ordering::Relaxed);
        ActiveSubscriberGuard {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct ActiveSubscriberGuard {
    inner: Arc<Inner>,
}

impl Drop for ActiveSubscriberGuard {
    fn drop(&mut self) {
        self.inner.active.fetch_sub(1, Ordering::Relaxed);
    }
}

impl TelemetrySink for WebSocketSink {
    fn on_scheduler_tick_start(&self, ev: &SchedulerTickStart) {
        let _ = self.inner.tx.send(from_tick_start(ev, unix_ms()));
    }

    fn on_scheduler_tick_end(&self, ev: &SchedulerTickEnd) {
        let _ = self.inner.tx.send(from_tick_end(ev, unix_ms()));
    }

    fn on_queue_depths(&self, ev: &QueueDepths) {
        let _ = self.inner.tx.send(from_queue_depths(ev, unix_ms()));
    }

    fn on_node_evaluate_start(&self, ev: &NodeEvaluateStart<'_>) {
        let _ = self
            .inner
            .tx
            .send(from_node_evaluate_start(ev, unix_ms()));
    }

    fn on_node_evaluate_end(&self, ev: &NodeEvaluateEnd<'_>) {
        let _ = self.inner.tx.send(from_node_evaluate_end(ev, unix_ms()));
    }

    fn on_message_delivered(&self, ev: &MessageDelivered<'_>) {
        let _ = self.inner.tx.send(from_message_delivered(ev, unix_ms()));
    }

    fn on_node_mounted(&self, ev: &NodeMounted<'_>) {
        let _ = self.inner.tx.send(from_node_mounted(ev, unix_ms()));
    }

    fn on_node_unmounted(&self, ev: &NodeUnmounted<'_>) {
        let _ = self.inner.tx.send(from_node_unmounted(ev, unix_ms()));
    }

    fn on_topology_event(&self, ev: &TopologyEventOccurred<'_>) {
        let _ = self.inner.tx.send(from_topology_event(ev, unix_ms()));
    }

    fn on_supervisor_decision(&self, ev: &SupervisorDecisionMade<'_>) {
        let _ = self
            .inner
            .tx
            .send(from_supervisor_decision(ev, unix_ms()));
    }

    fn on_remote_message(&self, ev: &RemoteMessage<'_>) {
        let _ = self.inner.tx.send(from_remote_message(ev, unix_ms()));
    }

    fn on_hot_reload(&self, ev: &HotReload<'_>) {
        let _ = self.inner.tx.send(from_hot_reload(ev, unix_ms()));
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// --- Filter parsing ---

/// Decoded `?node=...&type=...&since=...` query parameters. Every
/// field is optional; missing → no filter on that axis.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventFilter {
    pub node: Option<String>,
    /// `?type=<kind>` — matches [`TelemetryEvent::kind`].
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub since: Option<u64>,
}

impl EventFilter {
    /// True when the event passes every active filter axis.
    #[must_use]
    pub fn accepts(&self, ev: &TelemetryEvent) -> bool {
        if let Some(want) = self.kind.as_deref() {
            if !ev.matches_kind(want) {
                return false;
            }
        }
        if let Some(want) = self.node.as_deref() {
            if !ev.matches_node(want) {
                return false;
            }
        }
        if let Some(since) = self.since {
            if ev.ts_ms() <= since {
                return false;
            }
        }
        true
    }
}

// --- Axum handlers ---

/// `GET /v1/observer/events` — WebSocket upgrade. Subscribes to the
/// daemon's [`WebSocketSink`] and forwards every matching event to
/// the client as a JSON text frame. On broadcast lag, sends a final
/// [`TelemetryEvent::Resync`] frame, increments the kicked counter,
/// and closes.
pub async fn ws_handler(
    State(state): State<ApiState>,
    Query(filter): Query<EventFilter>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let sink = match state.websocket.clone() {
        Some(sink) => sink,
        None => return ws_disabled_response(),
    };
    upgrade.on_upgrade(move |socket| handle_ws_connection(socket, sink, filter))
}

async fn handle_ws_connection(
    mut socket: WebSocket,
    sink: WebSocketSink,
    filter: EventFilter,
) {
    let _guard = sink.enter_active();
    let mut rx = sink.subscribe();
    loop {
        tokio::select! {
            // Client→server frame (Pong, Close, etc.). We discard
            // payload (no client→server commands in P7); a Close
            // frame ends the loop.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
            // Server→client telemetry frame.
            recv = rx.recv() => {
                match recv {
                    Ok(ev) => {
                        if !filter.accepts(&ev) {
                            continue;
                        }
                        let payload = match serde_json::to_string(&ev) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        if socket.send(Message::Text(payload)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        sink.record_kick();
                        let resync = TelemetryEvent::Resync {
                            ts_ms: unix_ms(),
                            missed,
                        };
                        if let Ok(payload) = serde_json::to_string(&resync) {
                            let _ = socket.send(Message::Text(payload)).await;
                        }
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

fn ws_disabled_response() -> Response {
    use axum::http::StatusCode;
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "websocket sink not installed",
    )
        .into_response()
}

/// `GET /v1/observer/events.sse` — Server-Sent Events fallback. Same
/// event stream + same filter set as the WebSocket handler; suitable
/// for clients that prefer EventSource (`curl --no-buffer`, browser
/// `EventSource`). On broadcast lag, sends a final `resync` event
/// and ends the stream.
pub async fn sse_handler(
    State(state): State<ApiState>,
    Query(filter): Query<EventFilter>,
) -> Response {
    let sink = match state.websocket.clone() {
        Some(sink) => sink,
        None => return ws_disabled_response(),
    };
    let stream = sse_stream(sink, filter);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn sse_stream(
    sink: WebSocketSink,
    filter: EventFilter,
) -> impl Stream<Item = Result<SseEvent, std::convert::Infallible>> {
    let rx = sink.subscribe();
    let active = sink.enter_active();
    let stream = futures::stream::unfold(
        (rx, sink.clone(), filter, Some(active), false),
        |(mut rx, sink, filter, active, ended)| async move {
            if ended {
                return None;
            }
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if !filter.accepts(&ev) {
                            continue;
                        }
                        let payload = match serde_json::to_string(&ev) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        let evt = SseEvent::default()
                            .event("telemetry")
                            .data(payload);
                        return Some((Ok(evt), (rx, sink, filter, active, false)));
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        sink.record_kick();
                        let resync = TelemetryEvent::Resync {
                            ts_ms: unix_ms(),
                            missed,
                        };
                        let payload = serde_json::to_string(&resync).unwrap_or_default();
                        let evt = SseEvent::default()
                            .event("resync")
                            .data(payload);
                        return Some((Ok(evt), (rx, sink, filter, active, true)));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );
    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use graphreact_core::TelemetrySink as _;

    use super::*;

    #[tokio::test]
    async fn websocket_sink_fans_message_delivered_to_subscribers() {
        let sink = WebSocketSink::new(64);
        let mut rx = sink.subscribe();
        sink.on_message_delivered(&MessageDelivered {
            from: "a",
            to: "b",
            message_type: "ping",
            ttl: 4,
            direct: false,
            accepted: true,
            trace_context: None,
        });
        let ev = rx.recv().await.expect("recv");
        match ev {
            TelemetryEvent::MessageDelivered { from, to, .. } => {
                assert_eq!(from, "a");
                assert_eq!(to, "b");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn slow_subscriber_sees_lagged_recv_error_after_capacity_overflow() {
        let sink = WebSocketSink::new(2);
        let mut rx = sink.subscribe();
        for _ in 0..8 {
            sink.on_scheduler_tick_start(&SchedulerTickStart {});
        }
        // The first recv pulls a TickStart; subsequent pulls
        // surface `Lagged` once the ring overflowed.
        let mut got_lagged = false;
        for _ in 0..8 {
            match rx.recv().await {
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    got_lagged = true;
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        assert!(got_lagged, "expected Lagged after capacity overflow");
    }

    #[test]
    fn event_filter_accepts_when_no_filters_set() {
        let filter = EventFilter::default();
        let ev = TelemetryEvent::TickStart { ts_ms: 1 };
        assert!(filter.accepts(&ev));
    }

    #[test]
    fn event_filter_rejects_wrong_kind() {
        let filter = EventFilter {
            kind: Some("message_delivered".into()),
            ..Default::default()
        };
        assert!(!filter.accepts(&TelemetryEvent::TickStart { ts_ms: 0 }));
    }

    #[test]
    fn event_filter_rejects_other_node() {
        let filter = EventFilter {
            node: Some("alice".into()),
            ..Default::default()
        };
        let ev = TelemetryEvent::MessageDelivered {
            ts_ms: 0,
            from: "bob".into(),
            to: "carol".into(),
            message_type: "ping".into(),
            ttl: 4,
            direct: false,
            accepted: true,
            trace_context: None,
        };
        assert!(!filter.accepts(&ev));
    }

    #[test]
    fn event_filter_since_is_strict_greater_than() {
        let filter = EventFilter {
            since: Some(100),
            ..Default::default()
        };
        let early = TelemetryEvent::TickStart { ts_ms: 100 };
        let late = TelemetryEvent::TickStart { ts_ms: 101 };
        assert!(!filter.accepts(&early));
        assert!(filter.accepts(&late));
    }

    #[test]
    fn announce_resync_emits_resync_variant() {
        let sink = WebSocketSink::new(8);
        let mut rx = sink.subscribe();
        sink.announce_resync(42);
        let ev = futures::executor::block_on(rx.recv()).unwrap();
        assert!(matches!(ev, TelemetryEvent::Resync { missed: 42, .. }));
    }
}
