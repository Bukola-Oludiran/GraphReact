//! **M16-Backend Phase 7.** Owned mirror of the
//! [`graphreact_core::TelemetrySink`] event surface — required because
//! the trait events use borrowed `&'a str` lifetimes that can't outlive
//! the scheduler's hot-path stack frame, but the WebSocket / SSE
//! broadcast fanout needs `Send + 'static` events that live across
//! tokio task boundaries.
//!
//! Every variant carries a `ts_ms` (Unix-millis) captured at sink time.
//! The discriminant `kind` is rendered as a stable string in the JSON
//! envelope; companion-ecosystem clients pin against these names.
//!
//! Filter helpers:
//!
//! - [`TelemetryEvent::matches_kind`] — matches the `kind` query
//!   parameter (`?type=<kind>`).
//! - [`TelemetryEvent::matches_node`] — matches the `node` query
//!   parameter against any node id field on the event (sender,
//!   receiver, mounted/unmounted node, supervisor's failed child).
//! - [`TelemetryEvent::ts_ms`] — for the `?since=<ts>` filter.

use serde::{Deserialize, Serialize};

use graphreact_core::telemetry::{
    HotReload, MessageDelivered, NodeEvaluateEnd, NodeEvaluateStart, NodeMounted, NodeUnmounted,
    QueueDepths, QueueDepthsSnapshot, RemoteDirection, RemoteMessage, SchedulerTickEnd,
    SchedulerTickStart, SupervisorAction, SupervisorDecisionMade, TopologyEventOccurred,
};

/// Stable owned mirror of every [`graphreact_core::TelemetrySink`]
/// event. `kind` is the wire-stable discriminant (snake_case) and
/// `ts_ms` is the sink-side timestamp.
///
/// `serde(tag = "kind")` flattens the discriminant into the same JSON
/// object as the payload fields — companion ecosystems decode by
/// matching on the `kind` string without needing a wrapping object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TelemetryEvent {
    TickStart {
        ts_ms: u64,
    },
    TickEnd {
        ts_ms: u64,
        iterations: u32,
        terminated_by_depth_cap: bool,
    },
    QueueDepths {
        ts_ms: u64,
        depth: u32,
        snapshot: QueueDepthsSnapshot,
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
        direct: bool,
        accepted: bool,
        /// **M16-Backend P10.** Remote OTel span context propagated
        /// from the wire packet (24-byte `TraceID(16) || SpanID(8)`).
        /// `None` for local-origin messages. Skipped from JSON when
        /// `None` to keep the WS/SSE wire shape stable for non-OTel
        /// consumers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_context: Option<[u8; 24]>,
    },
    NodeMounted {
        ts_ms: u64,
        node_id: String,
    },
    NodeUnmounted {
        ts_ms: u64,
        node_id: String,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason_detail: Option<String>,
    },
    TopologyEvent {
        ts_ms: u64,
        topology_kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        node_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        edge_u: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        edge_v: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        edge_key: Option<String>,
    },
    SupervisorDecision {
        ts_ms: u64,
        failed_id: String,
        reason: String,
        action: SupervisorAction,
    },
    RemoteMessage {
        ts_ms: u64,
        direction: RemoteDirection,
        peer: String,
        from: String,
        to: String,
        #[serde(rename = "type")]
        message_type: String,
        byte_size: usize,
    },
    HotReload {
        ts_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        source_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        origin_peer: Option<String>,
        nodes_replaced: usize,
        nodes_remounted: usize,
        edges_changed: usize,
        rolled_back: bool,
        error_count: usize,
    },
    /// Server→client divider sent when a slow subscriber is kicked.
    /// The client refetches state via REST and reconnects; the
    /// `missed` count reports how many events were dropped between
    /// the kick and the refetch.
    Resync {
        ts_ms: u64,
        missed: u64,
    },
}

