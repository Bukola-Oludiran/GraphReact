//! Channel policies + edge transforms — ported from
//! `graphreact/src/__tests__/integration/channel-policies.test.ts`.
//!
//! Covers per-neighbor accepts overrides, wildcard `*` default channel,
//! and the built-in `multiply-weight` / `threshold-gate` edge transforms
//! resolved via the `transform` edge attribute.

use graphreact_core::{
    use_effect, use_emit, use_message, Attrs, ChannelPolicy, CommunicationGraph, EmitOptions,
    NodeContext, NodeDefinition, PermissionManifest, Scheduler,
};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::rc::Rc;

fn attrs(value: serde_json::Value) -> Attrs {
    match value {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => panic!("attrs!() requires a JSON object"),
    }
}

fn manifest_with_channels(
    accepts: Vec<&str>,
    emits: Vec<&str>,
    channels: Vec<(&str, ChannelPolicy)>,
) -> PermissionManifest {
    let channel_map: IndexMap<String, ChannelPolicy> = channels
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    PermissionManifest {
        accepts: accepts.into_iter().map(String::from).collect(),
        emits: emits.into_iter().map(String::from).collect(),
        channels: channel_map,
        accepts_external: Vec::new(),
        accepts_external_call: Vec::new(),
        needs_secret: None,
    }
}

#[test]
fn per_neighbor_accepts_filters_by_sender() {
    let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let sender_factory = |id: &'static str| -> NodeDefinition {
        NodeDefinition::new("Sender", move |_ctx: &NodeContext| {
            let emit = use_emit();
            use_effect(
                move || {
                    emit(
                        "data",
                        serde_json::json!({ "from": id }),
                        EmitOptions::default(),
                    );
                    None
                },
                vec![],
            );
            PermissionManifest::new(Vec::<String>::new(), ["data"])
        })
    };

    let target = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("data", move |msg| {
                received.borrow_mut().push(msg.from.clone());
            });
            // Accept "data" only from `a`; reject from `b`.
            manifest_with_channels(
                vec!["data"],
                vec![],
                vec![
                    (
                        "a",
                        ChannelPolicy {
                            accepts: Some(vec!["data".into()]),
                            ..Default::default()
                        },
                    ),
                    (
                        "b",
                        ChannelPolicy {
                            accepts: Some(vec![]),
                            ..Default::default()
                        },
                    ),
                ],
            )
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("a", Attrs::new());
    graph.add_node("b", Attrs::new());
    graph.add_node("target", Attrs::new());
    graph.add_edge("a", "target", None, Attrs::new());
    graph.add_edge("b", "target", None, Attrs::new());

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(sender_factory("a"), "a", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(sender_factory("b"), "b", Attrs::new(), 0)
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

    assert_eq!(*received.borrow(), vec!["a".to_string()]);
}

