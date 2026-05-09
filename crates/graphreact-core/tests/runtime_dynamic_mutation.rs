//! Dynamic runtime graph mutation — ported from
//! `graphreact/src/__tests__/runtime/dynamic-mutation.test.ts`.

use graphreact_core::{
    use_effect, use_emit, use_message, Attrs, CommunicationGraph, EmitOptions, NodeContext,
    NodeDefinition, PermissionManifest, Scheduler,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[test]
fn add_node_at_runtime_connects_and_routes_messages() {
    let received: Rc<RefCell<Vec<serde_json::Value>>> = Rc::new(RefCell::new(Vec::new()));

    let sender = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_message("trigger", move |_msg| {
            emit(
                "ping",
                serde_json::json!({ "v": 2 }),
                EmitOptions::default(),
            );
        });
        PermissionManifest::new(["trigger"], ["ping"])
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
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Sender", sender),
            "sender",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    scheduler
        .add_node_at_runtime(
            NodeDefinition::new("Receiver", receiver),
            "receiver",
            Attrs::new(),
            vec!["sender".into()],
        )
        .unwrap();

    scheduler.send("sender", "trigger", serde_json::Value::Null);
    scheduler.process_batch().unwrap();
    assert_eq!(received.borrow().len(), 1);
    assert_eq!(received.borrow()[0], serde_json::json!({ "v": 2 }));
}

#[test]
fn add_node_at_runtime_evaluates_node_immediately() {
    let mounted = Rc::new(Cell::new(false));

    let component = {
        let mounted = Rc::clone(&mounted);
        move |_ctx: &NodeContext| {
            let mounted = Rc::clone(&mounted);
            use_effect(
                move || {
                    mounted.set(true);
                    None
                },
                vec![],
            );
            PermissionManifest::default()
        }
    };

    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();
    assert!(!mounted.get());

    scheduler
        .add_node_at_runtime(
            NodeDefinition::new("N", component),
            "n",
            Attrs::new(),
            vec![],
        )
        .unwrap();
    assert!(mounted.get());
}

#[test]
fn add_node_at_runtime_connects_to_multiple_existing_nodes() {
    let mut graph = CommunicationGraph::new();
    graph.add_node("a", Attrs::new());
    graph.add_node("b", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("A", |_| PermissionManifest::default()),
            "a",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("B", |_| PermissionManifest::default()),
            "b",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    scheduler
        .add_node_at_runtime(
            NodeDefinition::new("New", |_| PermissionManifest::default()),
            "new",
            Attrs::new(),
            vec!["a".into(), "b".into()],
        )
        .unwrap();

    let g = scheduler.graph.borrow();
    assert!(g.has_edge("new", "a", None));
    assert!(g.has_edge("new", "b", None));
    let mut nbrs = g.neighbors("new").unwrap();
    nbrs.sort();
    assert_eq!(nbrs, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn remove_node_at_runtime_runs_cleanup() {
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
    graph.add_node("a", Attrs::new());
    graph.add_node("n", Attrs::new());
    graph.add_edge("a", "n", None, Attrs::new());

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("A", |_| PermissionManifest::default()),
            "a",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();
    assert!(scheduler.graph.borrow().has_node("n"));
    assert!(!cleaned_up.get());

    scheduler.remove_node_at_runtime("n").unwrap();
    assert!(!scheduler.graph.borrow().has_node("n"));
    assert!(cleaned_up.get());
    assert!(!scheduler.has_node("n"));
}

#[test]
fn dynamically_added_node_can_send_messages() {
    let log: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let existing = {
        let log = Rc::clone(&log);
        move |_ctx: &NodeContext| {
            let log = Rc::clone(&log);
            use_message("hello", move |msg| {
                let from = msg
                    .payload
                    .get("from")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                log.borrow_mut().push(format!("existing got: {from}"));
            });
            PermissionManifest::new(["hello"], Vec::<String>::new())
        }
    };

    let dynamic = move |_ctx: &NodeContext| {
        let emit = use_emit();
        use_effect(
            move || {
                emit(
                    "hello",
                    serde_json::json!({ "from": "dynamic" }),
                    EmitOptions::default(),
                );
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["hello"])
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("existing", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            NodeDefinition::new("Existing", existing),
            "existing",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();

    scheduler
        .add_node_at_runtime(
            NodeDefinition::new("Dynamic", dynamic),
            "dynamic",
            Attrs::new(),
            vec!["existing".into()],
        )
        .unwrap();
    assert_eq!(*log.borrow(), vec!["existing got: dynamic".to_string()]);
}

#[test]
fn remove_node_at_runtime_on_nonexistent_is_noop() {
    let mut scheduler = Scheduler::new(CommunicationGraph::new());
    scheduler.start().unwrap();
    scheduler.remove_node_at_runtime("nonexistent").unwrap();
}
