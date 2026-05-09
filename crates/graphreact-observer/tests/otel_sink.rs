//! **M16-Backend P10 verification gate.** OtelSink span-shape pinning
//! tests via `InMemorySpanExporter` from `opentelemetry_sdk`'s testing
//! feature. Drives the OtelSink through the 12 TelemetrySink events
//! and asserts the resulting span tree matches the documented shape
//! in `docs/backend/m16-otel.md`.
//!
//! Run via `cargo test -p graphreact-observer --features otel-test
//! --test otel_sink` (the `otel-test` feature pulls in
//! `opentelemetry_sdk/testing` which exposes `InMemorySpanExporter`).
//! Without the feature, the test compiles to nothing.

#![cfg(feature = "otel-test")]

use std::sync::Arc;

use graphreact_core::telemetry::{
    HotReload, MessageDelivered, NodeEvaluateEnd, NodeEvaluateStart, NodeMounted, NodeUnmounted,
    QueueDepths, QueueDepthsSnapshot, RemoteDirection, RemoteMessage, SchedulerTickEnd,
    SchedulerTickStart, SupervisorAction, SupervisorDecisionMade, TelemetrySink,
    TopologyEventOccurred,
};

use graphreact_observer::{decode_wire_context, encode_wire_context, OtelSink};
use opentelemetry::trace::{SpanId, SpanKind, TraceFlags, TraceId, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

/// Build an `OtelSink` backed by an `InMemorySpanExporter` so tests
/// can read back the exported spans in-process. Returns the sink +
/// the exporter (for `force_flush` + assertion) + the provider (for
/// shutdown). The provider holds a SimpleSpanProcessor (NOT batch)
/// because the test asserts spans synchronously without spinning up
/// a tokio runtime.
fn build_test_sink() -> (Arc<OtelSink>, InMemorySpanExporter, SdkTracerProvider) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("graphreact-observer-test");
    let sink = Arc::new(OtelSink::with_tracer(tracer));
    (sink, exporter, provider)
}

/// **P10 acceptance gate (local).** A `scheduler_tick_start` →
/// `scheduler_tick_end` pair produces exactly one root `scheduler.tick`
/// span with the expected `iterations` + `terminated_by_depth_cap`
/// attributes. No child spans when the tick is empty.
#[test]
fn scheduler_tick_emits_root_span_with_attributes() {
    let (sink, exporter, provider) = build_test_sink();
    sink.on_scheduler_tick_start(&SchedulerTickStart {});
    sink.on_scheduler_tick_end(&SchedulerTickEnd {
        iterations: 3,
        terminated_by_depth_cap: false,
    });
    provider.force_flush().expect("flush");
    let spans = exporter.get_finished_spans().expect("spans");
    assert_eq!(spans.len(), 1, "exactly one root tick span");
    let span = &spans[0];
    assert_eq!(span.name, "scheduler.tick");
    assert_eq!(span.span_kind, SpanKind::Internal);
    let iterations = span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "iterations")
        .expect("iterations attr")
        .value
        .as_str();
    assert_eq!(iterations, "3");
    let terminated = span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "terminated_by_depth_cap")
        .expect("terminated_by_depth_cap attr")
        .value
        .as_str();
    assert_eq!(terminated, "false");
}

/// `scheduler_tick_end` with `terminated_by_depth_cap=true` sets the
/// span status to error (so the OTel collector can flag it for ops
/// review without a separate metric).
#[test]
fn scheduler_tick_with_depth_cap_sets_error_status() {
    let (sink, exporter, provider) = build_test_sink();
    sink.on_scheduler_tick_start(&SchedulerTickStart {});
    sink.on_scheduler_tick_end(&SchedulerTickEnd {
        iterations: 64,
        terminated_by_depth_cap: true,
    });
    provider.force_flush().expect("flush");
    let spans = exporter.get_finished_spans().expect("spans");
    let span = &spans[0];
    assert!(
        matches!(
            span.status,
            opentelemetry::trace::Status::Error { .. }
        ),
        "depth-cap-terminated tick should have Error status, got: {:?}",
        span.status,
    );
}