#[test]
fn wildcard_channel_accepts_from_any_unlisted_sender() {
    let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let sender_factory = || -> NodeDefinition {
        NodeDefinition::new("Sender", move |_ctx: &NodeContext| {
            let emit = use_emit();
            use_effect(
                move || {
                    emit("ping", serde_json::json!({}), EmitOptions::default());
                    None
                },
                vec![],
            );
            PermissionManifest::new(Vec::<String>::new(), ["ping"])
        })
    };

    let target = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("ping", move |msg| {
                received.borrow_mut().push(msg.from.clone());
            });
            // Global `accepts` is empty; the wildcard channel accepts ping.
            manifest_with_channels(
                vec![],
                vec![],
                vec![(
                    "*",
                    ChannelPolicy {
                        accepts: Some(vec!["ping".into()]),
                        ..Default::default()
                    },
                )],
            )
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("s1", Attrs::new());
    graph.add_node("s2", Attrs::new());
    graph.add_node("target", Attrs::new());
    graph.add_edge("s1", "target", None, Attrs::new());
    graph.add_edge("s2", "target", None, Attrs::new());

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(sender_factory(), "s1", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(sender_factory(), "s2", Attrs::new(), 0)
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

    let mut got: Vec<String> = received.borrow().clone();
    got.sort();
    assert_eq!(got, vec!["s1".to_string(), "s2".to_string()]);
}

#[test]
fn channel_policy_overrides_global_accepts() {
    let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let sender = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_effect(
            move || {
                emit("allowed", serde_json::json!({}), EmitOptions::default());
                emit("blocked", serde_json::json!({}), EmitOptions::default());
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["allowed", "blocked"])
    };

    let target = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received_a = Rc::clone(&received);
            use_message("allowed", move |_msg| {
                received_a.borrow_mut().push("allowed".into());
            });
            let received_b = Rc::clone(&received);
            use_message("blocked", move |_msg| {
                received_b.borrow_mut().push("blocked".into());
            });
            manifest_with_channels(
                vec!["allowed", "blocked"], // Global accepts both.
                vec![],
                vec![(
                    "sender",
                    ChannelPolicy {
                        accepts: Some(vec!["allowed".into()]), // But channel only allows 'allowed'.
                        ..Default::default()
                    },
                )],
            )
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("sender", Attrs::new());
    graph.add_node("target", Attrs::new());
    graph.add_edge("sender", "target", None, Attrs::new());

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

    assert_eq!(*received.borrow(), vec!["allowed".to_string()]);
}

#[test]
fn multiply_weight_transform_scales_payload_value_in_transit() {
    let received: Rc<RefCell<Vec<f64>>> = Rc::new(RefCell::new(Vec::new()));

    let sender = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_effect(
            move || {
                emit(
                    "signal",
                    serde_json::json!({ "value": 10.0 }),
                    EmitOptions::default(),
                );
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["signal"])
    };

    let receiver = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("signal", move |msg| {
                let v = msg
                    .payload
                    .get("value")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0);
                received.borrow_mut().push(v);
            });
            PermissionManifest::new(["signal"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("s", Attrs::new());
    graph.add_node("r", Attrs::new());
    graph.add_edge(
        "s",
        "r",
        Some("0".into()),
        attrs(serde_json::json!({ "weight": 0.5, "transform": "multiply-weight" })),
    );

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("Sender", sender), "s", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Receiver", receiver),
            "r",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    assert_eq!(*received.borrow(), vec![5.0]); // 10 * 0.5
}

#[test]
fn threshold_gate_drops_messages_below_threshold() {
    let received: Rc<RefCell<Vec<f64>>> = Rc::new(RefCell::new(Vec::new()));

    let sender = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_effect(
            move || {
                emit(
                    "signal",
                    serde_json::json!({ "value": 3.0 }),
                    EmitOptions::default(),
                );
                emit(
                    "signal",
                    serde_json::json!({ "value": 8.0 }),
                    EmitOptions::default(),
                );
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["signal"])
    };

    let receiver = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("signal", move |msg| {
                let v = msg
                    .payload
                    .get("value")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0);
                received.borrow_mut().push(v);
            });
            PermissionManifest::new(["signal"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("s", Attrs::new());
    graph.add_node("r", Attrs::new());
    graph.add_edge(
        "s",
        "r",
        Some("0".into()),
        attrs(serde_json::json!({ "weight": 5.0, "transform": "threshold-gate" })),
    );

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("Sender", sender), "s", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Receiver", receiver),
            "r",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    assert_eq!(*received.borrow(), vec![8.0]); // 3.0 dropped (< 5), 8.0 passes
}

#[test]
fn backward_compatible_flat_manifest_still_works() {
    let received: Rc<RefCell<Vec<serde_json::Value>>> = Rc::new(RefCell::new(Vec::new()));

    let sender = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_effect(
            move || {
                emit(
                    "data",
                    serde_json::json!({ "v": 1 }),
                    EmitOptions::default(),
                );
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["data"])
    };

    let receiver = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("data", move |msg| {
                received.borrow_mut().push(msg.payload.clone());
            });
            // Flat manifest, no `channels` map — purely the legacy format.
            PermissionManifest::new(["data"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("s", Attrs::new());
    graph.add_node("r", Attrs::new());
    graph.add_edge("s", "r", None, Attrs::new());

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("Sender", sender), "s", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Receiver", receiver),
            "r",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    assert_eq!(*received.borrow(), vec![serde_json::json!({ "v": 1 })]);
}
