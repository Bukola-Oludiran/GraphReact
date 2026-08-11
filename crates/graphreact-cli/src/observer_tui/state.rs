//! Typed snapshot of the latest poll round + pane / connection state.
//!
//! Mutated only by the polling task (via [`ObserverState::apply_poll`])
//! and the input task (via [`ObserverState::handle_pane_event`]); read
//! by the render task on every frame. Lives behind a
//! `tokio::sync::watch` channel so the watch's RCU semantics buys us
//! read-only frames without locking.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Echo of the daemon's `Identity` payload — keeps the CLI free of a
/// graphreact-observer dependency at the type level. `version` is owned
/// here (in core's struct it's `&'static str`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Identity {
    pub process_start_id: u64,
    pub version: String,
    pub api_version: u32,
}

/// Echo of `graphreact_core::telemetry::NodeIntrospection`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NodeRow {
    pub id: String,
    pub role: String,
    pub depth: u32,
    #[serde(default)]
    pub supervisor: Option<String>,
}

/// Echo of `graphreact_core::telemetry::SupervisorIntrospection`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SupervisorRow {
    pub key: String,
    pub strategy: String,
    pub max_restarts: u32,
    pub period_secs: u64,
    pub restart_count: usize,
    pub children: Vec<String>,
}

/// Echo of `graphreact_observer::MessageSnapshot` minus the payload
/// (which the daemon never sends — PII safe-by-default). Field
/// `accepted` flags rejection at the receiver's permission manifest.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct MessageRow {
    pub ts_ms: u64,
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub message_type: String,
    pub ttl: u32,
    pub accepted: bool,
}

/// Connection status of the last poll round. The render layer turns
/// this into the top status-bar string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// Initial state before the first poll completes.
    Connecting,
    /// Last poll round succeeded.
    Connected,
    /// Last poll round failed; the reconnect loop is in backoff.
    Disconnected { reason: String, retry_in_ms: u64 },
}

/// One of the pane state-machine values. Tab cycles forward through
/// Overview → Tree → Nodes → Trace; `Enter` from Nodes drills into
/// `NodeDetail`; `Esc` from `NodeDetail` returns to Nodes.
///
/// Nodes pane carries a selection cursor (`nodes_cursor`); Trace
/// pane carries a node id the user picked + a tail of the latest
/// ring snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pane {
    Overview,
    Tree,
    Nodes,
    NodeDetail { node_id: String },
    Trace { node_id: Option<String> },
}

impl Pane {
    pub fn label(&self) -> &'static str {
        match self {
            Pane::Overview => "Overview",
            Pane::Tree => "Tree",
            Pane::Nodes => "Nodes",
            Pane::NodeDetail { .. } => "NodeDetail",
            Pane::Trace { .. } => "Trace",
        }
    }

    /// Hint string rendered in the bottom hint bar for the active
    /// pane. Pass-1 keybinds; pass-2 layout polish covers per-pane
    /// search / filtering hint variations.
    pub fn keybind_hint(&self) -> &'static str {
        match self {
            Pane::Overview => "Tab: switch pane | q: quit",
            Pane::Tree => "Tab: switch pane | j/k: scroll | q: quit",
            Pane::Nodes => "Tab: switch pane | j/k: select | Enter: detail | q: quit",
            Pane::NodeDetail { .. } => "Esc: back | Tab: switch pane | t: tail | q: quit",
            Pane::Trace { .. } => "Tab: switch pane | Esc: stop tailing | q: quit",
        }
    }

    /// Forward cycle for Tab. Returning the same variant keeps the
    /// inner state (selection / node id) so the user doesn't lose
    /// position when toggling.
    #[must_use]
    pub fn next(&self) -> Self {
        match self {
            Pane::Overview => Pane::Tree,
            Pane::Tree => Pane::Nodes,
            Pane::Nodes => Pane::Trace { node_id: None },
            Pane::NodeDetail { .. } | Pane::Trace { .. } => Pane::Overview,
        }
    }
}

/// Dividers + entries in the trace pane's running tape. Bounded
/// (`TRACE_LOG_CAP`) so the per-frame allocation footprint stays
/// flat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEntry {
    /// Operator-visible divider when `process_start_id` changes
    /// mid-session — the daemon was restarted under our feet.
    TargetRestarted {
        ts_ms: u64,
    },
    Message(MessageRow),
}

pub const TRACE_LOG_CAP: usize = 256;

/// The full poll-round-driven snapshot the render task reads on
/// every frame. Mutations land via [`ObserverState::apply_poll`] +
/// [`ObserverState::record_disconnect`] +
/// [`ObserverState::switch_pane`] etc.
#[derive(Debug, Clone)]
pub struct ObserverState {
    pub target_label: String,
    pub identity: Option<Identity>,
    pub nodes: Vec<NodeRow>,
    pub supervisors: Vec<SupervisorRow>,
    pub recent_messages: HashMap<String, Vec<MessageRow>>,
    pub trace_log: VecDeque<TraceEntry>,
    pub connection: Connection,
    pub pane: Pane,
    pub nodes_cursor: usize,
    pub last_poll_at: Option<Instant>,
    pub last_error: Option<String>,
    pub poll_round: u64,
}

