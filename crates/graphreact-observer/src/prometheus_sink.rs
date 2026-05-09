//! **M16-Backend Phase 2.** [`TelemetrySink`] consumer that writes
//! into a shared [`MetricsInner`]. The HTTP `/metrics` handler reads
//! the same `Arc<MetricsInner>` and renders it via
//! [`crate::prometheus::encode_text`].
//!
//! ## Hot-path discipline
//!
//! Every method body is one-or-two atomic increments + a possible
//! one-time `DashMap::entry` allocation per never-before-seen label
//! tuple. The ~25-40ns per-event budget is what powers the bench-(b)
//! ≤1% gate for the M14 ring fixture — see
//! `graphreact-observer/benches/scheduler_overhead.rs` `case_d`.
//!
//! ## Tick → histogram pairing
//!
//! `on_scheduler_tick_start` stashes an [`Instant`] in a thread-local
//! cell; `on_scheduler_tick_end` reads + subtracts it to feed
//! [`Histogram::observe`]. The scheduler is single-threaded
//! (LocalSet), so the TLS pair always sees its own start. Any
//! `tick_end` without a stashed start (defensive) silently skips
//! the observation rather than emitting a zero-duration sample.

use std::cell::Cell;
use std::sync::Arc;
use std::time::Instant;

use graphreact_core::{
    HotReload, MessageDelivered, NodeEvaluateEnd, NodeEvaluateStart, NodeMounted, NodeUnmounted,
    QueueDepths, RemoteDirection, RemoteMessage, SchedulerTickEnd, SchedulerTickStart,
    SupervisorAction, SupervisorDecisionMade, TelemetrySink, TopologyEventOccurred,
};

use crate::metrics::MetricsInner;

thread_local! {
    /// Per-thread cradle for the most recent `tick_start` instant.
    /// Read + cleared in `on_scheduler_tick_end`. `Cell<Option<...>>`
    /// avoids the cost of `RefCell`'s borrow tracking on a hot path
    /// the scheduler's own `process_batch` calls every cascade round.
    static TICK_START: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Counter-only [`TelemetrySink`] that aggregates events into a
/// shared [`MetricsInner`]. The Prometheus exporter reads the same
/// inner via [`Self::inner`].
#[derive(Clone)]
pub struct PrometheusSink {
    inner: Arc<MetricsInner>,
}

impl Default for PrometheusSink {
    fn default() -> Self {
        Self::new()
    }
}

impl PrometheusSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MetricsInner::new()),
        }
    }

    /// Construct from an existing `Arc<MetricsInner>`. Useful when
    /// the HTTP server constructs the inner first + hands it to both
    /// the sink and the scrape handler.
    #[must_use]
    pub fn from_arc(inner: Arc<MetricsInner>) -> Self {
        Self { inner }
    }

    /// Shared handle to the metric storage. The HTTP `/metrics`
    /// handler clones this and renders against it.
    #[must_use]
    pub fn inner(&self) -> &Arc<MetricsInner> {
        &self.inner
    }

    /// Convenience: wrap into `Arc<dyn TelemetrySink>` for direct
    /// `Scheduler::install_telemetry_sink` use.
    #[must_use]
    pub fn into_arc(self) -> Arc<dyn TelemetrySink> {
        Arc::new(self) as Arc<dyn TelemetrySink>
    }
}

impl TelemetrySink for PrometheusSink {
    #[inline]
    fn on_scheduler_tick_start(&self, _ev: &SchedulerTickStart) {
        TICK_START.with(|c| c.set(Some(Instant::now())));
    }

    #[inline]
    fn on_scheduler_tick_end(&self, ev: &SchedulerTickEnd) {
        // Pair with the matching tick_start to drive the histogram.
        // `take()` clears the cell so a stray `on_scheduler_tick_end`
        // without a preceding start (test fixtures, defensive
        // ordering) silently skips rather than reporting a bogus
        // sample.
        TICK_START.with(|c| {
            if let Some(start) = c.take() {
                let elapsed = start.elapsed().as_nanos();
                let elapsed_u64 = u64::try_from(elapsed).unwrap_or(u64::MAX);
                self.inner.process_batch_duration.observe(elapsed_u64);
            }
        });
        if ev.terminated_by_depth_cap {
            // Cascade saturated: account every queued mutation that
            // didn't get to run as a "max_depth" drop. We don't have
            // a precise count here (the queue-depth snapshot is at a
            // round boundary, not at termination), so we increment
            // ONCE per terminated tick — operators see "ticks
            // saturated", not "messages dropped". Documented in the
            // metric's HELP line.
            self.inner.messages_dropped_inc("max_depth");
        }
    }

