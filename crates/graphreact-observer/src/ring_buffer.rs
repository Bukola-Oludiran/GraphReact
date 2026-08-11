//! **M16-Backend Phase 5.** Per-node bounded ring buffer of recent
//! messages, fed by `MessageDelivered` events through a
//! [`crate::TelemetrySink`] consumer.
//!
//! Powers `GET /v1/nodes/:id/messages/recent?limit=N` and the
//! `grx trace <node-id>` line-mode follower.
//!
//! ## Capacity bounds
//!
//! Per-node capacity defaults to 64 messages. Old messages are
//! evicted in FIFO order on overflow. With N nodes the worst-case
//! memory is `N * 64 * sizeof(MessageSnapshot)` ≈ tens of KB even
//! for a 5000-body cluster — fits comfortably in the daemon's
//! resident set.
//!
//! ## Privacy (PII)
//!
//! [`MessageSnapshot`] **deliberately omits the message payload.**
//! The plan calls for "PII-opt-in via existing manifest annotation";
//! today's [`graphreact_core::PermissionManifest`] has no PII flag,
//! so P5 ships the safe-by-default version that captures only
//! routing metadata (from / to / type / ttl / direct / accepted /
//! timestamp). Operators wanting payload visibility look at the P3
//! JSON-log stream (`tracing::*` events) which is consciously
//! payload-aware. Adding payload capture is post-M16 work behind a
//! manifest flag — see the `payload_opt_in` followon documented in
//! the per-phase memory.
//!
//! ## Hot-path discipline
//!
//! `on_message_delivered` fires on every accepted scheduler
//! delivery. Per-message work:
//!
//! 1. `DashMap::entry(node_id)` — sharded, ~25-40ns warm.
//! 2. `RwLock<VecDeque>::write()` — uncontended fast path is one
//!    atomic CAS + spin-free fall-through; bench-validated.
//! 3. `VecDeque::push_back` (+ `pop_front` if at capacity).
//!
//! Estimated: ~120-180ns per call. Bench gate (b)/(a) re-runs at
//! the P5 closeout to confirm.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use graphreact_core::{
    HotReload, MessageDelivered, NodeEvaluateEnd, NodeEvaluateStart, NodeMounted, NodeUnmounted,
    QueueDepths, RemoteMessage, SchedulerTickEnd, SchedulerTickStart, SupervisorDecisionMade,
    TelemetrySink, TopologyEventOccurred,
};

/// Default per-node ring capacity. Plan-locked at 64.
pub const DEFAULT_RING_CAPACITY: usize = 64;

/// One row of the per-node recent-messages ring.
///
/// Payload deliberately omitted; see module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSnapshot {
    /// Unix-millis timestamp at which the event was captured.
    /// Captured at sink time, not at scheduler-delivery time —
    /// they're effectively identical (the sink runs synchronously
    /// in the scheduler's `deliver` call) but the sink's `now()`
    /// is the one we expose because it's the only timestamp
    /// available at this layer.
    pub ts_ms: u64,
    /// Sender node ID.
    pub from: String,
    /// Receiver node ID — same as the ring buffer's key.
    pub to: String,
    /// Stable channel-type string (`"hop"`, `"ping"`, etc.).
    #[serde(rename = "type")]
    pub message_type: String,
    /// TTL at delivery (0 = TTL exhausted on this hop).
    pub ttl: u32,
    /// Direct emission (Phase 9 / `useEmit`) flag.
    pub direct: bool,
    /// `true` if the channel-policy / accept-list path accepted
    /// the message (handler ran). `false` = post-policy reject.
    pub accepted: bool,
}

/// `Send + Sync` ring buffer holding the last N
/// [`MessageSnapshot`]s per node. Queried by the management API's
/// `GET /v1/nodes/:id/messages/recent?limit=N` handler directly
/// (no bridge — the ring is `Send + Sync` so the handler thread
/// can read it without touching the `!Send` scheduler).
pub struct RingBufferTelemetry {
    inner: Arc<DashMap<String, RwLock<VecDeque<MessageSnapshot>>>>,
    capacity: usize,
}

impl Default for RingBufferTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl RingBufferTelemetry {
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_RING_CAPACITY)
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
            capacity,
        }
    }

    /// Wrap into `Arc<dyn TelemetrySink>` for direct
    /// `Scheduler::install_telemetry_sink` use.
    #[must_use]
    pub fn into_arc(self) -> Arc<dyn TelemetrySink> {
        Arc::new(self) as Arc<dyn TelemetrySink>
    }

    /// Read the last `limit` messages for a node, newest LAST
    /// (chronological order — what `tail -f` would show). Returns
    /// an empty `Vec` if the node has never received a message
    /// (or doesn't exist).
    ///
    /// `limit` of 0 returns empty; `limit` ≥ ring capacity
    /// returns up to capacity. The actual buffer is read under
    /// the per-node `RwLock` read guard; concurrent writers see
    /// no contention until they're ready to insert.
    #[must_use]
    pub fn recent(&self, node_id: &str, limit: usize) -> Vec<MessageSnapshot> {
        if limit == 0 {
            return Vec::new();
        }
        let Some(deque_lock) = self.inner.get(node_id) else {
            return Vec::new();
        };
        let deque = deque_lock.read();
        let take = limit.min(deque.len());
        // VecDeque iter() yields oldest → newest; tail-N is the
        // last `take` items in that order.
        deque.iter().skip(deque.len() - take).cloned().collect()
    }

    /// Per-node ring depth — useful for testing wrap behaviour.
    #[must_use]
    pub fn depth(&self, node_id: &str) -> usize {
        self.inner.get(node_id).map_or(0, |lock| lock.read().len())
    }

    /// Number of nodes that have received at least one message.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.inner.len()
    }

    fn record(&self, ev: &MessageDelivered<'_>) {
        let snapshot = MessageSnapshot {
            ts_ms: now_unix_millis(),
            from: ev.from.to_string(),
            to: ev.to.to_string(),
            message_type: ev.message_type.to_string(),
            ttl: ev.ttl,
            direct: ev.direct,
            accepted: ev.accepted,
        };
        // DashMap::entry has the right shape: insert empty deque
        // on first-seen node, then push under its lock.
        let to_owned = ev.to.to_string();
        let entry = self
            .inner
            .entry(to_owned)
            .or_insert_with(|| RwLock::new(VecDeque::with_capacity(self.capacity)));
        let mut deque = entry.value().write();
        if deque.len() == self.capacity {
            deque.pop_front();
        }
        deque.push_back(snapshot);
    }
}