/// **P10 acceptance gate.** A `node.evaluate` start/end pair under
/// an active root tick produces a child span parented to the root
/// (matching `parent_span_id` / `trace_id`).
#[test]
fn node_evaluate_creates_child_span_under_tick() {
    let (sink, exporter, provider) = build_test_sink();
    sink.on_scheduler_tick_start(&SchedulerTickStart {});
    sink.on_node_evaluate_start(&NodeEvaluateStart {
        node_id: "n1",
    });
    sink.on_node_evaluate_end(&NodeEvaluateEnd { node_id: "n1" });
    sink.on_scheduler_tick_end(&SchedulerTickEnd {
        iterations: 1,
        terminated_by_depth_cap: false,
    });
    provider.force_flush().expect("flush");
    let spans = exporter.get_finished_spans().expect("spans");
    assert_eq!(spans.len(), 2, "child node.evaluate + root tick");

    // Find the two spans.
    let child = spans
        .iter()
        .find(|s| s.name == "node.evaluate")
        .expect("node.evaluate span");
    let root = spans
        .iter()
        .find(|s| s.name == "scheduler.tick")
        .expect("scheduler.tick span");

    // Same trace.
    assert_eq!(
        child.span_context.trace_id(),
        root.span_context.trace_id(),
        "child + root must share trace_id"
    );
    // Child parents to root.
    assert_eq!(
        child.parent_span_id,
        root.span_context.span_id(),
        "child's parent_span_id must match root's span_id"
    );

    // Child carries node.id attribute.
    let node_id = child
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "node.id")
        .expect("node.id attr")
        .value
        .as_str();
    assert_eq!(node_id, "n1");
}

/// **P10 acceptance gate.** `on_message_delivered` produces a one-shot
/// span with the documented attributes; lives under the active tick.
#[test]
fn message_delivered_emits_one_shot_span_with_attributes() {
    let (sink, exporter, provider) = build_test_sink();
    sink.on_scheduler_tick_start(&SchedulerTickStart {});
    sink.on_message_delivered(&MessageDelivered {
        from: "alice",
        to: "bob",
        message_type: "ping",
        ttl: 7,
        direct: false,
        accepted: true,
        trace_context: None,
    });
    sink.on_scheduler_tick_end(&SchedulerTickEnd {
        iterations: 1,
        terminated_by_depth_cap: false,
    });
    provider.force_flush().expect("flush");
    let spans = exporter.get_finished_spans().expect("spans");
    let msg_span = spans
        .iter()
        .find(|s| s.name == "message.deliver")
        .expect("message.deliver span");
    assert_eq!(msg_span.span_kind, SpanKind::Producer);
    let from = msg_span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "from")
        .expect("from attr")
        .value
        .as_str();
    assert_eq!(from, "alice");
    let msg_type = msg_span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "type")
        .expect("type attr")
        .value
        .as_str();
    assert_eq!(msg_type, "ping");
}

/// **P10 acceptance gate (cross-node).** When `MessageDelivered`
/// carries a `trace_context` (set by the dist crate's
/// `dispatch_inbound` from the wire packet), the resulting
/// `message.deliver` span includes an OTel `Link` to the remote
/// (TraceID, SpanID). This is the gate the canonical-plan
/// "3-hop messages produce 3-link traces" line requires.
#[test]
fn cross_node_message_delivered_produces_link_attribute() {
    let (sink, exporter, provider) = build_test_sink();
    // Pre-existing remote span context (e.g. the sender's
    // `scheduler.tick` span propagated through the wire packet).
    let remote_trace = TraceId::from_bytes([
        0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf,
        0xb0,
    ]);
    let remote_span = SpanId::from_bytes([0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8]);
    let remote_sc = opentelemetry::trace::SpanContext::new(
        remote_trace,
        remote_span,
        TraceFlags::SAMPLED,
        true,
        opentelemetry::trace::TraceState::default(),
    );
    let wire_bytes = encode_wire_context(&remote_sc).expect("encode");

    sink.on_scheduler_tick_start(&SchedulerTickStart {});
    sink.on_message_delivered(&MessageDelivered {
        from: "alice@peer-a",
        to: "bob",
        message_type: "ping",
        ttl: 6,
        direct: false,
        accepted: true,
        trace_context: Some(wire_bytes),
    });
    sink.on_scheduler_tick_end(&SchedulerTickEnd {
        iterations: 1,
        terminated_by_depth_cap: false,
    });
    provider.force_flush().expect("flush");

    let spans = exporter.get_finished_spans().expect("spans");
    let msg_span = spans
        .iter()
        .find(|s| s.name == "message.deliver")
        .expect("message.deliver span");
    assert_eq!(msg_span.links.len(), 1, "exactly one cross-node link");
    let link = &msg_span.links[0];
    assert_eq!(
        link.span_context.trace_id(),
        remote_trace,
        "link points at remote trace_id"
    );
    assert_eq!(
        link.span_context.span_id(),
        remote_span,
        "link points at remote span_id"
    );
}