    #[inline]
    fn on_queue_depths(&self, ev: &QueueDepths) {
        // Last-write-wins gauges — value is the current depth at
        // this cascade round's classification step.
        let snap = ev.snapshot;
        self.inner
            .queue_depth_messages
            .set(u64::try_from(snap.messages).unwrap_or(u64::MAX));
        self.inner
            .queue_depth_dirty
            .set(u64::try_from(snap.dirty).unwrap_or(u64::MAX));
        self.inner
            .queue_depth_pending_topology
            .set(u64::try_from(snap.pending_topology).unwrap_or(u64::MAX));
        self.inner
            .queue_depth_external_inbox
            .set(u64::try_from(snap.external_inbox).unwrap_or(u64::MAX));
        self.inner
            .queue_depth_pending_mutations
            .set(u64::try_from(snap.pending_mutations).unwrap_or(u64::MAX));
    }

    #[inline]
    fn on_node_evaluate_start(&self, _ev: &NodeEvaluateStart<'_>) {
        // Pair-only event used by Phase-10 OTel; Prometheus emits
        // the count in `on_node_evaluate_end` so we don't double.
    }

    #[inline]
    fn on_node_evaluate_end(&self, ev: &NodeEvaluateEnd<'_>) {
        self.inner.node_renders_inc(ev.node_id);
    }

    #[inline]
    fn on_message_delivered(&self, ev: &MessageDelivered<'_>) {
        if ev.accepted {
            self.inner
                .messages_delivered_inc(ev.from, ev.to, ev.message_type);
        } else {
            // Channel-policy / accept-list / transform path rejected
            // the message. The bounded reason here is
            // "policy_rejected"; an operator who needs per-handler
            // breakdown gets it from the JSON-log stream.
            self.inner.messages_dropped_inc("policy_rejected");
        }
    }

    #[inline]
    fn on_node_mounted(&self, _ev: &NodeMounted<'_>) {
        self.inner.node_mount_total.inc();
    }

    #[inline]
    fn on_node_unmounted(&self, ev: &NodeUnmounted<'_>) {
        // ev.reason is one of the stable strings emitted by
        // `ExitReason::as_str()` ("normal", "shutdown", "crashed",
        // "killed", etc.). Cardinality is bounded by the variant
        // set in graphreact-core's ExitReason.
        self.inner.node_unmount_inc(ev.reason);
    }

    #[inline]
    fn on_topology_event(&self, _ev: &TopologyEventOccurred<'_>) {
        // No metric in the P2 spec — the JSON log stream carries
        // topology events. Phase 5+ may add a counter; deliberately
        // omitted here to keep the cardinality budget tight.
    }

    #[inline]
    fn on_supervisor_decision(&self, ev: &SupervisorDecisionMade<'_>) {
        let action = match ev.action {
            SupervisorAction::Proceeded => "proceeded",
            SupervisorAction::Deferred => "deferred",
            SupervisorAction::Escalated => "escalated",
        };
        self.inner.supervisor_restarts_inc(action);
    }

    #[inline]
    fn on_remote_message(&self, ev: &RemoteMessage<'_>) {
        let direction = match ev.direction {
            RemoteDirection::Inbound => "inbound",
            RemoteDirection::Outbound => "outbound",
        };
        self.inner.remote_messages_routed_inc(ev.peer, direction);
    }

    #[inline]
    fn on_hot_reload(&self, ev: &HotReload<'_>) {
        let outcome = if ev.rolled_back {
            "rolled_back"
        } else {
            "applied"
        };
        self.inner.hot_reload_inc(outcome);
    }
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;
    use std::time::Duration;

    use graphreact_core::QueueDepthsSnapshot;

    use super::*;

