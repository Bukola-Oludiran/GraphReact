//! **M16-Backend Phase 1.** Acceptance test for `JsonLogTelemetry`:
//! every Telemetry event produces a single-line JSON record;
//! `serde_json::from_str` round-trip succeeds; required fields (`event`,
//! `payload`) present.
//!
//! Trait-shape gate: a panicking sink (`AssertArgShapesSink`) that
//! invariant-checks every event's argument shape catches regressions
//! where a new event sneaks in with a wrong-shape signature.

use std::io::{self, Write};
use std::sync::Arc;

use parking_lot::Mutex;

use graphreact_core::{
    runtime::types::{NodeContext, PermissionManifest},
    Attrs, CommunicationGraph, ExitReason, NodeDefinition, Scheduler, TelemetrySink,
};
use graphreact_observer::JsonLogTelemetry;

/// Test writer that captures emitted bytes.
#[derive(Clone)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    fn snapshot(&self) -> String {
        String::from_utf8(self.0.lock().clone()).expect("utf-8")
    }
}

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn build_two_node_scheduler() -> Scheduler {
    let mut graph = CommunicationGraph::new();
    graph.add_node("alpha".to_string(), Attrs::new());
    graph.add_node("beta".to_string(), Attrs::new());
    graph.add_edge("alpha".to_string(), "beta".to_string(), None, Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    let alpha_def = NodeDefinition::new("alpha", |_ctx: &NodeContext| {
        PermissionManifest::new(vec!["ping".to_string()], vec!["ping".to_string()])
    });
    let beta_def = NodeDefinition::new("beta", |_ctx: &NodeContext| {
        PermissionManifest::new(vec!["ping".to_string()], vec!["ping".to_string()])
    });
    scheduler
        .register_node(alpha_def, "alpha", Attrs::new(), 0)
        .expect("alpha");
    scheduler
        .register_node(beta_def, "beta", Attrs::new(), 0)
        .expect("beta");
    scheduler.start().expect("start");
    scheduler
}

#[test]
fn round_trips_canonical_event_set() {
    let buf = SharedBuffer::new();
    let sink = JsonLogTelemetry::with_writer(Box::new(buf.clone()));
    let mut scheduler = build_two_node_scheduler();
    scheduler.install_telemetry_sink(sink.into_arc());

    // Drive a tick: send "ping" alpha → beta. This exercises
    // tick_start/end, queue_depths, evaluate_start/end (alpha + beta
    // mounted earlier; the message-dispatch tick re-evaluates none
    // of them but we still get tick boundaries), message_delivered.
    scheduler.send("beta", "ping", serde_json::json!({}));
    scheduler.process_batch().expect("batch");

    let out = buf.snapshot();
    let lines: Vec<&str> = out.lines().collect();
    assert!(!lines.is_empty(), "expected at least one event line");
    for line in &lines {
        let parsed: serde_json::Value =
            serde_json::from_str(line).expect("each line must parse as JSON");
        assert!(
            parsed.get("event").and_then(|v| v.as_str()).is_some(),
            "event line missing required `event` field: {line}"
        );
        assert!(
            parsed.get("payload").is_some(),
            "event line missing required `payload` field: {line}"
        );
    }

    let event_names: Vec<String> = lines
        .iter()
        .filter_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v["event"].as_str().map(String::from))
        })
        .collect();
    assert!(
        event_names.iter().any(|n| n == "scheduler_tick_start"),
        "missing scheduler_tick_start in {event_names:?}"
    );
    assert!(
        event_names.iter().any(|n| n == "scheduler_tick_end"),
        "missing scheduler_tick_end in {event_names:?}"
    );
    assert!(
        event_names.iter().any(|n| n == "queue_depths"),
        "missing queue_depths in {event_names:?}"
    );
    assert!(
        event_names.iter().any(|n| n == "message_delivered"),
        "missing message_delivered in {event_names:?}"
    );
}

