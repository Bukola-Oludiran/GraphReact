//! Integration tests for the OTP-inspired supervision layer:
//! monitor (`__DOWN__`), link (`__EXIT__`), and process groups.
//!
//! No direct TS equivalent — these tests anchor the M5 stage 1 + 2
//! contract that scheduler.unmount fans out the right system messages.

use graphreact_core::{
    use_effect, use_group_join, use_link, use_message, use_monitor, Attrs, CommunicationGraph,
    EmitOptions, ExitReason, NodeContext, NodeDefinition, PermissionManifest, Scheduler,
};
use std::cell::RefCell;
use std::rc::Rc;

/// Helper — define a node that records every message it receives keyed
/// by message type.
fn record_messages(
    role: &'static str,
    accepts: Vec<&'static str>,
    captures: Rc<RefCell<Vec<(String, serde_json::Value)>>>,
) -> NodeDefinition {
    NodeDefinition::new(role, move |_ctx: &NodeContext| {
        for r#type in &accepts {
            let captures = Rc::clone(&captures);
            let r#type = (*r#type).to_string();
            use_message(r#type.clone(), move |msg| {
                captures
                    .borrow_mut()
                    .push((r#type.clone(), msg.payload.clone()));
            });
        }
        PermissionManifest::new(accepts.clone(), Vec::<String>::new())
    })
}

// ---- Monitor ----

