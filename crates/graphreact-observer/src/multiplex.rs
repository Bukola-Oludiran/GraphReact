//! Fan-out [`TelemetrySink`] combinator. Lets a process attach more
//! than one consumer (Prometheus + JSON logs + WebSocket fan-out + OTel)
//! through the scheduler's single `install_telemetry_sink` slot.
//!
//! ## Why this lives in `graphreact-observer`, not `graphreact-core`
//!
//! Core defines the trait so the scheduler can call it without
//! depending on this crate (mirrors how `RemoteSink` lives in core
//! but `RemoteSupervision` lives in `graphreact-dist`). Combinator
//! impls + concrete consumers live here so core stays focused on the
//! runtime contract.
//!
//! ## Construction patterns
//!
//! - `MultiplexSink::new(vec![sink_a, sink_b])` — explicit list at
//!   construction time. Sinks are immutable after construction.
//! - `MultiplexSink::builder()` — incremental; useful when consumer
//!   set is built up across configuration steps.

use std::sync::Arc;

use graphreact_core::{
    HotReload, MessageDelivered, NodeEvaluateEnd, NodeEvaluateStart, NodeMounted, NodeUnmounted,
    QueueDepths, RemoteMessage, SchedulerTickEnd, SchedulerTickStart, SupervisorDecisionMade,
    TelemetrySink, TopologyEventOccurred,
};

/// Fan-out sink. Holds `Arc<dyn TelemetrySink>` per consumer; every
/// trait method delegates to each in registration order.
///
/// Sinks fire in registration order (deterministic) so test fixtures
/// can assert on event ordering across consumers. Order is NOT a
/// production semantic guarantee — Phase-2+ consumers should be
/// independent of one another.
pub struct MultiplexSink {
    sinks: Vec<Arc<dyn TelemetrySink>>,
}

impl MultiplexSink {
    /// Construct from an explicit list. Empty lists are valid (the
    /// resulting sink is equivalent to a no-op).
    #[must_use]
    pub fn new(sinks: Vec<Arc<dyn TelemetrySink>>) -> Self {
        Self { sinks }
    }

    /// Builder-style construction: start empty, push sinks
    /// incrementally. Phase-2+ wiring uses this when a daemon's
    /// configuration enables some surfaces (Prometheus) but not
    /// others (OTel feature-gated).
    #[must_use]
    pub fn builder() -> MultiplexSinkBuilder {
        MultiplexSinkBuilder::default()
    }

    /// Number of consumers attached. Test surface.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sinks.len()
    }

    /// `true` when no consumers are attached. Test surface.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }
}

/// Builder for [`MultiplexSink`]. See [`MultiplexSink::builder`].
#[derive(Default)]
pub struct MultiplexSinkBuilder {
    sinks: Vec<Arc<dyn TelemetrySink>>,
}

impl MultiplexSinkBuilder {
    /// Append a sink. Order is preserved.
    #[must_use]
    pub fn push(mut self, sink: Arc<dyn TelemetrySink>) -> Self {
        self.sinks.push(sink);
        self
    }

    /// Finalize into a [`MultiplexSink`].
    #[must_use]
    pub fn build(self) -> MultiplexSink {
        MultiplexSink::new(self.sinks)
    }
}

impl TelemetrySink for MultiplexSink {
    #[inline]
    fn on_scheduler_tick_start(&self, ev: &SchedulerTickStart) {
        for sink in &self.sinks {
            sink.on_scheduler_tick_start(ev);
        }
    }

    #[inline]
    fn on_scheduler_tick_end(&self, ev: &SchedulerTickEnd) {
        for sink in &self.sinks {
            sink.on_scheduler_tick_end(ev);
        }
    }

    #[inline]
    fn on_queue_depths(&self, ev: &QueueDepths) {
        for sink in &self.sinks {
            sink.on_queue_depths(ev);
        }
    }

    #[inline]
    fn on_node_evaluate_start(&self, ev: &NodeEvaluateStart<'_>) {
        for sink in &self.sinks {
            sink.on_node_evaluate_start(ev);
        }
    }

    #[inline]
    fn on_node_evaluate_end(&self, ev: &NodeEvaluateEnd<'_>) {
        for sink in &self.sinks {
            sink.on_node_evaluate_end(ev);
        }
    }

    #[inline]
    fn on_message_delivered(&self, ev: &MessageDelivered<'_>) {
        for sink in &self.sinks {
            sink.on_message_delivered(ev);
        }
    }

    #[inline]
    fn on_node_mounted(&self, ev: &NodeMounted<'_>) {
        for sink in &self.sinks {
            sink.on_node_mounted(ev);
        }
    }

    #[inline]
    fn on_node_unmounted(&self, ev: &NodeUnmounted<'_>) {
        for sink in &self.sinks {
            sink.on_node_unmounted(ev);
        }
    }

    #[inline]
    fn on_topology_event(&self, ev: &TopologyEventOccurred<'_>) {
        for sink in &self.sinks {
            sink.on_topology_event(ev);
        }
    }

    #[inline]
    fn on_supervisor_decision(&self, ev: &SupervisorDecisionMade<'_>) {
        for sink in &self.sinks {
            sink.on_supervisor_decision(ev);
        }
    }

    #[inline]
    fn on_remote_message(&self, ev: &RemoteMessage<'_>) {
        for sink in &self.sinks {
            sink.on_remote_message(ev);
        }
    }

    #[inline]
    fn on_hot_reload(&self, ev: &HotReload<'_>) {
        for sink in &self.sinks {
            sink.on_hot_reload(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct CountingSink(AtomicUsize);

    impl TelemetrySink for CountingSink {
        fn on_scheduler_tick_start(&self, _ev: &SchedulerTickStart) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn fans_out_to_each_sink() {
        let a = Arc::new(CountingSink::default());
        let b = Arc::new(CountingSink::default());
        let mux = MultiplexSink::new(vec![
            a.clone() as Arc<dyn TelemetrySink>,
            b.clone() as Arc<dyn TelemetrySink>,
        ]);
        mux.on_scheduler_tick_start(&SchedulerTickStart::default());
        mux.on_scheduler_tick_start(&SchedulerTickStart::default());
        assert_eq!(a.0.load(Ordering::Relaxed), 2);
        assert_eq!(b.0.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn empty_mux_is_a_noop() {
        let mux = MultiplexSink::new(Vec::new());
        assert!(mux.is_empty());
        // Should not panic.
        mux.on_scheduler_tick_start(&SchedulerTickStart::default());
    }

    #[test]
    fn builder_preserves_order() {
        let a = Arc::new(CountingSink::default());
        let b = Arc::new(CountingSink::default());
        let mux = MultiplexSink::builder()
            .push(a.clone() as Arc<dyn TelemetrySink>)
            .push(b.clone() as Arc<dyn TelemetrySink>)
            .build();
        assert_eq!(mux.len(), 2);
        mux.on_scheduler_tick_start(&SchedulerTickStart::default());
        assert_eq!(a.0.load(Ordering::Relaxed), 1);
        assert_eq!(b.0.load(Ordering::Relaxed), 1);
    }
}