    #[test]
    fn message_delivered_increments_dynamic_label() {
        let sink = PrometheusSink::new();
        sink.on_message_delivered(&MessageDelivered {
            from: "a",
            to: "b",
            message_type: "ping",
            ttl: 1,
            direct: false,
            accepted: true,
            trace_context: None,
        });
        sink.on_message_delivered(&MessageDelivered {
            from: "a",
            to: "b",
            message_type: "ping",
            ttl: 1,
            direct: false,
            accepted: true,
            trace_context: None,
        });
        let total: u64 = sink
            .inner()
            .messages_delivered
            .iter()
            .map(|e| e.value().counter.get())
            .sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn rejected_messages_increment_dropped() {
        let sink = PrometheusSink::new();
        sink.on_message_delivered(&MessageDelivered {
            from: "a",
            to: "b",
            message_type: "ping",
            ttl: 1,
            direct: false,
            accepted: false,
            trace_context: None,
        });
        let total: u64 = sink
            .inner()
            .messages_dropped
            .iter()
            .map(|e| e.value().counter.get())
            .sum();
        assert_eq!(total, 1);
    }

    #[test]
    fn tick_pair_observes_histogram() {
        let sink = PrometheusSink::new();
        sink.on_scheduler_tick_start(&SchedulerTickStart::default());
        sleep(Duration::from_micros(10));
        sink.on_scheduler_tick_end(&SchedulerTickEnd {
            iterations: 1,
            terminated_by_depth_cap: false,
        });
        let snap = sink.inner().process_batch_duration.snapshot();
        assert_eq!(snap.count, 1);
        assert!(snap.sum_ns > 0);
    }

    #[test]
    fn depth_cap_termination_drops_max_depth() {
        let sink = PrometheusSink::new();
        sink.on_scheduler_tick_start(&SchedulerTickStart::default());
        sink.on_scheduler_tick_end(&SchedulerTickEnd {
            iterations: 100,
            terminated_by_depth_cap: true,
        });
        let total: u64 = sink
            .inner()
            .messages_dropped
            .iter()
            .map(|e| e.value().counter.get())
            .sum();
        assert_eq!(total, 1);
    }

    #[test]
    fn queue_depths_set_gauges() {
        let sink = PrometheusSink::new();
        sink.on_queue_depths(&QueueDepths {
            depth: 1,
            snapshot: QueueDepthsSnapshot {
                messages: 5,
                dirty: 2,
                pending_topology: 1,
                external_inbox: 0,
                pending_mutations: 3,
            },
        });
        assert_eq!(sink.inner().queue_depth_messages.get(), 5);
        assert_eq!(sink.inner().queue_depth_dirty.get(), 2);
        assert_eq!(sink.inner().queue_depth_pending_topology.get(), 1);
        assert_eq!(sink.inner().queue_depth_external_inbox.get(), 0);
        assert_eq!(sink.inner().queue_depth_pending_mutations.get(), 3);
    }

    #[test]
    fn supervisor_action_uses_bounded_label() {
        let sink = PrometheusSink::new();
        sink.on_supervisor_decision(&SupervisorDecisionMade {
            failed_id: "child",
            reason: "crashed",
            action: SupervisorAction::Proceeded,
        });
        sink.on_supervisor_decision(&SupervisorDecisionMade {
            failed_id: "child2",
            reason: "crashed",
            action: SupervisorAction::Proceeded,
        });
        sink.on_supervisor_decision(&SupervisorDecisionMade {
            failed_id: "child",
            reason: "crashed",
            action: SupervisorAction::Deferred,
        });
        // 2 unique action labels: "proceeded" and "deferred"
        assert_eq!(sink.inner().supervisor_restarts.len(), 2);
    }

    #[test]
    fn hot_reload_outcome_distinguishes_rolled_back() {
        let sink = PrometheusSink::new();
        sink.on_hot_reload(&HotReload {
            source_path: None,
            origin_peer: None,
            nodes_replaced: 1,
            nodes_remounted: 0,
            edges_changed: 0,
            rolled_back: false,
            error_count: 0,
        });
        sink.on_hot_reload(&HotReload {
            source_path: None,
            origin_peer: None,
            nodes_replaced: 0,
            nodes_remounted: 0,
            edges_changed: 0,
            rolled_back: true,
            error_count: 1,
        });
        assert_eq!(sink.inner().hot_reload.len(), 2);
    }
}