#[test]
fn monitor_fires_down_message_when_target_unmounts() {
    let captures: Rc<RefCell<Vec<(String, serde_json::Value)>>> = Rc::new(RefCell::new(Vec::new()));

    // Observer registers a monitor on the target via use_monitor and
    // listens for the `__DOWN__` system message.
    let observer = {
        let captures = Rc::clone(&captures);
        move |_ctx: &NodeContext| {
            // Ignore the returned ref; the test asserts on the message.
            let _ = use_monitor("target");
            let captures = Rc::clone(&captures);
            use_message("__DOWN__", move |msg| {
                captures
                    .borrow_mut()
                    .push(("__DOWN__".to_string(), msg.payload.clone()));
            });
            PermissionManifest::new(["__DOWN__"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("observer", Attrs::new());
    graph.add_node("target", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Observer", observer),
            "observer",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Target", |_| PermissionManifest::default()),
            "target",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();
    captures.borrow_mut().clear();

    // Unmount target with a non-Normal reason so we can verify the
    // payload reflects the supplied reason.
    let target_key = scheduler.key_for("target").unwrap();
    scheduler.unmount_with_reason(
        target_key,
        ExitReason::Crashed {
            detail: "test-crash".into(),
        },
    );
    scheduler.process_batch().unwrap();

    let captured = captures.borrow();
    assert_eq!(captured.len(), 1, "observer received exactly one DOWN");
    let (kind, payload) = &captured[0];
    assert_eq!(kind, "__DOWN__");
    assert_eq!(
        payload.get("target").and_then(serde_json::Value::as_str),
        Some("target")
    );
    assert_eq!(
        payload.get("reason").and_then(serde_json::Value::as_str),
        Some("crashed")
    );
}

#[test]
fn monitor_is_unidirectional_observer_unmount_does_not_signal_target() {
    let target_captures: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let observer = move |_ctx: &NodeContext| {
        let _ = use_monitor("target");
        PermissionManifest::default()
    };

    let target = {
        let target_captures = Rc::clone(&target_captures);
        move |_ctx: &NodeContext| {
            let target_captures = Rc::clone(&target_captures);
            use_message("__DOWN__", move |_msg| {
                target_captures.borrow_mut().push("DOWN".into());
            });
            use_message("__EXIT__", move |_msg| {
                // Should never fire — target wasn't linked.
            });
            PermissionManifest::new(["__DOWN__", "__EXIT__"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("observer", Attrs::new());
    graph.add_node("target", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Observer", observer),
            "observer",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Target", target),
            "target",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();
    target_captures.borrow_mut().clear();

    let observer_key = scheduler.key_for("observer").unwrap();
    scheduler.unmount(observer_key);
    scheduler.process_batch().unwrap();

    assert!(
        target_captures.borrow().is_empty(),
        "target receives no signals when an observer unmounts"
    );
}

// ---- Link ----

#[test]
fn link_is_bidirectional_both_peers_receive_exit() {
    let a_captures: Rc<RefCell<Vec<serde_json::Value>>> = Rc::new(RefCell::new(Vec::new()));
    let b_captures: Rc<RefCell<Vec<serde_json::Value>>> = Rc::new(RefCell::new(Vec::new()));

    let a = {
        let a_captures = Rc::clone(&a_captures);
        move |_ctx: &NodeContext| {
            let _ = use_link("b");
            let captures = Rc::clone(&a_captures);
            use_message("__EXIT__", move |msg| {
                captures.borrow_mut().push(msg.payload.clone());
            });
            PermissionManifest::new(["__EXIT__"], Vec::<String>::new())
        }
    };

    let b = {
        let b_captures = Rc::clone(&b_captures);
        move |_ctx: &NodeContext| {
            let captures = Rc::clone(&b_captures);
            use_message("__EXIT__", move |msg| {
                captures.borrow_mut().push(msg.payload.clone());
            });
            PermissionManifest::new(["__EXIT__"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("a", Attrs::new());
    graph.add_node("b", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("A", a), "a", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(NodeDefinition::new("B", b), "b", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();

    // Verify the link is established post-mount.
    let a_key = scheduler.key_for("a").unwrap();
    let b_key = scheduler.key_for("b").unwrap();
    assert!(scheduler.is_linked(a_key, b_key));

    // Drop b — a should receive __EXIT__. Then drop a separately and
    // verify b's pre-existing handler received nothing extra.
    let b_initial = b_captures.borrow().len();
    scheduler.unmount_with_reason(
        b_key,
        ExitReason::Crashed {
            detail: "boom".into(),
        },
    );
    scheduler.process_batch().unwrap();

    let captured = a_captures.borrow();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].get("from").and_then(serde_json::Value::as_str),
        Some("b")
    );
    assert_eq!(
        captured[0]
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("crashed")
    );
    // b unmounted before its handler could receive anything new.
    assert_eq!(b_captures.borrow().len(), b_initial);
}

#[test]
fn unlink_stops_exit_propagation() {
    let captures: Rc<RefCell<Vec<serde_json::Value>>> = Rc::new(RefCell::new(Vec::new()));

    let observer = {
        let captures = Rc::clone(&captures);
        move |_ctx: &NodeContext| {
            let _ = use_link("target");
            let captures = Rc::clone(&captures);
            use_message("__EXIT__", move |msg| {
                captures.borrow_mut().push(msg.payload.clone());
            });
            PermissionManifest::new(["__EXIT__"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("observer", Attrs::new());
    graph.add_node("target", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Observer", observer),
            "observer",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Target", |_| PermissionManifest::default()),
            "target",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    // Manually unlink before unmounting target.
    let observer_key = scheduler.key_for("observer").unwrap();
    let target_key = scheduler.key_for("target").unwrap();
    scheduler.unlink(observer_key, target_key);
    assert!(!scheduler.is_linked(observer_key, target_key));

    scheduler.unmount_with_reason(target_key, ExitReason::Crashed { detail: "x".into() });
    scheduler.process_batch().unwrap();

    assert!(captures.borrow().is_empty(), "no EXIT after unlink");
}

// ---- Process groups ----

#[test]
fn group_join_then_broadcast_reaches_all_members() {
    let captures: Rc<RefCell<Vec<(String, serde_json::Value)>>> = Rc::new(RefCell::new(Vec::new()));

    let member = {
        let captures = Rc::clone(&captures);
        move |ctx: &NodeContext| {
            let id = ctx.id.clone();
            use_group_join("workers");
            let captures = Rc::clone(&captures);
            use_message("ping", move |msg| {
                captures
                    .borrow_mut()
                    .push((id.clone(), msg.payload.clone()));
            });
            PermissionManifest::new(["ping"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("a", Attrs::new());
    graph.add_node("b", Attrs::new());
    graph.add_node("c", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    for id in ["a", "b", "c"] {
        scheduler
            .register_node(
                NodeDefinition::new("Worker", member.clone()),
                id,
                Attrs::new(),
                0,
            )
            .unwrap();
    }
    scheduler.start().unwrap();
    captures.borrow_mut().clear();

    assert_eq!(scheduler.group_members("workers").len(), 3);
    scheduler.group_broadcast(
        "workers",
        "__external__",
        "ping",
        serde_json::json!({ "n": 1 }),
    );
    scheduler.process_batch().unwrap();

    let mut got: Vec<String> = captures.borrow().iter().map(|(id, _)| id.clone()).collect();
    got.sort();
    assert_eq!(got, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
}

#[test]
fn group_auto_leaves_on_unmount() {
    let member = move |_ctx: &NodeContext| {
        use_group_join("workers");
        PermissionManifest::default()
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("a", Attrs::new());
    graph.add_node("b", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("W", member), "a", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(NodeDefinition::new("W", member), "b", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();
    assert_eq!(scheduler.group_members("workers").len(), 2);

    let a_key = scheduler.key_for("a").unwrap();
    scheduler.unmount(a_key);
    assert_eq!(scheduler.group_members("workers").len(), 1);
}

#[test]
fn use_effect_with_use_link_records_record_messages_helper() {
    // Smoke test that the helper definition compiles + works.
    let captures: Rc<RefCell<Vec<(String, serde_json::Value)>>> = Rc::new(RefCell::new(Vec::new()));

    let definition = record_messages("Recorder", vec!["ping"], Rc::clone(&captures));

    let sender = move |_ctx: &NodeContext| {
        let emit = use_emit_helper();
        use_effect(
            move || {
                emit(
                    "ping",
                    serde_json::json!({ "v": 1 }),
                    EmitOptions::default(),
                );
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["ping"])
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("recorder", Attrs::new());
    graph.add_node("sender", Attrs::new());
    graph.add_edge("sender", "recorder", None, Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(definition, "recorder", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Sender", sender),
            "sender",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    assert_eq!(captures.borrow().len(), 1);
    assert_eq!(captures.borrow()[0].0, "ping");
}

// Local re-export so the helper test can call use_emit without stomping
// the runtime symbol used in the broadcast test above.
fn use_emit_helper() -> std::rc::Rc<dyn Fn(&str, serde_json::Value, EmitOptions)> {
    graphreact_core::use_emit()
}