impl TelemetryEvent {
    /// The wire-stable discriminant string (`"tick_start"`,
    /// `"message_delivered"`, etc.).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            TelemetryEvent::TickStart { .. } => "tick_start",
            TelemetryEvent::TickEnd { .. } => "tick_end",
            TelemetryEvent::QueueDepths { .. } => "queue_depths",
            TelemetryEvent::NodeEvaluateStart { .. } => "node_evaluate_start",
            TelemetryEvent::NodeEvaluateEnd { .. } => "node_evaluate_end",
            TelemetryEvent::MessageDelivered { .. } => "message_delivered",
            TelemetryEvent::NodeMounted { .. } => "node_mounted",
            TelemetryEvent::NodeUnmounted { .. } => "node_unmounted",
            TelemetryEvent::TopologyEvent { .. } => "topology_event",
            TelemetryEvent::SupervisorDecision { .. } => "supervisor_decision",
            TelemetryEvent::RemoteMessage { .. } => "remote_message",
            TelemetryEvent::HotReload { .. } => "hot_reload",
            TelemetryEvent::Resync { .. } => "resync",
        }
    }

    /// Sink-side timestamp in Unix-millis.
    #[must_use]
    pub fn ts_ms(&self) -> u64 {
        match self {
            TelemetryEvent::TickStart { ts_ms }
            | TelemetryEvent::TickEnd { ts_ms, .. }
            | TelemetryEvent::QueueDepths { ts_ms, .. }
            | TelemetryEvent::NodeEvaluateStart { ts_ms, .. }
            | TelemetryEvent::NodeEvaluateEnd { ts_ms, .. }
            | TelemetryEvent::MessageDelivered { ts_ms, .. }
            | TelemetryEvent::NodeMounted { ts_ms, .. }
            | TelemetryEvent::NodeUnmounted { ts_ms, .. }
            | TelemetryEvent::TopologyEvent { ts_ms, .. }
            | TelemetryEvent::SupervisorDecision { ts_ms, .. }
            | TelemetryEvent::RemoteMessage { ts_ms, .. }
            | TelemetryEvent::HotReload { ts_ms, .. }
            | TelemetryEvent::Resync { ts_ms, .. } => *ts_ms,
        }
    }

    /// Match the `?type=<kind>` filter — case-insensitive exact match
    /// against [`TelemetryEvent::kind`].
    #[must_use]
    pub fn matches_kind(&self, want: &str) -> bool {
        self.kind().eq_ignore_ascii_case(want)
    }

    /// Match the `?node=<id>` filter — true when any node-id field on
    /// this event equals `want` (sender, receiver, mounted /
    /// unmounted node, supervisor's failed child, evaluate target).
    #[must_use]
    pub fn matches_node(&self, want: &str) -> bool {
        match self {
            TelemetryEvent::NodeEvaluateStart { node_id, .. }
            | TelemetryEvent::NodeEvaluateEnd { node_id, .. }
            | TelemetryEvent::NodeMounted { node_id, .. }
            | TelemetryEvent::NodeUnmounted { node_id, .. } => node_id == want,
            TelemetryEvent::MessageDelivered { from, to, .. }
            | TelemetryEvent::RemoteMessage { from, to, .. } => from == want || to == want,
            TelemetryEvent::TopologyEvent { node_id, .. } => {
                node_id.as_deref() == Some(want)
            }
            TelemetryEvent::SupervisorDecision { failed_id, .. } => failed_id == want,
            TelemetryEvent::TickStart { .. }
            | TelemetryEvent::TickEnd { .. }
            | TelemetryEvent::QueueDepths { .. }
            | TelemetryEvent::HotReload { .. }
            | TelemetryEvent::Resync { .. } => false,
        }
    }
}

// --- Borrowed→owned conversions, called by `WebSocketSink` per event ---

#[must_use]
pub fn from_tick_start(_ev: &SchedulerTickStart, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::TickStart { ts_ms }
}

#[must_use]
pub fn from_tick_end(ev: &SchedulerTickEnd, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::TickEnd {
        ts_ms,
        iterations: ev.iterations,
        terminated_by_depth_cap: ev.terminated_by_depth_cap,
    }
}

#[must_use]
pub fn from_queue_depths(ev: &QueueDepths, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::QueueDepths {
        ts_ms,
        depth: ev.depth,
        snapshot: ev.snapshot,
    }
}

#[must_use]
pub fn from_node_evaluate_start(ev: &NodeEvaluateStart<'_>, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::NodeEvaluateStart {
        ts_ms,
        node_id: ev.node_id.to_string(),
    }
}

#[must_use]
pub fn from_node_evaluate_end(ev: &NodeEvaluateEnd<'_>, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::NodeEvaluateEnd {
        ts_ms,
        node_id: ev.node_id.to_string(),
    }
}

#[must_use]
pub fn from_message_delivered(ev: &MessageDelivered<'_>, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::MessageDelivered {
        ts_ms,
        from: ev.from.to_string(),
        to: ev.to.to_string(),
        message_type: ev.message_type.to_string(),
        ttl: ev.ttl,
        direct: ev.direct,
        accepted: ev.accepted,
        // **M16-Backend P10.** Forward the optional remote OTel
        // span context through to the owned event for WS/SSE
        // consumers + collector-side cross-node link rendering.
        trace_context: ev.trace_context,
    }
}

#[must_use]
pub fn from_node_mounted(ev: &NodeMounted<'_>, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::NodeMounted {
        ts_ms,
        node_id: ev.node_id.to_string(),
    }
}