impl TelemetrySink for RingBufferTelemetry {
    /// Record both accepted AND rejected deliveries — operators
    /// debugging policy issues need to see the rejected ones.
    /// Hot-path: see module-level discipline note.
    #[inline]
    fn on_message_delivered(&self, ev: &MessageDelivered<'_>) {
        self.record(ev);
    }

    // All other events default to no-op via the trait's default
    // impls. RingBufferTelemetry only cares about message
    // deliveries.
    #[inline]
    fn on_scheduler_tick_start(&self, _ev: &SchedulerTickStart) {}
    #[inline]
    fn on_scheduler_tick_end(&self, _ev: &SchedulerTickEnd) {}
    #[inline]
    fn on_queue_depths(&self, _ev: &QueueDepths) {}
    #[inline]
    fn on_node_evaluate_start(&self, _ev: &NodeEvaluateStart<'_>) {}
    #[inline]
    fn on_node_evaluate_end(&self, _ev: &NodeEvaluateEnd<'_>) {}
    #[inline]
    fn on_node_mounted(&self, _ev: &NodeMounted<'_>) {}
    #[inline]
    fn on_node_unmounted(&self, _ev: &NodeUnmounted<'_>) {}
    #[inline]
    fn on_topology_event(&self, _ev: &TopologyEventOccurred<'_>) {}
    #[inline]
    fn on_supervisor_decision(&self, _ev: &SupervisorDecisionMade<'_>) {}
    #[inline]
    fn on_remote_message(&self, _ev: &RemoteMessage<'_>) {}
    #[inline]
    fn on_hot_reload(&self, _ev: &HotReload<'_>) {}
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev<'a>(from: &'a str, to: &'a str, ty: &'a str) -> MessageDelivered<'a> {
        MessageDelivered {
            from,
            to,
            message_type: ty,
            ttl: 1,
            direct: false,
            accepted: true,
            trace_context: None,
        }
    }

    #[test]
    fn records_single_event_for_target() {
        let ring = RingBufferTelemetry::new();
        ring.on_message_delivered(&ev("alice", "bob", "ping"));
        assert_eq!(ring.depth("bob"), 1);
        assert_eq!(ring.depth("alice"), 0); // sender side empty
    }

    #[test]
    fn ring_wraps_at_capacity() {
        let ring = RingBufferTelemetry::with_capacity(3);
        for i in 0..10 {
            let ty = format!("msg{i}");
            ring.on_message_delivered(&ev("a", "b", ty.as_str()));
        }
        assert_eq!(ring.depth("b"), 3);
        let recent = ring.recent("b", 10);
        // Newest 3 are msg7, msg8, msg9 in chronological order.
        let types: Vec<&str> = recent.iter().map(|s| s.message_type.as_str()).collect();
        assert_eq!(types, vec!["msg7", "msg8", "msg9"]);
    }

    #[test]
    fn recent_returns_tail_n_in_chronological_order() {
        let ring = RingBufferTelemetry::with_capacity(64);
        for i in 0..5 {
            let ty = format!("hop{i}");
            ring.on_message_delivered(&ev("a", "b", ty.as_str()));
        }
        let recent = ring.recent("b", 3);
        let types: Vec<&str> = recent.iter().map(|s| s.message_type.as_str()).collect();
        assert_eq!(types, vec!["hop2", "hop3", "hop4"]);
    }

    #[test]
    fn unknown_node_returns_empty() {
        let ring = RingBufferTelemetry::new();
        assert!(ring.recent("ghost", 10).is_empty());
        assert_eq!(ring.depth("ghost"), 0);
    }

    #[test]
    fn limit_zero_returns_empty() {
        let ring = RingBufferTelemetry::new();
        ring.on_message_delivered(&ev("a", "b", "x"));
        assert!(ring.recent("b", 0).is_empty());
    }

    #[test]
    fn rejected_events_also_recorded() {
        let ring = RingBufferTelemetry::new();
        let mut e = ev("a", "b", "blocked");
        e.accepted = false;
        ring.on_message_delivered(&e);
        let recent = ring.recent("b", 1);
        assert_eq!(recent.len(), 1);
        assert!(!recent[0].accepted);
    }

    #[test]
    fn snapshot_contains_no_payload_field() {
        let ring = RingBufferTelemetry::new();
        ring.on_message_delivered(&ev("a", "b", "x"));
        let recent = ring.recent("b", 1);
        let s = recent.first().unwrap();
        let v = serde_json::to_value(s).unwrap();
        // Fields present:
        assert!(v.get("ts_ms").is_some());
        assert!(v.get("from").is_some());
        assert!(v.get("to").is_some());
        assert!(v.get("type").is_some());
        // PII-sensitive fields ABSENT (this is the safe-by-default
        // contract — see module docs).
        assert!(v.get("payload").is_none());
        assert!(v.get("body").is_none());
    }
}
