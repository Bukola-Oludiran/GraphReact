//! Behavioral spec for `CommunicationGraph` — ported from
//! `graphreact/src/__tests__/graph/graph.test.ts`. Tests are grouped to
//! mirror the `describe` blocks in the original.

use graphreact_core::{Attrs, CommunicationGraph, GraphError, TopologyEvent};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;

/// Helper: build [`Attrs`] from a JSON object literal.
fn attrs(value: serde_json::Value) -> Attrs {
    match value {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => panic!("attrs!() requires a JSON object"),
    }
}

// ---- node operations ----

#[test]
fn adds_and_queries_nodes() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", attrs(json!({ "label": "Node A" })));
    g.add_node("b", Attrs::new());

    assert!(g.has_node("a"));
    assert!(g.has_node("b"));
    assert!(!g.has_node("c"));
    assert_eq!(g.order(), 2);
}

#[test]
fn merges_attributes_on_duplicate_add_node() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", attrs(json!({ "x": 1 })));
    g.add_node("a", attrs(json!({ "y": 2 })));

    let data = g.get_node_data("a").unwrap();
    assert_eq!(data.get("x"), Some(&json!(1)));
    assert_eq!(data.get("y"), Some(&json!(2)));
}

#[test]
fn removes_a_node_and_its_edges() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("a", "c", None, Attrs::new());
    g.remove_node("a").unwrap();

    assert!(!g.has_node("a"));
    assert!(!g.has_edge("a", "b", None));
    assert!(g.neighbors("b").unwrap().is_empty());
}

#[test]
fn throws_on_removing_nonexistent_node() {
    let mut g = CommunicationGraph::new();
    let err = g.remove_node("x").unwrap_err();
    assert_eq!(err, GraphError::NodeMissing { id: "x".into() });
}

#[test]
fn iterates_nodes() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", Attrs::new());
    g.add_node("b", Attrs::new());
    g.add_node("c", Attrs::new());
    let ids: Vec<&str> = g.nodes().collect();
    assert_eq!(ids, vec!["a", "b", "c"]);
}

// ---- edge operations ----

#[test]
fn adds_edges_and_auto_creates_nodes() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());

    assert!(g.has_node("a"));
    assert!(g.has_node("b"));
    assert!(g.has_edge("a", "b", None));
    assert!(g.has_edge("b", "a", None));
}

#[test]
fn supports_multi_edges_with_auto_keys() {
    let mut g = CommunicationGraph::new();
    let k1 = g.add_edge("a", "b", None, attrs(json!({ "channel": "data" })));
    let k2 = g.add_edge("a", "b", None, attrs(json!({ "channel": "control" })));

    assert_eq!(k1, "0");
    assert_eq!(k2, "1");
    assert_eq!(g.edge_keys("a", "b"), vec!["0", "1"]);
    assert_eq!(
        g.get_edge_data("a", "b", Some("0")).unwrap().get("channel"),
        Some(&json!("data"))
    );
    assert_eq!(
        g.get_edge_data("a", "b", Some("1")).unwrap().get("channel"),
        Some(&json!("control"))
    );
}

#[test]
fn supports_named_edge_keys() {
    let mut g = CommunicationGraph::new();
    g.add_edge(
        "a",
        "b",
        Some("data-channel".into()),
        attrs(json!({ "weight": 5 })),
    );
    g.add_edge(
        "a",
        "b",
        Some("control-channel".into()),
        attrs(json!({ "weight": 1 })),
    );

    assert!(g.has_edge("a", "b", Some("data-channel")));
    assert!(g.has_edge("a", "b", Some("control-channel")));
    assert!(!g.has_edge("a", "b", Some("nonexistent")));
}

#[test]
fn updates_edge_attrs_with_same_key() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("k".into()), attrs(json!({ "weight": 1 })));
    g.add_edge("a", "b", Some("k".into()), attrs(json!({ "weight": 5 })));

    assert_eq!(
        g.get_edge_data("a", "b", Some("k")).unwrap().get("weight"),
        Some(&json!(5))
    );
}

#[test]
fn shares_edge_data_bidirectionally() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("0".into()), attrs(json!({ "x": 1 })));

    // Both directions resolve to the same canonical edge — equal contents,
    // and the underlying memory is the same (canonical storage).
    let from_a = g.get_edge_data("a", "b", Some("0")).unwrap();
    let from_b = g.get_edge_data("b", "a", Some("0")).unwrap();
    assert_eq!(from_a, from_b);
    // The references point to the same allocation.
    assert!(std::ptr::eq(from_a, from_b));
}

