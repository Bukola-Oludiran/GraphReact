//! Direct messaging — `EmitOptions::direct` bypasses channel-policy
//! resolution at delivery (skips per-neighbor channels, edge filters, edge
//! transforms) but the target's flat `manifest.accepts` still applies.
//!
//! Mirrors the TS `_direct` semantics introduced in Phase 9.

use graphreact_core::{
    use_effect, use_emit, use_message, Attrs, ChannelPolicy, CommunicationGraph, EmitOptions,
    NodeContext, NodeDefinition, PermissionManifest, Scheduler,
};
use std::cell::RefCell;
use std::rc::Rc;

#[test]
fn direct_message_bypasses_channel_policy_that_would_reject() {
    // Sender's channel rule on the wire would normally reject 'data' from
    // 'sender' — this test confirms `direct: Some("target")` skips that
    // rule but still delivers because the target's flat accepts allows it.
    let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let sender = |_ctx: &NodeContext| {
        let emit = use_emit();
        use_effect(
            move || {
                emit(
                    "data",
                    serde_json::json!({ "v": 1 }),
                    EmitOptions {
                        direct: Some("target".into()),
                        ..Default::default()
                    },
                );
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["data"])
    };

    let target = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("data", move |msg| {
                received.borrow_mut().push(msg.from.clone());
            });
            // Per-neighbor rule: reject everything from 'sender'.
            // Direct messaging should bypass this.
            let mut channels: indexmap::IndexMap<String, ChannelPolicy> = indexmap::IndexMap::new();
            channels.insert(
                "sender".into(),
                ChannelPolicy {
                    accepts: Some(Vec::<String>::new()),
                    ..Default::default()
                },
            );
            PermissionManifest {
                accepts: vec!["data".into()],
                emits: Vec::new(),
                channels,
                accepts_external: Vec::new(),
                accepts_external_call: Vec::new(),
                needs_secret: None,
            }
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("sender", Attrs::new());
    graph.add_node("target", Attrs::new());
    // No edge between them — direct messaging doesn't need one.
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Sender", sender),
            "sender",
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
    scheduler.process_batch().unwrap();

    assert_eq!(
        *received.borrow(),
        vec!["sender".to_string()],
        "direct-message bypasses the per-neighbor channel rule that would reject"
    );
}

#[test]
fn direct_message_still_rejected_if_target_does_not_accept_type() {
    // Target's flat accepts is empty — direct doesn't grant a capability
    // the target hasn't declared.
    let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let sender = |_ctx: &NodeContext| {
        let emit = use_emit();
        use_effect(
            move || {
                emit(
                    "secret",
                    serde_json::json!({}),
                    EmitOptions {
                        direct: Some("target".into()),
                        ..Default::default()
                    },
                );
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["secret"])
    };

    let target = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("secret", move |msg| {
                received.borrow_mut().push(msg.r#type.clone());
            });
            // Target accepts nothing on the flat list.
            PermissionManifest::new(Vec::<String>::new(), Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("sender", Attrs::new());
    graph.add_node("target", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Sender", sender),
            "sender",
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
    scheduler.process_batch().unwrap();

    assert!(
        received.borrow().is_empty(),
        "direct message rejected when target doesn't accept the type"
    );
}
