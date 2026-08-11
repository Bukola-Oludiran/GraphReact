//! **M16-Backend Phase 7.** WebSocket client for the `grx observer`
//! TUI's live-event subscription path. Pairs with the daemon's
//! [`graphreact_observer::WebSocketSink`] / [`/v1/observer/events`]
//! handler.
//!
//! The server pushes `TelemetryEvent` JSON frames; the client decodes
//! them and either feeds them into the shared
//! [`super::state::ObserverState`] (e.g. [`TelemetryEvent::MessageDelivered`]
//! flows into `recent_messages` + the trace tape) OR ignores them
//! (events that don't fit the TUI's display model — `tick_*`,
//! `queue_depths`, etc., are buffered for future use).
//!
//! On a [`TelemetryEvent::Resync`] frame from the server, the WS
//! task closes; the polling task already in flight provides the
//! re-baseline so no extra REST refetch is needed in P6/P7. The WS
//! task then re-enters via the reconnect schedule (same `Backoff`
//! struct as `polling.rs`).

use std::sync::Arc;
use std::time::SystemTime;

use futures::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::observer_client::{Target, TlsMode};

use super::reconnect::Backoff;
use super::state::{MessageRow, ObserverState, TraceEntry};

/// Owned mirror of `graphreact_observer::TelemetryEvent`. Kept in
/// the CLI crate so the type is decode-stable against the daemon
/// schema across `/v1/`-versioned wire-format changes.
///
/// The CLI only actively consumes `MessageDelivered` events for the
/// trace tape + recent-messages map; other variants decode but
/// don't trigger state updates today. P7 ships the substrate; future
/// phases can wire more event kinds into the TUI.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WsTelemetryEvent {
    TickStart {
        ts_ms: u64,
    },
    TickEnd {
        ts_ms: u64,
    },
    QueueDepths {
        ts_ms: u64,
    },
    NodeEvaluateStart {
        ts_ms: u64,
        node_id: String,
    },
    NodeEvaluateEnd {
        ts_ms: u64,
        node_id: String,
    },
    MessageDelivered {
        ts_ms: u64,
        from: String,
        to: String,
        #[serde(rename = "type")]
        message_type: String,
        ttl: u32,
        #[serde(default)]
        direct: bool,
        accepted: bool,
    },
    NodeMounted {
        ts_ms: u64,
        node_id: String,
    },
    NodeUnmounted {
        ts_ms: u64,
        node_id: String,
        reason: String,
    },
    TopologyEvent {
        ts_ms: u64,
    },
    SupervisorDecision {
        ts_ms: u64,
        failed_id: String,
        reason: String,
    },
    RemoteMessage {
        ts_ms: u64,
        from: String,
        to: String,
        #[serde(rename = "type")]
        message_type: String,
    },
    HotReload {
        ts_ms: u64,
    },
    Resync {
        ts_ms: u64,
        missed: u64,
    },
    /// Catch-all so future event variants don't break the client.
    /// `serde` deserializes any unknown `kind` here; the dispatch
    /// loop just ignores it.
    #[serde(other)]
    Unknown,
}

impl WsTelemetryEvent {
    /// Sink-side timestamp captured by the daemon when it owned the
    /// event. Used by tests + future `?since=` re-subscription work.
    #[must_use]
    pub fn ts_ms(&self) -> u64 {
        match self {
            Self::TickStart { ts_ms }
            | Self::TickEnd { ts_ms }
            | Self::QueueDepths { ts_ms }
            | Self::NodeEvaluateStart { ts_ms, .. }
            | Self::NodeEvaluateEnd { ts_ms, .. }
            | Self::MessageDelivered { ts_ms, .. }
            | Self::NodeMounted { ts_ms, .. }
            | Self::NodeUnmounted { ts_ms, .. }
            | Self::TopologyEvent { ts_ms }
            | Self::SupervisorDecision { ts_ms, .. }
            | Self::RemoteMessage { ts_ms, .. }
            | Self::HotReload { ts_ms }
            | Self::Resync { ts_ms, .. } => *ts_ms,
            Self::Unknown => 0,
        }
    }
}

#[derive(Debug, Error)]
pub enum WsClientError {
    #[error("ws: {0}")]
    Tungstenite(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("server announced resync (missed={0})")]
    ResyncRequested(u64),
    #[error("server closed connection")]
    Closed,
}

/// Inputs the WS task carries — fixed for the duration of a session.
pub struct WsCtx {
    pub target: Target,
    pub token: Option<String>,
    pub tls: TlsMode,
}

