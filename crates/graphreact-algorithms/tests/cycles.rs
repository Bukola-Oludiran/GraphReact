//! Behavioral spec for cycle detection — ported from
//! `graphreact/src/__tests__/algorithms/cycles.test.ts`.

use graphreact_algorithms::{find_cycle, has_cycle};
use graphreact_core::{Attrs, CommunicationGraph};

#[test]
fn detects_no_cycle_in_a_tree() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("b", "c", None, Attrs::new());
    g.add_edge("b", "d", None, Attrs::new());

    assert!(find_cycle(&g).unwrap().is_none());
    assert!(!has_cycle(&g).unwrap());
}

#[test]
fn detects_a_triangle_cycle() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("b", "c", None, Attrs::new());
    g.add_edge("c", "a", None, Attrs::new());

    assert!(has_cycle(&g).unwrap());
    let cycle = find_cycle(&g).unwrap().unwrap();
    assert!(cycle.len() >= 2);
}

#[test]
fn detects_cycle_in_a_larger_graph() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("b", "c", None, Attrs::new());
    g.add_edge("c", "d", None, Attrs::new());
    g.add_edge("d", "b", None, Attrs::new()); // cycle b-c-d-b

    assert!(has_cycle(&g).unwrap());
}

#[test]
fn handles_disconnected_graph_with_cycle_in_one_component() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("x", "y", None, Attrs::new());
    g.add_edge("y", "z", None, Attrs::new());
    g.add_edge("z", "x", None, Attrs::new());

    assert!(has_cycle(&g).unwrap());
}

#[test]
fn no_cycle_in_empty_graph() {
    let g = CommunicationGraph::new();
    assert!(!has_cycle(&g).unwrap());
}

#[test]
fn no_cycle_with_single_node() {
    let mut g = CommunicationGraph::new();
    g.add_node("a", Attrs::new());
    assert!(!has_cycle(&g).unwrap());
}
