//! Behavioral spec for connected components — ported from
//! `graphreact/src/__tests__/algorithms/components.test.ts`.

use graphreact_algorithms::{connected_components, is_connected, number_of_connected_components};
use graphreact_core::{Attrs, CommunicationGraph};

#[test]
fn finds_one_component_in_connected_graph() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("b", "c", None, Attrs::new());

    let comps = connected_components(&g).unwrap();
    assert_eq!(comps.len(), 1);
    let expected: rustc_hash::FxHashSet<String> =
        ["a", "b", "c"].iter().map(ToString::to_string).collect();
    assert_eq!(comps[0], expected);
}

#[test]
fn finds_multiple_components() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_node("c", Attrs::new()); // isolated
    g.add_edge("d", "e", None, Attrs::new());

    let comps = connected_components(&g).unwrap();
    assert_eq!(comps.len(), 3);
}

#[test]
fn is_connected_true_for_connected_graph() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("b", "c", None, Attrs::new());
    assert!(is_connected(&g).unwrap());
}

#[test]
fn is_connected_false_for_disconnected_graph() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_node("c", Attrs::new());
    assert!(!is_connected(&g).unwrap());
}

#[test]
fn empty_graph_is_connected() {
    let g = CommunicationGraph::new();
    assert!(is_connected(&g).unwrap());
}

#[test]
fn counts_components_correctly() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_node("c", Attrs::new());
    g.add_edge("d", "e", None, Attrs::new());

    assert_eq!(number_of_connected_components(&g).unwrap(), 3);
}
