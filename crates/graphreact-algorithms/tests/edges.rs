//! Behavioral spec for edge-query helpers — ported from
//! `graphreact/src/__tests__/algorithms/edges.test.ts`.

use graphreact_algorithms::{
    are_connected, connected_pairs, list_edges, list_edges_between, list_edges_of,
};
use graphreact_core::{Attrs, CommunicationGraph};
use serde_json::json;

fn attrs(value: serde_json::Value) -> Attrs {
    match value {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => panic!("attrs!() requires a JSON object"),
    }
}

fn make_graph() -> CommunicationGraph {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("0".into()), attrs(json!({ "weight": 1 })));
    g.add_edge("a", "b", Some("1".into()), attrs(json!({ "weight": 2 })));
    g.add_edge("b", "c", Some("0".into()), attrs(json!({ "weight": 3 })));
    g.add_node("d", Attrs::new());
    g
}

// ---- are_connected ----

#[test]
fn are_connected_true_for_connected_nodes() {
    let g = make_graph();
    assert!(are_connected(&g, "a", "b"));
    assert!(are_connected(&g, "b", "a"));
}

#[test]
fn are_connected_false_for_unconnected_nodes() {
    let g = make_graph();
    assert!(!are_connected(&g, "a", "c"));
    assert!(!are_connected(&g, "a", "d"));
}

#[test]
fn are_connected_false_for_nonexistent_nodes() {
    let g = make_graph();
    assert!(!are_connected(&g, "a", "z"));
}

// ---- list_edges ----

#[test]
fn list_edges_returns_all_edges() {
    let g = make_graph();
    let edges = list_edges(&g);
    assert_eq!(edges.len(), 3);
}

#[test]
fn list_edges_includes_attrs() {
    let g = make_graph();
    let edges = list_edges(&g);
    let ab0 = edges
        .iter()
        .find(|e| e.key == "0" && e.source == "a")
        .unwrap();
    assert_eq!(ab0.attrs.get("weight"), Some(&json!(1)));
}

#[test]
fn list_edges_returns_empty_for_empty_graph() {
    let g = CommunicationGraph::new();
    assert_eq!(list_edges(&g).len(), 0);
}

// ---- list_edges_between ----

#[test]
fn list_edges_between_returns_multi_edges() {
    let g = make_graph();
    let edges = list_edges_between(&g, "a", "b");
    assert_eq!(edges.len(), 2);
    let mut keys: Vec<_> = edges.iter().map(|e| e.key.clone()).collect();
    keys.sort();
    assert_eq!(keys, vec!["0".to_string(), "1".to_string()]);
}

#[test]
fn list_edges_between_returns_empty_for_unconnected() {
    let g = make_graph();
    assert!(list_edges_between(&g, "a", "d").is_empty());
}

#[test]
fn list_edges_between_works_in_both_directions() {
    let g = make_graph();
    let ab = list_edges_between(&g, "a", "b");
    let ba = list_edges_between(&g, "b", "a");
    assert_eq!(ab.len(), ba.len());
}

// ---- list_edges_of ----

#[test]
fn list_edges_of_returns_all_incident_edges() {
    let g = make_graph();
    let edges = list_edges_of(&g, "b").unwrap();
    assert_eq!(edges.len(), 3); // 2 with a + 1 with c
}

#[test]
fn list_edges_of_returns_empty_for_isolated_node() {
    let g = make_graph();
    assert!(list_edges_of(&g, "d").unwrap().is_empty());
}

// ---- connected_pairs ----

#[test]
fn connected_pairs_returns_unique_pairs() {
    let g = make_graph();
    let pairs = connected_pairs(&g);
    assert_eq!(pairs.len(), 2);
    assert!(pairs.contains(&("a".to_string(), "b".to_string())));
    assert!(pairs.contains(&("b".to_string(), "c".to_string())));
}

#[test]
fn connected_pairs_returns_empty_for_no_edges() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", Attrs::new());
    g.add_node("b", Attrs::new());
    assert!(connected_pairs(&g).is_empty());
}