#[test]
fn removes_a_specific_edge_key() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("0".into()), Attrs::new());
    g.add_edge("a", "b", Some("1".into()), Attrs::new());
    g.remove_edge("a", "b", Some("0")).unwrap();

    assert!(!g.has_edge("a", "b", Some("0")));
    assert!(g.has_edge("a", "b", Some("1")));
    assert!(g.has_edge("a", "b", None));
}

#[test]
fn removes_all_edges_between_a_pair() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("0".into()), Attrs::new());
    g.add_edge("a", "b", Some("1".into()), Attrs::new());
    g.remove_edge("a", "b", None).unwrap();

    assert!(!g.has_edge("a", "b", None));
    assert!(!g.has_edge("b", "a", None));
}

#[test]
fn handles_self_loops() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "a", Some("0".into()), attrs(json!({ "type": "self" })));

    assert!(g.has_edge("a", "a", None));
    assert_eq!(g.neighbors("a").unwrap(), vec!["a".to_string()]);
    assert_eq!(
        g.get_edge_data("a", "a", Some("0")).unwrap().get("type"),
        Some(&json!("self"))
    );
}

// ---- neighbors & degree ----

#[test]
fn returns_neighbors() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("a", "c", None, Attrs::new());

    let mut nbrs = g.neighbors("a").unwrap();
    nbrs.sort();
    assert_eq!(nbrs, vec!["b".to_string(), "c".to_string()]);
}

#[test]
fn degree_counts_multi_edges() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("a", "c", None, Attrs::new());

    assert_eq!(g.degree("a").unwrap(), 3);
}

// ---- iteration & size ----

#[test]
fn counts_edges_correctly() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("b", "c", None, Attrs::new());

    assert_eq!(g.size(), 3);
}

#[test]
fn iterates_edges_without_duplicates() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("0".into()), Attrs::new());
    g.add_edge("b", "c", Some("0".into()), Attrs::new());

    let edges: Vec<_> = g.edges().collect();
    assert_eq!(edges.len(), 2);
}

#[test]
fn iterates_edges_of_a_specific_node() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("0".into()), Attrs::new());
    g.add_edge("a", "c", Some("0".into()), Attrs::new());
    g.add_edge("b", "c", Some("0".into()), Attrs::new());

    let a_edges = g.edges_of("a").unwrap();
    assert_eq!(a_edges.len(), 2);
    assert!(a_edges.iter().all(|(u, _, _)| u == "a"));
}

// ---- edge mutation ----

#[test]
fn update_edge_merges_attributes_on_existing_edge() {
    let mut g = CommunicationGraph::new();
    g.add_edge(
        "a",
        "b",
        Some("k".into()),
        attrs(json!({ "weight": 1, "label": "original" })),
    );
    g.update_edge("a", "b", Some("k"), attrs(json!({ "weight": 5 })))
        .unwrap();

    let data = g.get_edge_data("a", "b", Some("k")).unwrap();
    assert_eq!(data.get("weight"), Some(&json!(5)));
    assert_eq!(data.get("label"), Some(&json!("original")));
}

#[test]
fn update_edge_with_no_key_updates_first_edge() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, attrs(json!({ "weight": 1 })));
    g.update_edge("a", "b", None, attrs(json!({ "weight": 10 })))
        .unwrap();

    assert_eq!(
        g.get_edge_data("a", "b", None).unwrap().get("weight"),
        Some(&json!(10))
    );
}

#[test]
fn update_edge_throws_if_edge_does_not_exist() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", Attrs::new());
    g.add_node("b", Attrs::new());
    let err = g
        .update_edge("a", "b", None, attrs(json!({ "x": 1 })))
        .unwrap_err();
    assert_eq!(
        err,
        GraphError::EdgeMissing {
            u: "a".into(),
            v: "b".into()
        }
    );
}

#[test]
fn update_edge_throws_if_key_does_not_exist() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("k1".into()), Attrs::new());
    let err = g
        .update_edge("a", "b", Some("k2"), attrs(json!({ "x": 1 })))
        .unwrap_err();
    assert_eq!(
        err,
        GraphError::EdgeKeyMissing {
            u: "a".into(),
            v: "b".into(),
            key: "k2".into()
        }
    );
}

#[test]
fn update_edge_does_not_auto_create_nodes() {
    let mut g = CommunicationGraph::new();
    let _ = g.update_edge("x", "y", None, attrs(json!({ "x": 1 })));
    assert!(!g.has_node("x"));
}