/// Run the WS task. Loops over connect → consume → reconnect with
/// exponential backoff. Updates `state_tx` directly via
/// `send_modify` for every relevant event. Exits when `shutdown_rx`
/// flips to `true`.
pub async fn run_ws_task(
    ctx: Arc<WsCtx>,
    state_tx: watch::Sender<ObserverState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = Backoff::new();
    loop {
        if *shutdown.borrow() {
            return;
        }
        match connect_and_stream(&ctx, &state_tx, &mut shutdown).await {
            Ok(()) => {
                // Server closed cleanly — try again immediately.
                backoff.reset();
                continue;
            }
            Err(WsClientError::ResyncRequested(missed)) => {
                state_tx.send_modify(|s| {
                    s.push_trace(TraceEntry::TargetRestarted { ts_ms: unix_ms() });
                    s.last_error = Some(format!("ws resync (missed={missed})"));
                });
                backoff.reset();
                continue;
            }
            Err(err) => {
                state_tx.send_modify(|s| {
                    s.last_error = Some(format!("ws: {err}"));
                });
                let delay = backoff.current();
                backoff.bump();
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    _ = shutdown.changed() => return,
                }
            }
        }
    }
}

async fn connect_and_stream(
    ctx: &WsCtx,
    state_tx: &watch::Sender<ObserverState>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), WsClientError> {
    let url = build_ws_url(&ctx.target);
    let mut request = url
        .into_client_request()
        .map_err(|e| WsClientError::InvalidUrl(format!("{e}")))?;
    if let Some(t) = ctx.token.as_deref() {
        let val = HeaderValue::from_str(&format!("Bearer {t}"))
            .map_err(|e| WsClientError::InvalidUrl(format!("token header: {e}")))?;
        request.headers_mut().insert("Authorization", val);
    }

    // tokio-tungstenite's `connect_async` picks the TLS connector
    // automatically when the URL scheme is `wss://` — the
    // `rustls-tls-native-roots` feature flag in `Cargo.toml` selects
    // rustls + native roots. The CLI's `--insecure-no-tls` flag is
    // honoured ONLY on the polling path today; WSS connect uses
    // strict cert verification. Documenting as a P7 footgun for
    // P8+ work.
    let _ = ctx.tls;
    let (mut socket, _resp) = tokio_tungstenite::connect_async(request).await?;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                let _ = socket.send(Message::Close(None)).await;
                return Ok(());
            }
            msg = socket.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let ev: WsTelemetryEvent = serde_json::from_str(&text)?;
                        if let WsTelemetryEvent::Resync { missed, .. } = ev {
                            return Err(WsClientError::ResyncRequested(missed));
                        }
                        apply_event(state_tx, ev);
                    }
                    Some(Ok(Message::Close(_))) | None => return Err(WsClientError::Closed),
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(WsClientError::from(err)),
                }
            }
        }
    }
}

fn build_ws_url(target: &Target) -> String {
    let scheme = if target.tls { "wss" } else { "ws" };
    format!(
        "{scheme}://{}:{}/v1/observer/events",
        target.host, target.port
    )
}

fn apply_event(state_tx: &watch::Sender<ObserverState>, ev: WsTelemetryEvent) {
    if let WsTelemetryEvent::MessageDelivered {
        ts_ms,
        from,
        to,
        message_type,
        ttl,
        accepted,
        ..
    } = ev
    {
        let row = MessageRow {
            ts_ms,
            from: from.clone(),
            to: to.clone(),
            message_type,
            ttl,
            accepted,
        };
        state_tx.send_modify(|s| {
            s.push_trace(TraceEntry::Message(row.clone()));
            s.recent_messages.entry(to.clone()).or_default().push(row);
        });
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_picks_ws_for_plain_target() {
        let t = Target::parse("127.0.0.1:9090").unwrap();
        assert_eq!(build_ws_url(&t), "ws://127.0.0.1:9090/v1/observer/events");
    }

    #[test]
    fn ws_url_picks_wss_for_tls_target() {
        let t = Target::parse("https://example.com:443").unwrap();
        assert_eq!(build_ws_url(&t), "wss://example.com:443/v1/observer/events");
    }

    #[test]
    fn message_delivered_round_trips() {
        let s = r#"{"kind":"message_delivered","ts_ms":1234,"from":"a","to":"b","type":"ping","ttl":4,"direct":false,"accepted":true}"#;
        let ev: WsTelemetryEvent = serde_json::from_str(s).unwrap();
        match ev {
            WsTelemetryEvent::MessageDelivered {
                from,
                to,
                ttl,
                accepted,
                ..
            } => {
                assert_eq!(from, "a");
                assert_eq!(to, "b");
                assert_eq!(ttl, 4);
                assert!(accepted);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn unknown_event_kind_decodes_as_unknown() {
        let s = r#"{"kind":"future_event_kind","ts_ms":99}"#;
        let ev: WsTelemetryEvent = serde_json::from_str(s).unwrap();
        assert!(matches!(ev, WsTelemetryEvent::Unknown));
    }

    #[test]
    fn resync_decodes_with_missed_count() {
        let s = r#"{"kind":"resync","ts_ms":42,"missed":7}"#;
        let ev: WsTelemetryEvent = serde_json::from_str(s).unwrap();
        match ev {
            WsTelemetryEvent::Resync { missed, .. } => assert_eq!(missed, 7),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn ts_ms_returns_inner_value() {
        let ev = WsTelemetryEvent::TickStart { ts_ms: 1234 };
        assert_eq!(ev.ts_ms(), 1234);
    }
}
