//! Messaging integration — ported from
//! `graphreact/src/__tests__/integration/messaging.test.ts`.
//!
//! Covers the basic emit→deliver round-trip, edge filtering, TTL cycle
//! protection, multi-parent fan-in, multi-edge channels, external-send
//! injection, and `scheduler.stop()` cleanup.

use graphreact_core::{
    use_effect, use_emit, use_message, use_state, Attrs, CommunicationGraph, EmitOptions,
    NodeContext, NodeDefinition, PermissionManifest, Scheduler, Signal,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

fn attrs(value: serde_json::Value) -> Attrs {
    match value {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => panic!("attrs!() requires a JSON object"),
    }
}

#[test]
fn sender_to_receiver_round_trip() {
    let received: Rc<RefCell<Vec<serde_json::Value>>> = Rc::new(RefCell::new(Vec::new()));

    let sender = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_effect(
            move || {
                emit(
                    "ping",
                    serde_json::json!({ "value": "hello" }),
                    EmitOptions::default(),
                );
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["ping"])
    };

    let receiver = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("ping", move |msg| {
                received.borrow_mut().push(msg.payload.clone());
            });
            PermissionManifest::new(["ping"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("sender", Attrs::new());
    graph.add_node("receiver", Attrs::new());
    graph.add_edge("sender", "receiver", None, Attrs::new());

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
            NodeDefinition::new("Receiver", receiver),
            "receiver",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    let captured = received.borrow();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0], serde_json::json!({ "value": "hello" }));
}

#[test]
fn permission_manifest_rejects_unauthorized_messages() {
    let received: Rc<RefCell<Vec<serde_json::Value>>> = Rc::new(RefCell::new(Vec::new()));

    let sender = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_effect(
            move || {
                emit(
                    "secret",
                    serde_json::json!({ "data": "classified" }),
                    EmitOptions::default(),
                );
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["secret"])
    };

    let receiver = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("secret", move |msg| {
                received.borrow_mut().push(msg.payload.clone());
            });
            // Manifest does NOT include 'secret' in accepts.
            PermissionManifest::new(["ping"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("sender", Attrs::new());
    graph.add_node("receiver", Attrs::new());
    graph.add_edge("sender", "receiver", None, Attrs::new());

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
            NodeDefinition::new("Receiver", receiver),
            "receiver",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    assert!(received.borrow().is_empty(), "secret was rejected");
}

#[test]
fn edge_filter_blocks_unallowed_message_types() {
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

    let receiver = {
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
            PermissionManifest::new(["allowed", "blocked"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("sender", Attrs::new());
    graph.add_node("receiver", Attrs::new());
    // Edge filter only allows the 'allowed' message type.
    graph.add_edge(
        "sender",
        "receiver",
        Some("0".into()),
        attrs(serde_json::json!({ "filter": ["allowed"] })),
    );

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
            NodeDefinition::new("Receiver", receiver),
            "receiver",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    assert_eq!(*received.borrow(), vec!["allowed".to_string()]);
}

#[test]
fn state_updates_propagate_across_nodes() {
    let logged: Rc<RefCell<Vec<i64>>> = Rc::new(RefCell::new(Vec::new()));

    let counter = move |_ctx: &NodeContext| {
        let count: Signal<i64> = use_state(0);
        let emit = use_emit();
        use_message("increment", move |_msg| {
            count.update(|c| c + 1);
        });
        let value = count.get();
        use_effect(
            move || {
                emit(
                    "count-updated",
                    serde_json::json!({ "value": value }),
                    EmitOptions::default(),
                );
                None
            },
            vec![serde_json::json!(value)],
        );
        PermissionManifest::new(["increment"], ["count-updated"])
    };

    let logger = {
        let logged = Rc::clone(&logged);
        move |_ctx: &NodeContext| {
            let logged = Rc::clone(&logged);
            use_message("count-updated", move |msg| {
                let v = msg
                    .payload
                    .get("value")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                logged.borrow_mut().push(v);
            });
            PermissionManifest::new(["count-updated"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("counter", Attrs::new());
    graph.add_node("logger", Attrs::new());
    graph.add_edge("counter", "logger", None, Attrs::new());

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Counter", counter),
            "counter",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Logger", logger),
            "logger",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    // On mount the counter emits count-updated(0).
    assert_eq!(*logged.borrow(), vec![0]);

    // External increment.
    scheduler.send("counter", "increment", serde_json::Value::Null);
    scheduler.process_batch().unwrap();
    assert_eq!(*logged.borrow(), vec![0, 1]);
}

#[test]
fn ttl_prevents_infinite_message_loops() {
    let ping_count = Rc::new(Cell::new(0u32));
    let pong_count = Rc::new(Cell::new(0u32));

    let ping_node = {
        let ping_count = Rc::clone(&ping_count);
        let pong_count = Rc::clone(&pong_count);
        move |_ctx: &NodeContext| {
            let _ = ping_count.clone();
            let pong_count = Rc::clone(&pong_count);
            let emit = use_emit();
            let emit_for_pong = emit.clone();
            use_message("pong", move |_msg| {
                pong_count.set(pong_count.get() + 1);
                emit_for_pong("ping", serde_json::json!({}), EmitOptions::default());
            });
            use_effect(
                move || {
                    emit("ping", serde_json::json!({}), EmitOptions::default());
                    None
                },
                vec![],
            );
            PermissionManifest::new(["pong"], ["ping"])
        }
    };

    let pong_node = {
        let ping_count = Rc::clone(&ping_count);
        move |_ctx: &NodeContext| {
            let ping_count = Rc::clone(&ping_count);
            let emit = use_emit();
            use_message("ping", move |_msg| {
                ping_count.set(ping_count.get() + 1);
                emit("pong", serde_json::json!({}), EmitOptions::default());
            });
            PermissionManifest::new(["ping"], ["pong"])
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("ping", Attrs::new());
    graph.add_node("pong", Attrs::new());
    graph.add_edge("ping", "pong", None, Attrs::new());

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("PingNode", ping_node),
            "ping",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("PongNode", pong_node),
            "pong",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    // The default TTL of 10 bounds the ping/pong exchange.
    assert!(ping_count.get() > 0);
    assert!(ping_count.get() < 50, "TTL bounded the loop");
}

#[test]
fn multi_parent_node_receives_from_each_parent() {
    let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let parent_factory = |id: &'static str| -> NodeDefinition {
        NodeDefinition::new("Parent", move |_ctx: &NodeContext| {
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

    let child = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("data", move |msg| {
                received.borrow_mut().push(msg.from.clone());
            });
            PermissionManifest::new(["data"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("parent-a", Attrs::new());
    graph.add_node("parent-b", Attrs::new());
    graph.add_node("child", Attrs::new());
    graph.add_edge("parent-a", "child", None, Attrs::new());
    graph.add_edge("parent-b", "child", None, Attrs::new());

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(parent_factory("parent-a"), "parent-a", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(parent_factory("parent-b"), "parent-b", Attrs::new(), 0)
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Child", child),
            "child",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    let mut got: Vec<String> = received.borrow().clone();
    got.sort();
    assert_eq!(got, vec!["parent-a".to_string(), "parent-b".to_string()]);
}

#[test]
fn multi_edge_channels_each_filter_their_own_types() {
    let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let sensor = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_effect(
            move || {
                emit(
                    "data",
                    serde_json::json!({ "reading": 42 }),
                    EmitOptions::default(),
                );
                emit(
                    "control",
                    serde_json::json!({ "cmd": "calibrate" }),
                    EmitOptions::default(),
                );
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["data", "control"])
    };

    let processor = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received_d = Rc::clone(&received);
            use_message("data", move |_msg| {
                received_d.borrow_mut().push("data".into());
            });
            let received_c = Rc::clone(&received);
            use_message("control", move |_msg| {
                received_c.borrow_mut().push("control".into());
            });
            PermissionManifest::new(["data", "control"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("sensor", Attrs::new());
    graph.add_node("processor", Attrs::new());
    // Two parallel edges, each with its own filter.
    graph.add_edge(
        "sensor",
        "processor",
        Some("data".into()),
        attrs(serde_json::json!({ "filter": ["data"] })),
    );
    graph.add_edge(
        "sensor",
        "processor",
        Some("control".into()),
        attrs(serde_json::json!({ "filter": ["control"] })),
    );

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Sensor", sensor),
            "sensor",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Processor", processor),
            "processor",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    let mut got: Vec<String> = received.borrow().clone();
    got.sort();
    assert_eq!(got, vec!["control".to_string(), "data".to_string()]);
}

#[test]
fn external_send_injects_messages_into_running_graph() {
    let received: Rc<RefCell<Vec<serde_json::Value>>> = Rc::new(RefCell::new(Vec::new()));

    let component = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("external-cmd", move |msg| {
                received.borrow_mut().push(msg.payload.clone());
            });
            PermissionManifest::new(["external-cmd"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("n", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();
    scheduler.send(
        "n",
        "external-cmd",
        serde_json::json!({ "action": "do-something" }),
    );
    scheduler.process_batch().unwrap();
    assert_eq!(
        *received.borrow(),
        vec![serde_json::json!({ "action": "do-something" })]
    );
}

#[test]
fn stop_cleans_up_effects() {
    let cleaned_up = Rc::new(Cell::new(false));

    let component = {
        let cleaned_up = Rc::clone(&cleaned_up);
        move |_ctx: &NodeContext| {
            let cleaned_up = Rc::clone(&cleaned_up);
            use_effect(
                move || {
                    let cu = Rc::clone(&cleaned_up);
                    Some(Box::new(move || {
                        cu.set(true);
                    }))
                },
                vec![],
            );
            PermissionManifest::default()
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("n", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();
    assert!(!cleaned_up.get());

    scheduler.stop();
    assert!(cleaned_up.get());
}
