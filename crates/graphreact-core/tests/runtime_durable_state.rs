use graphreact_core::{
    use_durable_state, use_emit, use_message, Attrs, CommunicationGraph, EmitOptions, NodeContext,
    NodeDefinition, PermissionManifest, Scheduler,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn counter_definition() -> NodeDefinition {
    NodeDefinition::new("Counter", |_ctx: &NodeContext| {
        let count = use_durable_state("count", serde_json::json!(0));
        use_message("increment", move |_| {
            count.update(|value| serde_json::json!(value.as_u64().unwrap_or_default() + 1));
        });
        PermissionManifest::new(vec!["increment"], Vec::<String>::new())
    })
}

fn counter_scheduler() -> Scheduler {
    let mut graph = CommunicationGraph::new();
    graph.add_node("counter", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(counter_definition(), "counter", Attrs::new(), 0)
        .unwrap();
    scheduler
}

#[test]
fn named_json_state_survives_snapshot_and_restore_before_mount() {
    let mut first = counter_scheduler();
    first.start().unwrap();
    first.send("counter", "increment", serde_json::Value::Null);
    first.send("counter", "increment", serde_json::Value::Null);
    first.process_batch().unwrap();

    let snapshot = first.snapshot_durable_state();
    assert_eq!(snapshot.nodes["counter"]["count"], serde_json::json!(2));

    let mut recovered = counter_scheduler();
    recovered.restore_durable_state(snapshot).unwrap();
    recovered.start().unwrap();
    assert_eq!(
        recovered.snapshot_durable_state().nodes["counter"]["count"],
        serde_json::json!(2)
    );

    recovered.send("counter", "increment", serde_json::Value::Null);
    recovered.process_batch().unwrap();
    assert_eq!(
        recovered.snapshot_durable_state().nodes["counter"]["count"],
        serde_json::json!(3)
    );
}

#[test]
fn duplicate_durable_keys_fail_the_evaluation() {
    let definition = NodeDefinition::new("Invalid", |_ctx: &NodeContext| {
        let _first = use_durable_state("same", serde_json::Value::Null);
        let _duplicate = use_durable_state("same", serde_json::Value::Null);
        PermissionManifest::default()
    });
    let mut graph = CommunicationGraph::new();
    graph.add_node("invalid", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(definition, "invalid", Attrs::new(), 0)
        .unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scheduler.start()));
    let panic = result.expect_err("duplicate durable key must fail before completion");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(message.contains("duplicate durable state key `same`"));
}

#[test]
fn restoring_unknown_node_fails_closed() {
    let mut snapshot = graphreact_core::DurableStateSnapshot::default();
    snapshot.nodes.insert(
        "missing".into(),
        [("key".into(), serde_json::json!(true))]
            .into_iter()
            .collect(),
    );
    let scheduler = Scheduler::default();
    let error = scheduler.restore_durable_state(snapshot).unwrap_err();
    assert!(error.to_string().contains("missing"));
}

#[test]
fn durability_is_opt_in() {
    assert_eq!(
        graphreact_core::DurabilityMode::default(),
        graphreact_core::DurabilityMode::Off
    );
}

#[test]
fn durable_run_context_propagates_through_local_emits() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let sender = NodeDefinition::new("Sender", |_ctx: &NodeContext| {
        let emit = use_emit();
        use_message("start", move |_| {
            emit(
                "work",
                serde_json::Value::Null,
                EmitOptions {
                    direct: Some("receiver".into()),
                    ..EmitOptions::default()
                },
            );
        });
        PermissionManifest::new(vec!["start"], vec!["work"])
    });
    let receiver_observed = Rc::clone(&observed);
    let receiver = NodeDefinition::new("Receiver", move |_ctx: &NodeContext| {
        let receiver_observed = Rc::clone(&receiver_observed);
        use_message("work", move |message| {
            receiver_observed
                .borrow_mut()
                .push((message.run_id.clone(), message.attempt_id.clone()));
        });
        PermissionManifest::new(vec!["work"], Vec::<String>::new())
    });

    let mut graph = CommunicationGraph::new();
    graph.add_node("sender", Attrs::new());
    graph.add_node("receiver", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(sender, "sender", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(receiver, "receiver", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();
    scheduler.send_durable(
        "sender",
        "start",
        serde_json::Value::Null,
        "run-1",
        "attempt-1",
    );
    scheduler.process_batch().unwrap();

    assert_eq!(
        observed.borrow().as_slice(),
        &[(Some("run-1".into()), Some("attempt-1".into()))]
    );
}

#[test]
fn durable_run_emit_does_not_cross_a_remote_peer() {
    let routed = Arc::new(AtomicUsize::new(0));
    let definition = NodeDefinition::new("Sender", |_ctx: &NodeContext| {
        let emit = use_emit();
        use_message("start", move |_| {
            emit(
                "work",
                serde_json::Value::Null,
                EmitOptions {
                    direct: Some("worker@peer-b".into()),
                    ..EmitOptions::default()
                },
            );
        });
        PermissionManifest::new(vec!["start"], vec!["work"])
    });
    let mut graph = CommunicationGraph::new();
    graph.add_node("sender", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(definition, "sender", Attrs::new(), 0)
        .unwrap();
    scheduler.set_node_name("peer-a".into());
    let routed_for_sink = Arc::clone(&routed);
    scheduler.install_remote_sink(Arc::new(move |_| {
        routed_for_sink.fetch_add(1, Ordering::SeqCst);
        true
    }));
    scheduler.start().unwrap();

    scheduler.send_durable(
        "sender",
        "start",
        serde_json::Value::Null,
        "run-1",
        "attempt-1",
    );
    scheduler.process_batch().unwrap();
    assert_eq!(routed.load(Ordering::SeqCst), 0);

    scheduler.send("sender", "start", serde_json::Value::Null);
    scheduler.process_batch().unwrap();
    assert_eq!(routed.load(Ordering::SeqCst), 1);
}