#[test]
fn update_edge_visible_bidirectionally() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("k".into()), attrs(json!({ "weight": 1 })));
    g.update_edge("a", "b", Some("k"), attrs(json!({ "weight": 99 })))
        .unwrap();

    assert_eq!(
        g.get_edge_data("b", "a", Some("k")).unwrap().get("weight"),
        Some(&json!(99))
    );
}

#[test]
fn set_edge_data_replaces_all_attributes() {
    let mut g = CommunicationGraph::new();
    g.add_edge(
        "a",
        "b",
        Some("k".into()),
        attrs(json!({ "weight": 1, "label": "old", "extra": true })),
    );
    g.set_edge_data("a", "b", Some("k"), attrs(json!({ "weight": 5 })))
        .unwrap();

    let data = g.get_edge_data("a", "b", Some("k")).unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data.get("weight"), Some(&json!(5)));
}

#[test]
fn set_edge_data_with_no_key_replaces_first_edge() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, attrs(json!({ "old": true })));
    g.set_edge_data("a", "b", None, attrs(json!({ "new": true })))
        .unwrap();

    let data = g.get_edge_data("a", "b", None).unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data.get("new"), Some(&json!(true)));
}

#[test]
fn set_edge_data_throws_if_edge_does_not_exist() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", Attrs::new());
    let err = g.set_edge_data("a", "b", None, Attrs::new()).unwrap_err();
    assert!(matches!(err, GraphError::EdgeMissing { .. }));
}

#[test]
fn set_edge_data_visible_bidirectionally() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("k".into()), attrs(json!({ "old": true })));
    g.set_edge_data("a", "b", Some("k"), attrs(json!({ "replaced": true })))
        .unwrap();

    let data = g.get_edge_data("b", "a", Some("k")).unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data.get("replaced"), Some(&json!(true)));
}

#[test]
fn set_node_data_replaces_all_node_attributes() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", attrs(json!({ "x": 1, "y": 2 })));
    g.set_node_data("a", attrs(json!({ "z": 3 }))).unwrap();

    let data = g.get_node_data("a").unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data.get("z"), Some(&json!(3)));
}

#[test]
fn set_node_data_throws_if_node_does_not_exist() {
    let mut g = CommunicationGraph::new();
    let err = g.set_node_data("x", Attrs::new()).unwrap_err();
    assert_eq!(err, GraphError::NodeMissing { id: "x".into() });
}

// ---- topology change listeners ----

/// Build a thread-safe event collector for use inside listeners.
fn collector() -> (
    Arc<Mutex<Vec<TopologyEvent>>>,
    impl Fn(&TopologyEvent) + Send + Sync + 'static,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let listener = move |e: &TopologyEvent| {
        captured.lock().push(e.clone());
    };
    (events, listener)
}

#[test]
fn fires_node_added_on_add_node() {
    let mut g = CommunicationGraph::new();
    let (events, listener) = collector();
    let _sub = g.on_topology_change(listener);

    g.add_node("a", Attrs::new());
    let captured = events.lock();
    assert_eq!(
        *captured,
        vec![TopologyEvent::NodeAdded {
            node_id: "a".into()
        }]
    );
}

#[test]
fn does_not_fire_node_added_on_add_node_merge() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", attrs(json!({ "x": 1 })));
    let (events, listener) = collector();
    let _sub = g.on_topology_change(listener);

    g.add_node("a", attrs(json!({ "y": 2 })));
    let captured = events.lock();
    assert!(captured.is_empty());
}

#[test]
fn fires_node_removed_and_edge_removed_on_remove_node() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("k1".into()), Attrs::new());
    let (events, listener) = collector();
    let _sub = g.on_topology_change(listener);

    g.remove_node("a").unwrap();
    let captured = events.lock();
    assert_eq!(
        *captured,
        vec![
            TopologyEvent::EdgeRemoved {
                u: "a".into(),
                v: "b".into(),
                key: "k1".into()
            },
            TopologyEvent::NodeRemoved {
                node_id: "a".into()
            },
        ]
    );
}

#[test]
fn fires_edge_added_on_add_edge() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", Attrs::new());
    g.add_node("b", Attrs::new());
    let (events, listener) = collector();
    let _sub = g.on_topology_change(listener);

    g.add_edge("a", "b", Some("k1".into()), attrs(json!({ "weight": 1 })));
    let captured = events.lock();
    assert_eq!(
        *captured,
        vec![TopologyEvent::EdgeAdded {
            u: "a".into(),
            v: "b".into(),
            key: "k1".into()
        }]
    );
}