/// **P10 acceptance gate.** Mount + unmount + restart + hot-reload
/// + topology + supervisor events fire as instant span events on
/// the active root tick span (NOT as their own spans).
#[test]
fn instant_events_fire_on_root_tick_span() {
    let (sink, exporter, provider) = build_test_sink();
    sink.on_scheduler_tick_start(&SchedulerTickStart {});

    sink.on_node_mounted(&NodeMounted { node_id: "n1" });
    sink.on_node_unmounted(&NodeUnmounted {
        node_id: "n2",
        reason: "shutdown",
        reason_detail: None,
    });
    sink.on_topology_event(&TopologyEventOccurred {
        kind: "node_added",
        node_id: Some("n3"),
        edge_u: None,
        edge_v: None,
        edge_key: None,
    });
    sink.on_supervisor_decision(&SupervisorDecisionMade {
        failed_id: "n4",
        reason: "crashed",
        action: SupervisorAction::Proceeded,
    });
    sink.on_remote_message(&RemoteMessage {
        direction: RemoteDirection::Inbound,
        peer: "peer-b",
        from: "alice@peer-b",
        to: "bob",
        message_type: "ping",
        byte_size: 256,
    });
    sink.on_hot_reload(&HotReload {
        source_path: Some("/tmp/fixture.grx"),
        origin_peer: None,
        nodes_replaced: 2,
        nodes_remounted: 1,
        edges_changed: 0,
        rolled_back: false,
        error_count: 0,
    });
    sink.on_queue_depths(&QueueDepths {
        depth: 4,
        snapshot: QueueDepthsSnapshot {
            messages: 12,
            dirty: 3,
            pending_topology: 0,
            external_inbox: 0,
            pending_mutations: 1,
        },
    });

    sink.on_scheduler_tick_end(&SchedulerTickEnd {
        iterations: 1,
        terminated_by_depth_cap: false,
    });
    provider.force_flush().expect("flush");

    let spans = exporter.get_finished_spans().expect("spans");
    let root = spans
        .iter()
        .find(|s| s.name == "scheduler.tick")
        .expect("root tick span");

    // All events landed on the root span as instant events.
    let event_names: Vec<&str> = root
        .events
        .iter()
        .map(|e| e.name.as_ref())
        .collect();
    assert!(event_names.contains(&"node.mount"), "events: {event_names:?}");
    assert!(event_names.contains(&"node.unmount"), "events: {event_names:?}");
    assert!(event_names.contains(&"topology.event"), "events: {event_names:?}");
    assert!(
        event_names.contains(&"supervisor.decision"),
        "events: {event_names:?}"
    );
    assert!(event_names.contains(&"remote.message"), "events: {event_names:?}");
    assert!(event_names.contains(&"hot.reload"), "events: {event_names:?}");
    assert!(event_names.contains(&"queue.depths"), "events: {event_names:?}");
}

/// `current_wire_context` returns Some when an OTel span is active
/// on the current thread, None otherwise. The cross-node propagation
/// path relies on this for the `WireTracingHook::extract_outbound`
/// implementation.
#[test]
fn current_wire_context_returns_active_span_id() {
    let (sink, _exporter, _provider) = build_test_sink();
    // Before any span: None.
    assert!(OtelSink::current_wire_context().is_none());
    // Open a tick span: now active on this thread.
    sink.on_scheduler_tick_start(&SchedulerTickStart {});
    let active = OtelSink::current_wire_context();
    assert!(active.is_some(), "tick span should make context available");
    let bytes = active.unwrap();
    // First 16 bytes = trace_id; not all-zero for a valid span.
    assert!(bytes[..16].iter().any(|&b| b != 0));
    assert!(bytes[16..].iter().any(|&b| b != 0));
    sink.on_scheduler_tick_end(&SchedulerTickEnd {
        iterations: 1,
        terminated_by_depth_cap: false,
    });
    // After tick ends: None.
    assert!(OtelSink::current_wire_context().is_none());
}

/// `decode_wire_context` round-trip: encode → decode → encode again
/// produces identical bytes. Pins the wire format against drift.
#[test]
fn wire_context_decode_round_trips_through_link() {
    let trace_id = TraceId::from_bytes([
        0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04, 0xfe, 0xed, 0xfa, 0xce, 0x05, 0x06, 0x07,
        0x08,
    ]);
    let span_id = SpanId::from_bytes([0xca, 0xfe, 0xba, 0xbe, 0x09, 0x0a, 0x0b, 0x0c]);
    let sc = opentelemetry::trace::SpanContext::new(
        trace_id,
        span_id,
        TraceFlags::SAMPLED,
        true,
        opentelemetry::trace::TraceState::default(),
    );
    let bytes = encode_wire_context(&sc).expect("encode");
    let decoded = decode_wire_context(&bytes);
    assert_eq!(decoded.trace_id(), trace_id);
    assert_eq!(decoded.span_id(), span_id);
    let bytes_again = encode_wire_context(&decoded).expect("re-encode");
    assert_eq!(bytes, bytes_again);
}
