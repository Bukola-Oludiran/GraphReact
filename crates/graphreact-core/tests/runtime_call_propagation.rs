//! **M-Ingress phase 2 acceptance gate.** End-to-end auto-propagation
//! of `Message.correlation_id` through `Scheduler::deliver` →
//! `make_emit` → `on_call_reply`.
//!
//! What this exercises (the substrate):
//! - `SchedulerHandle::send_ingress_with_correlation` injects a
//!   message with `from = "__ingress__"` and a populated
//!   `correlation_id`.
//! - `Scheduler::deliver` consults the target's
//!   `manifest.accepts_external_call` for a matching `request_type`
//!   and captures `(correlation_id, reply_type)` per-handler-
//!   invocation.
//! - `make_emit`, when the handler emits, auto-tags the FIRST
//!   matching outgoing message with the correlation_id (first-match-
//!   only) and fires `on_call_reply` so the higher-layer
//!   `PendingCalls` registry can resolve.
//! - The `accepts_external_call` gate IS default-deny (no entry +
//!   no `accepts_external` entry → reject).
//! - Multi-emit handlers tag exactly the first matching outgoing
//!   message, NOT subsequent ones.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use graphreact_core::{
    use_emit, use_message, Attrs, CallReply, CommunicationGraph, EmitOptions, ExternalCall,
    HotReload, MessageDelivered, NodeContext, NodeDefinition, NodeEvaluateEnd, NodeEvaluateStart,
    NodeMounted, NodeUnmounted, PermissionManifest, QueueDepths, RemoteMessage, Scheduler,
    SchedulerTickEnd, SchedulerTickStart, SupervisorDecisionMade, TelemetrySink,
    TopologyEventOccurred,
};

/// Test sink that captures every `on_call_reply` event for
/// assertions. Other event methods are silent no-ops.
#[derive(Default)]
struct RecorderSink {
    replies: Mutex<Vec<(String, serde_json::Value, String, String)>>,
    deliveries: Mutex<Vec<(String, String, bool)>>,
}

impl TelemetrySink for RecorderSink {
    fn on_call_reply(&self, ev: &CallReply<'_>) {
        self.replies.lock().unwrap().push((
            ev.correlation_id.to_string(),
            ev.payload.clone(),
            ev.from.to_string(),
            ev.reply_type.to_string(),
        ));
    }

    fn on_message_delivered(&self, ev: &MessageDelivered<'_>) {
        self.deliveries.lock().unwrap().push((
            ev.from.to_string(),
            ev.message_type.to_string(),
            ev.accepted,
        ));
    }