#[test]
fn fires_node_added_for_auto_created_nodes_in_add_edge() {
    let mut g = CommunicationGraph::new();
    let (events, listener) = collector();
    let _sub = g.on_topology_change(listener);

    g.add_edge("x", "y", Some("k".into()), Attrs::new());
    let captured = events.lock();
    assert!(captured.contains(&TopologyEvent::NodeAdded {
        node_id: "x".into()
    }));
    assert!(captured.contains(&TopologyEvent::NodeAdded {
        node_id: "y".into()
    }));
    assert!(captured.contains(&TopologyEvent::EdgeAdded {
        u: "x".into(),
        v: "y".into(),
        key: "k".into()
    }));
}

#[test]
fn fires_edge_updated_when_add_edge_merges_existing_key() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("k".into()), attrs(json!({ "weight": 1 })));
    let (events, listener) = collector();
    let _sub = g.on_topology_change(listener);

    g.add_edge("a", "b", Some("k".into()), attrs(json!({ "weight": 5 })));
    let captured = events.lock();
    assert_eq!(
        *captured,
        vec![TopologyEvent::EdgeUpdated {
            u: "a".into(),
            v: "b".into(),
            key: "k".into()
        }]
    );
}

#[test]
fn fires_edge_removed_on_remove_edge_with_key() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("k1".into()), Attrs::new());
    let (events, listener) = collector();
    let _sub = g.on_topology_change(listener);

    g.remove_edge("a", "b", Some("k1")).unwrap();
    let captured = events.lock();
    assert_eq!(
        *captured,
        vec![TopologyEvent::EdgeRemoved {
            u: "a".into(),
            v: "b".into(),
            key: "k1".into()
        }]
    );
}

#[test]
fn fires_edge_removed_for_each_key_when_remove_edge_removes_all() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("k1".into()), Attrs::new());
    g.add_edge("a", "b", Some("k2".into()), Attrs::new());
    let (events, listener) = collector();
    let _sub = g.on_topology_change(listener);

    g.remove_edge("a", "b", None).unwrap();
    let captured = events.lock();
    assert_eq!(
        *captured,
        vec![
            TopologyEvent::EdgeRemoved {
                u: "a".into(),
                v: "b".into(),
                key: "k1".into()
            },
            TopologyEvent::EdgeRemoved {
                u: "a".into(),
                v: "b".into(),
                key: "k2".into()
            },
        ]
    );
}

#[test]
fn fires_edge_updated_on_update_edge() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("k".into()), attrs(json!({ "weight": 1 })));
    let (events, listener) = collector();
    let _sub = g.on_topology_change(listener);

    g.update_edge("a", "b", Some("k"), attrs(json!({ "weight": 5 })))
        .unwrap();
    let captured = events.lock();
    assert_eq!(
        *captured,
        vec![TopologyEvent::EdgeUpdated {
            u: "a".into(),
            v: "b".into(),
            key: "k".into()
        }]
    );
}

#[test]
fn fires_edge_updated_on_set_edge_data() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("k".into()), attrs(json!({ "old": true })));
    let (events, listener) = collector();
    let _sub = g.on_topology_change(listener);

    g.set_edge_data("a", "b", Some("k"), attrs(json!({ "new": true })))
        .unwrap();
    let captured = events.lock();
    assert_eq!(
        *captured,
        vec![TopologyEvent::EdgeUpdated {
            u: "a".into(),
            v: "b".into(),
            key: "k".into()
        }]
    );
}

#[test]
fn unsubscribe_stops_events() {
    let mut g = CommunicationGraph::new();
    let (events, listener) = collector();
    let sub = g.on_topology_change(listener);

    g.add_node("a", Attrs::new());
    assert_eq!(events.lock().len(), 1);

    sub.unsubscribe();
    g.add_node("b", Attrs::new());
    assert_eq!(events.lock().len(), 1);
}

#[test]
fn no_events_when_no_listeners_registered() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", Attrs::new());
    g.add_edge("a", "b", None, Attrs::new());
    g.remove_edge("a", "b", None).unwrap();
    g.remove_node("a").unwrap();
    // Nothing to assert — just shouldn't panic.
}

// ---- copy ----

#[test]
fn copy_creates_independent_copy() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("0".into()), attrs(json!({ "weight": 1 })));

    let mut g2 = g.copy();
    g2.add_node("c", Attrs::new());
    g2.get_edge_data_mut("a", "b", Some("0"))
        .unwrap()
        .insert("weight".into(), json!(99));

    assert!(!g.has_node("c"));
    assert_eq!(
        g.get_edge_data("a", "b", Some("0")).unwrap().get("weight"),
        Some(&json!(1))
    );
}
