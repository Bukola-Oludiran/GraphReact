//! Behavioral spec for BFS/DFS — ported from
//! `graphreact/src/__tests__/algorithms/traversal.test.ts`.

use graphreact_algorithms::{bfs_edges, bfs_nodes, dfs_edges, dfs_nodes};
use graphreact_core::{Attrs, CommunicationGraph};

fn make_linear() -> CommunicationGraph {
    // a - b - c - d
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("b", "c", None, Attrs::new());
    g.add_edge("c", "d", None, Attrs::new());
    g
}

fn make_star() -> CommunicationGraph {
    let mut g = CommunicationGraph::new();
    g.add_edge("center", "a", None, Attrs::new());
    g.add_edge("center", "b", None, Attrs::new());
    g.add_edge("center", "c", None, Attrs::new());
    g.add_edge("center", "d", None, Attrs::new());
    g
}

// ---- BFS ----

#[test]
fn bfs_edges_traverses_breadth_first() {
    let g = make_linear();
    let edges = bfs_edges(&g, "a", None).unwrap();
    assert_eq!(
        edges,
        vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "c".to_string()),
            ("c".to_string(), "d".to_string()),
        ]
    );
}

#[test]
fn bfs_nodes_yields_in_bfs_order() {
    let g = make_linear();
    let nodes = bfs_nodes(&g, "a", None).unwrap();
    assert_eq!(nodes, vec!["a", "b", "c", "d"]);
}

#[test]
fn bfs_edges_from_star_center_visits_all_spokes() {
    let g = make_star();
    let edges = bfs_edges(&g, "center", None).unwrap();
    assert_eq!(edges.len(), 4);
    assert!(edges.iter().all(|(parent, _)| parent == "center"));
}

#[test]
fn bfs_respects_depth_limit() {
    let g = make_linear();
    let nodes = bfs_nodes(&g, "a", Some(2)).unwrap();
    assert_eq!(nodes, vec!["a", "b", "c"]);
}

#[test]
fn bfs_throws_for_nonexistent_source() {
    let g = CommunicationGraph::new();
    assert!(bfs_edges(&g, "x", None).is_err());
}

// ---- DFS ----

#[test]
fn dfs_edges_traverses_depth_first() {
    let g = make_linear();
    let edges = dfs_edges(&g, "a", None).unwrap();
    assert_eq!(
        edges,
        vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "c".to_string()),
            ("c".to_string(), "d".to_string()),
        ]
    );
}

#[test]
fn dfs_nodes_yields_in_dfs_order() {
    let g = make_linear();
    let nodes = dfs_nodes(&g, "a", None).unwrap();
    assert_eq!(nodes, vec!["a", "b", "c", "d"]);
}

#[test]
fn dfs_respects_depth_limit() {
    let g = make_linear();
    let nodes = dfs_nodes(&g, "a", Some(2)).unwrap();
    assert_eq!(nodes, vec!["a", "b", "c"]);
}

#[test]
fn dfs_handles_disconnected_components() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_node("c", Attrs::new()); // isolated

    let nodes = dfs_nodes(&g, "a", None).unwrap();
    assert_eq!(nodes, vec!["a", "b"]);
    assert!(!nodes.contains(&"c".to_string()));
}