impl ObserverState {
    pub fn new(target_label: String) -> Self {
        Self {
            target_label,
            identity: None,
            nodes: Vec::new(),
            supervisors: Vec::new(),
            recent_messages: HashMap::new(),
            trace_log: VecDeque::with_capacity(TRACE_LOG_CAP),
            connection: Connection::Connecting,
            pane: Pane::Overview,
            nodes_cursor: 0,
            last_poll_at: None,
            last_error: None,
            poll_round: 0,
        }
    }

    /// Apply a successful poll round. If `process_start_id` changed
    /// from the previous identity, push a divider into the trace
    /// log and clear the per-node ring snapshots. The acceptance
    /// gate's "auto-reconnect on target-process crash" is satisfied
    /// by this combined-with the reconnect loop in `reconnect.rs`.
    pub fn apply_poll(
        &mut self,
        identity: Identity,
        nodes: Vec<NodeRow>,
        supervisors: Vec<SupervisorRow>,
        per_node_messages: HashMap<String, Vec<MessageRow>>,
        now_ms: u64,
    ) {
        let restarted = match &self.identity {
            Some(prev) => prev.process_start_id != identity.process_start_id,
            None => false,
        };
        if restarted {
            self.recent_messages.clear();
            self.push_trace(TraceEntry::TargetRestarted { ts_ms: now_ms });
        }
        self.identity = Some(identity);
        self.nodes = nodes;
        self.supervisors = supervisors;
        // Merge per-node ring snapshots — only the actively-watched
        // node sends a fetch in P6, so old entries persist until
        // the user switches focus or the daemon restarts.
        for (id, ring) in per_node_messages {
            // Append novel messages to the rolling trace log so
            // the Trace pane shows a chronological tail.
            if let Some(prev) = self.recent_messages.get(&id) {
                let last_ts = prev.last().map(|m| m.ts_ms).unwrap_or(0);
                for m in &ring {
                    if m.ts_ms > last_ts {
                        self.push_trace(TraceEntry::Message(m.clone()));
                    }
                }
            } else {
                for m in &ring {
                    self.push_trace(TraceEntry::Message(m.clone()));
                }
            }
            self.recent_messages.insert(id, ring);
        }
        if self.nodes_cursor >= self.nodes.len() {
            self.nodes_cursor = self.nodes.len().saturating_sub(1);
        }
        self.connection = Connection::Connected;
        self.last_poll_at = Some(Instant::now());
        self.last_error = None;
        self.poll_round = self.poll_round.saturating_add(1);
    }

    /// Record a failed poll round + the reconnect loop's next retry
    /// budget. Identity / nodes / supervisors are NOT cleared — the
    /// last-known state stays visible while the reconnect happens.
    pub fn record_disconnect(&mut self, reason: String, retry_in_ms: u64) {
        self.last_error = Some(reason.clone());
        self.connection = Connection::Disconnected {
            reason,
            retry_in_ms,
        };
    }

    /// Operator-facing label for the focused trace node, or `None`
    /// when no node is selected yet.
    pub fn focused_trace_node(&self) -> Option<&str> {
        match &self.pane {
            Pane::Trace { node_id: Some(id) } => Some(id.as_str()),
            Pane::NodeDetail { node_id } => Some(node_id.as_str()),
            _ => None,
        }
    }

    /// Cycle to the next pane (Tab key). The render task reads
    /// `state.pane` on the next frame.
    pub fn switch_pane(&mut self) {
        self.pane = self.pane.next();
    }

    /// Move the Nodes-pane selection cursor by `delta` (clamped).
    /// Avoids signed/unsigned casts so the workspace's
    /// `cast_possible_wrap` + `cast_sign_loss` clippy lints stay
    /// satisfied.
    pub fn move_nodes_cursor(&mut self, delta: isize) {
        let len = self.nodes.len();
        if len == 0 {
            self.nodes_cursor = 0;
            return;
        }
        let last = len - 1;
        if delta >= 0 {
            let step = usize::try_from(delta).unwrap_or(usize::MAX);
            self.nodes_cursor = self.nodes_cursor.saturating_add(step).min(last);
        } else {
            let step = usize::try_from(-delta).unwrap_or(usize::MAX);
            self.nodes_cursor = self.nodes_cursor.saturating_sub(step);
        }
    }

    /// Drill from Nodes into NodeDetail using the current cursor.
    /// No-op when the cursor doesn't point at a real node.
    pub fn drill_into_selected(&mut self) {
        if let Some(node) = self.nodes.get(self.nodes_cursor) {
            self.pane = Pane::NodeDetail {
                node_id: node.id.clone(),
            };
        }
    }

    /// Pop the NodeDetail pane back to the Nodes list.
    pub fn pop_to_nodes(&mut self) {
        self.pane = Pane::Nodes;
    }

