//! Behavioral spec for `SubgraphView` — ported from
//! `graphreact/src/__tests__/graph/views.test.ts`.

use graphreact_core::{
    accept_all_nodes, subgraph_by_nodes, subgraph_view, Attrs, CommunicationGraph, EdgeFilter,
};
use rustc_hash::FxHashSet;
use serde_json::json;
use std::sync::Arc;

fn attrs(value: serde_json::Value) -> Attrs {
    match value {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => panic!("attrs!() requires a JSON object"),
    }
}

fn make_graph() -> CommunicationGraph {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", Some("0".into()), attrs(json!({ "weight": 1 })));
    g.add_edge("b", "c", Some("0".into()), attrs(json!({ "weight": 2 })));
    g.add_edge("c", "d", Some("0".into()), attrs(json!({ "weight": 3 })));
    g
}

fn id_set(ids: &[&str]) -> FxHashSet<String> {
    ids.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn filters_nodes_by_id_set() {
    let g = make_graph();
    let view = subgraph_by_nodes(&g, id_set(&["a", "b"]));

    assert!(view.has_node("a"));
    assert!(!view.has_node("c"));
    assert_eq!(view.order(), 2);
}

#[test]
fn filters_edges_only_shows_edges_between_included_nodes() {
    let g = make_graph();
    let view = subgraph_by_nodes(&g, id_set(&["a", "b"]));

    assert!(view.has_edge("a", "b", None));
    assert!(!view.has_edge("b", "c", None)); // c excluded
    assert_eq!(view.size(), 1);
}

#[test]
fn neighbors_only_returns_nodes_in_the_view() {
    let g = make_graph();
    let view = subgraph_by_nodes(&g, id_set(&["a", "b", "c"]));

    let mut nbrs = view.neighbors("b").unwrap();
    nbrs.sort();
    assert_eq!(nbrs, vec!["a".to_string(), "c".to_string()]);

    assert_eq!(view.neighbors("c").unwrap(), vec!["b".to_string()]);
}

#[test]
fn custom_edge_filter() {
    let mut g = CommunicationGraph::new();
    g.add_edge(
        "a",
        "b",
        Some("data".into()),
        attrs(json!({ "channel": "data" })),
    );
    g.add_edge(
        "a",
        "b",
        Some("control".into()),
        attrs(json!({ "channel": "control" })),
    );

    let edge_filter: EdgeFilter = Arc::new(|_u, _v, key| key == "data");
    let view = subgraph_view(&g, Some(accept_all_nodes()), Some(edge_filter));

    assert!(view.has_edge("a", "b", Some("data")));
    assert!(!view.has_edge("a", "b", Some("control")));
    assert_eq!(view.size(), 1);
}

#[test]
fn reflects_changes_in_source_graph() {
    let mut g = make_graph();
    let included = id_set(&["a", "b", "c", "d", "e"]);

    {
        let view = subgraph_by_nodes(&g, included.clone());
        assert!(!view.has_node("e"));
    }
    g.add_node("e", Attrs::new());
    let view = subgraph_by_nodes(&g, included);
    assert!(view.has_node("e"));
}