#[must_use]
pub fn from_node_unmounted(ev: &NodeUnmounted<'_>, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::NodeUnmounted {
        ts_ms,
        node_id: ev.node_id.to_string(),
        reason: ev.reason.to_string(),
        reason_detail: ev.reason_detail.map(str::to_string),
    }
}

#[must_use]
pub fn from_topology_event(ev: &TopologyEventOccurred<'_>, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::TopologyEvent {
        ts_ms,
        topology_kind: ev.kind.to_string(),
        node_id: ev.node_id.map(str::to_string),
        edge_u: ev.edge_u.map(str::to_string),
        edge_v: ev.edge_v.map(str::to_string),
        edge_key: ev.edge_key.map(str::to_string),
    }
}

#[must_use]
pub fn from_supervisor_decision(
    ev: &SupervisorDecisionMade<'_>,
    ts_ms: u64,
) -> TelemetryEvent {
    TelemetryEvent::SupervisorDecision {
        ts_ms,
        failed_id: ev.failed_id.to_string(),
        reason: ev.reason.to_string(),
        action: ev.action,
    }
}

#[must_use]
pub fn from_remote_message(ev: &RemoteMessage<'_>, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::RemoteMessage {
        ts_ms,
        direction: ev.direction,
        peer: ev.peer.to_string(),
        from: ev.from.to_string(),
        to: ev.to.to_string(),
        message_type: ev.message_type.to_string(),
        byte_size: ev.byte_size,
    }
}

#[must_use]
pub fn from_hot_reload(ev: &HotReload<'_>, ts_ms: u64) -> TelemetryEvent {
    TelemetryEvent::HotReload {
        ts_ms,
        source_path: ev.source_path.map(str::to_string),
        origin_peer: ev.origin_peer.map(str::to_string),
        nodes_replaced: ev.nodes_replaced,
        nodes_remounted: ev.nodes_remounted,
        edges_changed: ev.edges_changed,
        rolled_back: ev.rolled_back,
        error_count: ev.error_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_delivered_serializes_with_kind_tag() {
        let ev = TelemetryEvent::MessageDelivered {
            ts_ms: 1_000,
            from: "a".into(),
            to: "b".into(),
            message_type: "ping".into(),
            ttl: 4,
            direct: false,
            accepted: true,
            trace_context: None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""kind":"message_delivered""#), "{s}");
        assert!(s.contains(r#""type":"ping""#), "{s}");
        assert!(s.contains(r#""accepted":true"#), "{s}");
    }

    #[test]
    fn topology_event_serializes_topology_kind() {
        let ev = TelemetryEvent::TopologyEvent {
            ts_ms: 0,
            topology_kind: "node_added".into(),
            node_id: Some("a".into()),
            edge_u: None,
            edge_v: None,
            edge_key: None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""kind":"topology_event""#));
        assert!(s.contains(r#""topology_kind":"node_added""#));
    }

    #[test]
    fn matches_kind_is_case_insensitive() {
        let ev = TelemetryEvent::TickStart { ts_ms: 0 };
        assert!(ev.matches_kind("tick_start"));
        assert!(ev.matches_kind("Tick_Start"));
        assert!(!ev.matches_kind("tick_end"));
    }

    #[test]
    fn matches_node_checks_from_and_to() {
        let ev = TelemetryEvent::MessageDelivered {
            ts_ms: 0,
            from: "alice".into(),
            to: "bob".into(),
            message_type: "ping".into(),
            ttl: 1,
            direct: false,
            accepted: true,
            trace_context: None,
        };
        assert!(ev.matches_node("alice"));
        assert!(ev.matches_node("bob"));
        assert!(!ev.matches_node("charlie"));
    }

    #[test]
    fn matches_node_checks_supervisor_failed_id() {
        let ev = TelemetryEvent::SupervisorDecision {
            ts_ms: 0,
            failed_id: "worker-3".into(),
            reason: "crashed".into(),
            action: SupervisorAction::Proceeded,
        };
        assert!(ev.matches_node("worker-3"));
        assert!(!ev.matches_node("worker-2"));
    }

    #[test]
    fn ts_ms_returns_value() {
        let ev = TelemetryEvent::Resync {
            ts_ms: 12_345,
            missed: 7,
        };
        assert_eq!(ev.ts_ms(), 12_345);
    }

    #[test]
    fn round_trips_through_json() {
        let ev = TelemetryEvent::NodeUnmounted {
            ts_ms: 99,
            node_id: "n".into(),
            reason: "crashed".into(),
            reason_detail: Some("panic".into()),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: TelemetryEvent = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, TelemetryEvent::NodeUnmounted { .. }));
        assert_eq!(back.ts_ms(), 99);
    }
}