    fn on_scheduler_tick_start(&self, _: &SchedulerTickStart) {}
    fn on_scheduler_tick_end(&self, _: &SchedulerTickEnd) {}
    fn on_queue_depths(&self, _: &QueueDepths) {}
    fn on_node_evaluate_start(&self, _: &NodeEvaluateStart<'_>) {}
    fn on_node_evaluate_end(&self, _: &NodeEvaluateEnd<'_>) {}
    fn on_node_mounted(&self, _: &NodeMounted<'_>) {}
    fn on_node_unmounted(&self, _: &NodeUnmounted<'_>) {}
    fn on_topology_event(&self, _: &TopologyEventOccurred<'_>) {}
    fn on_supervisor_decision(&self, _: &SupervisorDecisionMade<'_>) {}
    fn on_remote_message(&self, _: &RemoteMessage<'_>) {}
    fn on_hot_reload(&self, _: &HotReload<'_>) {}
}

#[test]
fn auto_propagation_tags_first_matching_reply_emit() {
    let recorder = Arc::new(RecorderSink::default());

    // Component: declares accepts_external_call; emits "reply" on
    // inbound "query"; emits a NON-reply "log" message before the
    // reply so we can verify only the FIRST emit matching reply_type
    // gets tagged (NOT the "log").
    let aggregator = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_message("query", move |msg| {
            // Two emits: first one is NOT the reply_type — it
            // should NOT be tagged. Second one IS the reply_type
            // — gets the correlation_id.
            emit(
                "log",
                serde_json::json!({"q": msg.payload.clone()}),
                EmitOptions::default(),
            );
            emit(
                "reply",
                serde_json::json!({"answer": 42}),
                EmitOptions::default(),
            );
        });
        let mut m = PermissionManifest::new(vec!["query"], vec!["log", "reply"]);
        m.accepts_external_call.push(ExternalCall {
            request_type: "query".into(),
            reply_type: "reply".into(),
        });
        m
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("aggregator", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Aggregator", aggregator),
            "aggregator",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.install_telemetry_sink(Arc::clone(&recorder) as Arc<dyn TelemetrySink>);
    scheduler.start().unwrap();

    // Inject a call-shape message via the ingress entrypoint with a
    // correlation_id.
    scheduler.handle().send_ingress_with_correlation(
        "aggregator",
        "query",
        serde_json::json!({"q": 1}),
        "call-abc-123",
    );
    scheduler.process_batch().expect("process_batch");

    // Exactly ONE on_call_reply event fired, carrying the reply
    // payload (not the "log" payload).
    let replies = recorder.replies.lock().unwrap();
    assert_eq!(replies.len(), 1, "exactly one on_call_reply event");
    let (cid, payload, from, reply_type) = &replies[0];
    assert_eq!(cid, "call-abc-123");
    assert_eq!(payload, &serde_json::json!({"answer": 42}));
    assert_eq!(from, "aggregator");
    assert_eq!(reply_type, "reply");
}

#[test]
fn manifest_default_deny_for_call_request_without_entry() {
    let received: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
    let received_for_handler = Rc::clone(&received);

    // No accepts_external_call entry — call must be dropped at
    // delivery time even when the inbound carries a correlation_id.
    let aggregator = move |_ctx: &NodeContext| {
        let counter = Rc::clone(&received_for_handler);
        use_message("query", move |_msg| {
            *counter.borrow_mut() += 1;
        });
        // accepts: ['query'] is for node-to-node; accepts_external
        // is empty + accepts_external_call is empty → ingress
        // call shape must be rejected.
        PermissionManifest::new(vec!["query"], Vec::<String>::new())
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("aggregator", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Aggregator", aggregator),
            "aggregator",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    scheduler.handle().send_ingress_with_correlation(
        "aggregator",
        "query",
        serde_json::json!({}),
        "call-xyz-789",
    );
    scheduler.process_batch().expect("process_batch");

    // The inbound correlation-shape message hit the manifest gate
    // and was DROPPED. The handler never ran.
    assert_eq!(*received.borrow(), 0);
}

#[test]
fn nested_inbound_calls_do_not_cross_contaminate_correlation() {
    // Two distinct inbound calls (different correlation_ids)
    // delivered in the same `process_batch` round must each tag
    // their own reply, NOT each other's.
    let recorder = Arc::new(RecorderSink::default());

    let aggregator = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_message("query", move |msg| {
            // Echo the inbound payload as the reply payload so we
            // can identify which call's reply this is.
            emit("reply", msg.payload.clone(), EmitOptions::default());
        });
        let mut m = PermissionManifest::new(vec!["query"], vec!["reply"]);
        m.accepts_external_call.push(ExternalCall {
            request_type: "query".into(),
            reply_type: "reply".into(),
        });
        m
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("aggregator", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Aggregator", aggregator),
            "aggregator",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.install_telemetry_sink(Arc::clone(&recorder) as Arc<dyn TelemetrySink>);
    scheduler.start().unwrap();

    // Inject TWO distinct calls.
    scheduler.handle().send_ingress_with_correlation(
        "aggregator",
        "query",
        serde_json::json!({"i": 1}),
        "cid-1",
    );
    scheduler.handle().send_ingress_with_correlation(
        "aggregator",
        "query",
        serde_json::json!({"i": 2}),
        "cid-2",
    );
    scheduler.process_batch().expect("process_batch");

    let replies = recorder.replies.lock().unwrap();
    assert_eq!(replies.len(), 2);
    // Verify each correlation_id paired with its own payload
    // (not the other's).
    let mut pairs: Vec<(String, u64)> = replies
        .iter()
        .map(|(cid, payload, _from, _rt)| {
            let i = payload["i"].as_u64().unwrap();
            (cid.clone(), i)
        })
        .collect();
    pairs.sort();
    assert_eq!(pairs, vec![("cid-1".into(), 1), ("cid-2".into(), 2)]);
}
