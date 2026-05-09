//! `use_topology` event delivery — ported from
//! `graphreact/src/__tests__/runtime/topology.test.ts`.

use graphreact_core::{
    use_topology, Attrs, CommunicationGraph, NodeContext, NodeDefinition, PermissionManifest,
    Scheduler, TopologyEvent, TopologyEventKind, TopologyFilterOptions,
};
use std::cell::RefCell;
use std::rc::Rc;

type EventLog = Rc<RefCell<Vec<TopologyEvent>>>;

fn watcher_definition(events: EventLog, options: Option<TopologyFilterOptions>) -> NodeDefinition {
    NodeDefinition::new("Watcher", move |_ctx: &NodeContext| {
        let events = Rc::clone(&events);
        let options = options.clone();
        use_topology(
            move |e| {
                events.borrow_mut().push(e.clone());
            },
            options.clone(),
        );
        PermissionManifest::default()
    })
}

#[test]
fn use_topology_receives_node_added_from_dynamic_mutation() {
    let events: EventLog = Rc::new(RefCell::new(Vec::new()));

    let mut graph = CommunicationGraph::new();
    graph.add_node("watcher", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            watcher_definition(Rc::clone(&events), None),
            "watcher",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();
    events.borrow_mut().clear();

    scheduler
        .add_node_at_runtime(
            NodeDefinition::new("New", |_| PermissionManifest::default()),
            "dynamic",
            Attrs::new(),
            vec!["watcher".into()],
        )
        .unwrap();

    let added: Vec<_> = events
        .borrow()
        .iter()
        .filter(|e| TopologyEventKind::of(e) == TopologyEventKind::NodeAdded)
        .cloned()
        .collect();
    assert!(
        added.contains(&TopologyEvent::NodeAdded {
            node_id: "dynamic".into()
        }),
        "expected NodeAdded for 'dynamic' in {:?}",
        added
    );
}

#[test]
fn use_topology_receives_node_removed_and_edge_removed_on_remove_at_runtime() {
    let events: EventLog = Rc::new(RefCell::new(Vec::new()));

    let mut graph = CommunicationGraph::new();
    graph.add_node("watcher", Attrs::new());
    graph.add_node("temp", Attrs::new());
    graph.add_edge("watcher", "temp", None, Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            watcher_definition(Rc::clone(&events), None),
            "watcher",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Temp", |_| PermissionManifest::default()),
            "temp",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();
    events.borrow_mut().clear();

    scheduler.remove_node_at_runtime("temp").unwrap();

    let captured = events.borrow();
    assert!(captured
        .iter()
        .any(|e| TopologyEventKind::of(e) == TopologyEventKind::EdgeRemoved));
    assert!(captured.iter().any(|e| matches!(
        e,
        TopologyEvent::NodeRemoved { node_id } if node_id == "temp"
    )));
}

#[test]
fn use_topology_filter_by_event_type() {
    let events: EventLog = Rc::new(RefCell::new(Vec::new()));
    let opts = TopologyFilterOptions {
        kinds: Some(vec![TopologyEventKind::EdgeAdded]),
        neighbors_only: false,
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("watcher", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            watcher_definition(Rc::clone(&events), Some(opts)),
            "watcher",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();
    events.borrow_mut().clear();

    scheduler
        .add_node_at_runtime(
            NodeDefinition::new("New", |_| PermissionManifest::default()),
            "n",
            Attrs::new(),
            vec!["watcher".into()],
        )
        .unwrap();

    let captured = events.borrow();
    assert!(!captured.is_empty());
    assert!(captured
        .iter()
        .all(|e| TopologyEventKind::of(e) == TopologyEventKind::EdgeAdded));
}

#[test]
fn use_topology_neighbors_filter_excludes_far_events() {
    let events: EventLog = Rc::new(RefCell::new(Vec::new()));
    let opts = TopologyFilterOptions {
        kinds: None,
        neighbors_only: true,
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("watcher", Attrs::new());
    graph.add_node("x", Attrs::new());
    graph.add_node("y", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            watcher_definition(Rc::clone(&events), Some(opts)),
            "watcher",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();
    events.borrow_mut().clear();

    // Edge between two nodes neither of which is a neighbor of the watcher.
    scheduler
        .graph
        .borrow_mut()
        .add_edge("x", "y", Some("link".into()), Attrs::new());
    scheduler.process_batch().unwrap();

    assert!(events.borrow().is_empty());
}

#[test]
fn use_topology_neighbors_filter_includes_near_events() {
    let events: EventLog = Rc::new(RefCell::new(Vec::new()));
    let opts = TopologyFilterOptions {
        kinds: None,
        neighbors_only: true,
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("watcher", Attrs::new());
    graph.add_node("a", Attrs::new());
    graph.add_node("b", Attrs::new());
    graph.add_edge("watcher", "a", None, Attrs::new());

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            watcher_definition(Rc::clone(&events), Some(opts)),
            "watcher",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();
    events.borrow_mut().clear();

    // Edge between `a` (a neighbor) and `b` — should be visible.
    scheduler
        .graph
        .borrow_mut()
        .add_edge("a", "b", Some("link".into()), Attrs::new());
    scheduler.process_batch().unwrap();

    let captured = events.borrow();
    assert!(captured.iter().any(|e| matches!(
        e,
        TopologyEvent::EdgeAdded { u, v, key } if u == "a" && v == "b" && key == "link"
    )));
}

#[test]
fn use_topology_unmounted_node_does_not_receive_events() {
    let events: EventLog = Rc::new(RefCell::new(Vec::new()));

    let mut graph = CommunicationGraph::new();
    graph.add_node("watcher", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            watcher_definition(Rc::clone(&events), None),
            "watcher",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();
    events.borrow_mut().clear();

    scheduler.remove_node_at_runtime("watcher").unwrap();
    events.borrow_mut().clear();

    scheduler.graph.borrow_mut().add_node("new-node", Attrs::new());
    scheduler.process_batch().unwrap();
    assert!(events.borrow().is_empty());
}

#[test]
fn use_topology_receives_edge_updated() {
    let events: EventLog = Rc::new(RefCell::new(Vec::new()));

    let mut graph = CommunicationGraph::new();
    graph.add_node("watcher", Attrs::new());
    graph.add_node("peer", Attrs::new());
    graph.add_edge(
        "watcher",
        "peer",
        Some("k".into()),
        [("latency".to_string(), serde_json::json!(10))]
            .into_iter()
            .collect(),
    );

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(
            watcher_definition(Rc::clone(&events), None),
            "watcher",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(
            NodeDefinition::new("Peer", |_| PermissionManifest::default()),
            "peer",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();
    events.borrow_mut().clear();

    let updated_attrs = [("latency".to_string(), serde_json::json!(5))]
        .into_iter()
        .collect();
    scheduler
        .graph
        .borrow_mut()
        .update_edge("watcher", "peer", Some("k"), updated_attrs)
        .unwrap();
    scheduler.process_batch().unwrap();

    let captured = events.borrow();
    assert!(captured.iter().any(|e| matches!(
        e,
        TopologyEvent::EdgeUpdated { u, v, key } if u == "watcher" && v == "peer" && key == "k"
    )));
}

#[test]
fn multiple_nodes_with_use_topology_receive_independently() {
    let events_a: EventLog = Rc::new(RefCell::new(Vec::new()));
    let events_b: EventLog = Rc::new(RefCell::new(Vec::new()));

    let mut graph = CommunicationGraph::new();
    graph.add_node("a", Attrs::new());
    graph.add_node("b", Attrs::new());

    let mut scheduler = Scheduler::new(graph);

    let opts_a = TopologyFilterOptions {
        kinds: Some(vec![TopologyEventKind::NodeAdded]),
        neighbors_only: false,
    };
    let opts_b = TopologyFilterOptions {
        kinds: Some(vec![TopologyEventKind::EdgeAdded]),
        neighbors_only: false,
    };
    scheduler
        .register_node(
            watcher_definition(Rc::clone(&events_a), Some(opts_a)),
            "a",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler
        .register_node(
            watcher_definition(Rc::clone(&events_b), Some(opts_b)),
            "b",
            Attrs::new(),
            0,
        )
        .unwrap();
    scheduler.start().unwrap();
    events_a.borrow_mut().clear();
    events_b.borrow_mut().clear();

    scheduler
        .add_node_at_runtime(
            NodeDefinition::new("New", |_| PermissionManifest::default()),
            "new",
            Attrs::new(),
            vec!["a".into()],
        )
        .unwrap();

    let captured_a = events_a.borrow();
    let captured_b = events_b.borrow();
    assert!(!captured_a.is_empty());
    assert!(captured_a
        .iter()
        .all(|e| TopologyEventKind::of(e) == TopologyEventKind::NodeAdded));
    assert!(!captured_b.is_empty());
    assert!(captured_b
        .iter()
        .all(|e| TopologyEventKind::of(e) == TopologyEventKind::EdgeAdded));
}