    /// Start tailing the currently-focused node in the Trace pane.
    /// Called from `t` in NodeDetail.
    pub fn start_trace_for_focused(&mut self) {
        if let Pane::NodeDetail { node_id } = &self.pane {
            self.pane = Pane::Trace {
                node_id: Some(node_id.clone()),
            };
        }
    }

    /// Drop the trace target (Esc from Trace).
    pub fn stop_trace(&mut self) {
        if matches!(self.pane, Pane::Trace { .. }) {
            self.pane = Pane::Trace { node_id: None };
        }
    }

    /// Push a trace-tape entry, evicting from the front when the
    /// ring is full.
    pub fn push_trace(&mut self, entry: TraceEntry) {
        if self.trace_log.len() == TRACE_LOG_CAP {
            self.trace_log.pop_front();
        }
        self.trace_log.push_back(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(start: u64) -> Identity {
        Identity {
            process_start_id: start,
            version: "0.1.0".into(),
            api_version: 1,
        }
    }

    fn node(id: &str, role: &str) -> NodeRow {
        NodeRow {
            id: id.into(),
            role: role.into(),
            depth: 0,
            supervisor: None,
        }
    }

    #[test]
    fn first_poll_seeds_identity_no_divider() {
        let mut s = ObserverState::new("127.0.0.1:9090".into());
        s.apply_poll(id(1), vec![node("a", "x")], vec![], HashMap::new(), 1000);
        assert_eq!(s.connection, Connection::Connected);
        assert!(s.identity.is_some());
        assert!(s.trace_log.is_empty());
    }

    #[test]
    fn process_start_id_change_pushes_divider_and_clears_rings() {
        let mut s = ObserverState::new("127.0.0.1:9090".into());
        s.apply_poll(id(1), vec![node("a", "x")], vec![], HashMap::new(), 1000);
        let mut ring = HashMap::new();
        ring.insert(
            "a".to_string(),
            vec![MessageRow {
                ts_ms: 1500,
                from: "x".into(),
                to: "a".into(),
                message_type: "ping".into(),
                ttl: 4,
                accepted: true,
            }],
        );
        s.apply_poll(id(1), vec![node("a", "x")], vec![], ring, 1500);
        assert_eq!(s.recent_messages.get("a").map(Vec::len), Some(1));

        // Daemon restart — process_start_id changes.
        s.apply_poll(id(2), vec![node("a", "x")], vec![], HashMap::new(), 2000);
        assert!(s.recent_messages.is_empty());
        assert!(matches!(
            s.trace_log.back(),
            Some(TraceEntry::TargetRestarted { .. })
        ));
    }

    #[test]
    fn pane_cycle_is_overview_tree_nodes_trace() {
        let mut s = ObserverState::new("x".into());
        assert_eq!(s.pane, Pane::Overview);
        s.switch_pane();
        assert_eq!(s.pane, Pane::Tree);
        s.switch_pane();
        assert_eq!(s.pane, Pane::Nodes);
        s.switch_pane();
        assert!(matches!(s.pane, Pane::Trace { node_id: None }));
        s.switch_pane();
        assert_eq!(s.pane, Pane::Overview);
    }

    #[test]
    fn drill_into_node_detail_then_pop() {
        let mut s = ObserverState::new("x".into());
        s.apply_poll(
            id(1),
            vec![node("a", "x"), node("b", "y")],
            vec![],
            HashMap::new(),
            1000,
        );
        s.pane = Pane::Nodes;
        s.move_nodes_cursor(1);
        s.drill_into_selected();
        assert_eq!(
            s.pane,
            Pane::NodeDetail {
                node_id: "b".into()
            }
        );
        s.pop_to_nodes();
        assert_eq!(s.pane, Pane::Nodes);
    }

    #[test]
    fn nodes_cursor_clamps_after_node_set_shrinks() {
        let mut s = ObserverState::new("x".into());
        s.apply_poll(
            id(1),
            vec![node("a", "x"), node("b", "y"), node("c", "z")],
            vec![],
            HashMap::new(),
            1000,
        );
        s.nodes_cursor = 2;
        s.apply_poll(id(1), vec![node("a", "x")], vec![], HashMap::new(), 1100);
        assert_eq!(s.nodes_cursor, 0);
    }

    #[test]
    fn trace_log_evicts_at_cap() {
        let mut s = ObserverState::new("x".into());
        for i in 0..(TRACE_LOG_CAP + 5) {
            s.push_trace(TraceEntry::TargetRestarted { ts_ms: i as u64 });
        }
        assert_eq!(s.trace_log.len(), TRACE_LOG_CAP);
    }

    #[test]
    fn record_disconnect_keeps_last_known_nodes() {
        let mut s = ObserverState::new("x".into());
        s.apply_poll(id(1), vec![node("a", "x")], vec![], HashMap::new(), 1000);
        s.record_disconnect("connection refused".into(), 1_500);
        assert!(matches!(s.connection, Connection::Disconnected { .. }));
        assert_eq!(s.nodes.len(), 1);
        assert!(s.identity.is_some());
    }
}