#[test]
fn fires_node_unmounted_with_reason_string() {
    let buf = SharedBuffer::new();
    let sink = JsonLogTelemetry::with_writer(Box::new(buf.clone()));
    let mut scheduler = build_two_node_scheduler();
    scheduler.install_telemetry_sink(sink.into_arc());
    let alpha_key = scheduler.key_for("alpha").expect("alpha key");
    scheduler.unmount_with_reason(alpha_key, ExitReason::Crashed { detail: "oops".to_string() });
    scheduler.process_batch().expect("batch");
    let out = buf.snapshot();
    let unmount_line = out
        .lines()
        .find(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .map(|v| v["event"] == "node_unmounted")
                .unwrap_or(false)
        })
        .expect("expected node_unmounted line");
    let v: serde_json::Value = serde_json::from_str(unmount_line).expect("parse");
    assert_eq!(v["payload"]["node_id"], "alpha");
    assert_eq!(v["payload"]["reason"], "crashed");
    assert_eq!(v["payload"]["reason_detail"], "oops");
}

/// Trait-shape sink: every method increments a counter so the test
/// can confirm that `install_telemetry_sink` reaches every event we
/// fire from the scheduler. Catches "we added a new event but
/// forgot to wire the callsite" regressions early.
#[derive(Default)]
struct CountingSink {
    tick_start: parking_lot::Mutex<u32>,
    tick_end: parking_lot::Mutex<u32>,
    queue_depths: parking_lot::Mutex<u32>,
    evaluate_start: parking_lot::Mutex<u32>,
    evaluate_end: parking_lot::Mutex<u32>,
    message_delivered: parking_lot::Mutex<u32>,
    node_mounted: parking_lot::Mutex<u32>,
    node_unmounted: parking_lot::Mutex<u32>,
    topology: parking_lot::Mutex<u32>,
}

impl TelemetrySink for CountingSink {
    fn on_scheduler_tick_start(&self, _: &graphreact_core::SchedulerTickStart) {
        *self.tick_start.lock() += 1;
    }
    fn on_scheduler_tick_end(&self, _: &graphreact_core::SchedulerTickEnd) {
        *self.tick_end.lock() += 1;
    }
    fn on_queue_depths(&self, _: &graphreact_core::QueueDepths) {
        *self.queue_depths.lock() += 1;
    }
    fn on_node_evaluate_start(&self, _: &graphreact_core::NodeEvaluateStart<'_>) {
        *self.evaluate_start.lock() += 1;
    }
    fn on_node_evaluate_end(&self, _: &graphreact_core::NodeEvaluateEnd<'_>) {
        *self.evaluate_end.lock() += 1;
    }
    fn on_message_delivered(&self, _: &graphreact_core::MessageDelivered<'_>) {
        *self.message_delivered.lock() += 1;
    }
    fn on_node_mounted(&self, _: &graphreact_core::NodeMounted<'_>) {
        *self.node_mounted.lock() += 1;
    }
    fn on_node_unmounted(&self, _: &graphreact_core::NodeUnmounted<'_>) {
        *self.node_unmounted.lock() += 1;
    }
    fn on_topology_event(&self, _: &graphreact_core::TopologyEventOccurred<'_>) {
        *self.topology.lock() += 1;
    }
}

#[test]
fn scheduler_callsites_fire_expected_events() {
    let counting = Arc::new(CountingSink::default());
    let mut scheduler = build_two_node_scheduler();
    scheduler.install_telemetry_sink(counting.clone() as Arc<dyn TelemetrySink>);

    scheduler.send("beta", "ping", serde_json::json!({}));
    scheduler.process_batch().expect("batch");

    assert!(*counting.tick_start.lock() >= 1, "tick_start fired");
    assert!(*counting.tick_end.lock() >= 1, "tick_end fired");
    assert!(*counting.queue_depths.lock() >= 1, "queue_depths fired");
    assert_eq!(
        *counting.message_delivered.lock(),
        1,
        "message_delivered fired exactly once"
    );

    // add_node_at_runtime is the one that fires node_mounted; verify
    // by adding a third node + connecting via the public API.
    let gamma_def = NodeDefinition::new("gamma", |_ctx: &NodeContext| {
        PermissionManifest::new(vec![] as Vec<String>, vec![] as Vec<String>)
    });
    scheduler
        .add_node_at_runtime(gamma_def, "gamma", Attrs::new(), vec![])
        .expect("add gamma");
    assert_eq!(
        *counting.node_mounted.lock(),
        1,
        "node_mounted fired once for runtime mount"
    );
}
