//! Behavioral spec for shortest_path / all_paths / astar_path — ported from
//! `graphreact/src/__tests__/algorithms/pathfinding.test.ts`.

use graphreact_algorithms::{
    all_paths, astar_path, shortest_path, AStarOptions, AllPathsOptions, EdgeFilter, PathHeuristic,
    PathOptions,
};
use graphreact_core::{Attrs, CommunicationGraph};
use serde_json::json;
use std::sync::Arc;

fn attrs(value: serde_json::Value) -> Attrs {
    match value {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => panic!("attrs!() requires a JSON object"),
    }
}

fn make_linear() -> CommunicationGraph {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("b", "c", None, Attrs::new());
    g.add_edge("c", "d", None, Attrs::new());
    g
}

fn make_diamond() -> CommunicationGraph {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("a", "c", None, Attrs::new());
    g.add_edge("b", "d", None, Attrs::new());
    g.add_edge("c", "d", None, Attrs::new());
    g
}

fn make_grid() -> CommunicationGraph {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("b", "c", None, Attrs::new());
    g.add_edge("a", "d", None, Attrs::new());
    g.add_edge("b", "e", None, Attrs::new());
    g.add_edge("c", "f", None, Attrs::new());
    g.add_edge("d", "e", None, Attrs::new());
    g.add_edge("e", "f", None, Attrs::new());
    g.add_edge("d", "g", None, Attrs::new());
    g.add_edge("e", "h", None, Attrs::new());
    g.add_edge("f", "i", None, Attrs::new());
    g.add_edge("g", "h", None, Attrs::new());
    g.add_edge("h", "i", None, Attrs::new());
    g
}

fn make_encrypted() -> CommunicationGraph {
    let mut g = CommunicationGraph::new();
    g.add_edge(
        "a",
        "b",
        Some("ab".into()),
        attrs(json!({ "encryption": "AES-256" })),
    );
    g.add_edge(
        "b",
        "c",
        Some("bc".into()),
        attrs(json!({ "encryption": "AES-256" })),
    );
    g.add_edge(
        "a",
        "d",
        Some("ad".into()),
        attrs(json!({ "encryption": "none" })),
    );
    g.add_edge(
        "d",
        "c",
        Some("dc".into()),
        attrs(json!({ "encryption": "none" })),
    );
    g
}

fn encrypted_only() -> EdgeFilter {
    Arc::new(|_u, _v, _k, attrs| {
        attrs.get("encryption").and_then(serde_json::Value::as_str) == Some("AES-256")
    })
}

// ---- shortest_path ----

#[test]
fn shortest_path_in_linear_graph() {
    let g = make_linear();
    let path = shortest_path(&g, "a", "d", &PathOptions::default()).unwrap();
    assert_eq!(
        path,
        Some(vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string()
        ])
    );
}

#[test]
fn shortest_path_source_equals_target() {
    let g = make_linear();
    assert_eq!(
        shortest_path(&g, "b", "b", &PathOptions::default()).unwrap(),
        Some(vec!["b".to_string()])
    );
}

#[test]
fn shortest_path_in_diamond() {
    let g = make_diamond();
    let path = shortest_path(&g, "a", "d", &PathOptions::default())
        .unwrap()
        .unwrap();
    assert_eq!(path.len(), 3);
    assert_eq!(path[0], "a");
    assert_eq!(path.last().unwrap(), "d");
}

#[test]
fn shortest_path_returns_none_when_no_path() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_node("c", Attrs::new());

    assert_eq!(
        shortest_path(&g, "a", "c", &PathOptions::default()).unwrap(),
        None
    );
}

#[test]
fn shortest_path_throws_for_nonexistent_source() {
    let g = make_linear();
    assert!(shortest_path(&g, "x", "a", &PathOptions::default()).is_err());
}

#[test]
fn shortest_path_throws_for_nonexistent_target() {
    let g = make_linear();
    assert!(shortest_path(&g, "a", "x", &PathOptions::default()).is_err());
}

#[test]
fn shortest_path_respects_edge_filter() {
    let g = make_encrypted();
    let opts = PathOptions {
        edge_filter: Some(encrypted_only()),
    };
    let path = shortest_path(&g, "a", "c", &opts).unwrap();
    assert_eq!(path, Some(vec!["a".into(), "b".into(), "c".into()]));
}

#[test]
fn shortest_path_returns_none_when_filter_blocks_all() {
    let g = make_encrypted();
    let opts = PathOptions {
        edge_filter: Some(Arc::new(|_, _, _, _| false)),
    };
    assert_eq!(shortest_path(&g, "a", "c", &opts).unwrap(), None);
}

#[test]
fn shortest_path_in_grid() {
    let g = make_grid();
    let path = shortest_path(&g, "a", "i", &PathOptions::default())
        .unwrap()
        .unwrap();
    assert_eq!(path.len(), 5);
    assert_eq!(path[0], "a");
    assert_eq!(path.last().unwrap(), "i");
}

// ---- all_paths ----

#[test]
fn all_paths_in_diamond() {
    let g = make_diamond();
    let paths = all_paths(&g, "a", "d", &AllPathsOptions::default()).unwrap();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&vec!["a".into(), "b".into(), "d".into()]));
    assert!(paths.contains(&vec!["a".into(), "c".into(), "d".into()]));
}

#[test]
fn all_paths_source_equals_target() {
    let g = make_linear();
    assert_eq!(
        all_paths(&g, "a", "a", &AllPathsOptions::default()).unwrap(),
        vec![vec!["a".to_string()]]
    );
}

#[test]
fn all_paths_single_path_in_linear_graph() {
    let g = make_linear();
    let paths = all_paths(&g, "a", "d", &AllPathsOptions::default()).unwrap();
    assert_eq!(
        paths,
        vec![vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string()
        ]]
    );
}

#[test]
fn all_paths_returns_empty_when_no_path() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_node("c", Attrs::new());
    assert!(all_paths(&g, "a", "c", &AllPathsOptions::default())
        .unwrap()
        .is_empty());
}

#[test]
fn all_paths_respects_max_depth() {
    let mut g = make_diamond();
    g.add_edge("a", "e", None, Attrs::new());
    g.add_edge("e", "f", None, Attrs::new());
    g.add_edge("f", "d", None, Attrs::new());

    let opts = AllPathsOptions {
        max_depth: Some(2),
        ..Default::default()
    };
    let paths = all_paths(&g, "a", "d", &opts).unwrap();
    assert!(paths.iter().all(|p| p.len() <= 3));
    assert!(paths.contains(&vec!["a".into(), "b".into(), "d".into()]));
    assert!(paths.contains(&vec!["a".into(), "c".into(), "d".into()]));
}

#[test]
fn all_paths_respects_edge_filter() {
    let g = make_encrypted();
    let opts = AllPathsOptions {
        edge_filter: Some(encrypted_only()),
        ..Default::default()
    };
    let paths = all_paths(&g, "a", "c", &opts).unwrap();
    let expected: Vec<Vec<String>> = vec![vec!["a".into(), "b".into(), "c".into()]];
    assert_eq!(paths, expected);
}

#[test]
fn all_paths_in_grid() {
    let g = make_grid();
    let paths = all_paths(&g, "a", "i", &AllPathsOptions::default()).unwrap();
    assert!(paths.len() > 1);
    for p in &paths {
        assert_eq!(p[0], "a");
        assert_eq!(p.last().unwrap(), "i");
        // simple paths
        let unique: std::collections::HashSet<&String> = p.iter().collect();
        assert_eq!(unique.len(), p.len());
    }
}

#[test]
fn all_paths_throws_for_nonexistent_nodes() {
    let g = make_linear();
    assert!(all_paths(&g, "x", "a", &AllPathsOptions::default()).is_err());
    assert!(all_paths(&g, "a", "x", &AllPathsOptions::default()).is_err());
}

// ---- astar_path ----

#[test]
fn astar_finds_shortest_without_heuristic() {
    let g = make_linear();
    let path = astar_path(&g, "a", "d", &AStarOptions::default(), None).unwrap();
    assert_eq!(
        path,
        Some(vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string()
        ])
    );
}

#[test]
fn astar_source_equals_target() {
    let g = make_linear();
    assert_eq!(
        astar_path(&g, "b", "b", &AStarOptions::default(), None).unwrap(),
        Some(vec!["b".to_string()])
    );
}

#[test]
fn astar_returns_none_when_no_path() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_node("c", Attrs::new());
    assert_eq!(
        astar_path(&g, "a", "c", &AStarOptions::default(), None).unwrap(),
        None
    );
}

#[test]
fn astar_uses_heuristic_to_guide_search() {
    let mut g = make_grid();
    let coords: [(&str, i64, i64); 9] = [
        ("a", 0, 0),
        ("b", 1, 0),
        ("c", 2, 0),
        ("d", 0, 1),
        ("e", 1, 1),
        ("f", 2, 1),
        ("g", 0, 2),
        ("h", 1, 2),
        ("i", 2, 2),
    ];
    for (id, x, y) in coords {
        g.add_node(id, attrs(json!({ "x": x, "y": y })));
    }

    let manhattan: PathHeuristic = Arc::new(|current, target, graph, _directory| {
        let c = graph.get_node_data(current).unwrap();
        let t = graph.get_node_data(target).unwrap();
        let cx = c
            .get("x")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let cy = c
            .get("y")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let tx = t
            .get("x")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        let ty = t
            .get("y")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        (cx - tx).abs() + (cy - ty).abs()
    });

    let opts = AStarOptions {
        heuristic: Some(manhattan),
        ..Default::default()
    };
    let path = astar_path(&g, "a", "i", &opts, None).unwrap().unwrap();
    assert_eq!(path.len(), 5);
    assert_eq!(path[0], "a");
    assert_eq!(path.last().unwrap(), "i");
}

#[test]
fn astar_respects_edge_filter() {
    let g = make_encrypted();
    let opts = AStarOptions {
        edge_filter: Some(encrypted_only()),
        ..Default::default()
    };
    let path = astar_path(&g, "a", "c", &opts, None).unwrap();
    assert_eq!(path, Some(vec!["a".into(), "b".into(), "c".into()]));
}

#[test]
fn astar_heuristic_with_penalty_steers_away() {
    let mut g = CommunicationGraph::new();
    g.add_edge("a", "b", None, Attrs::new());
    g.add_edge("b", "c", None, Attrs::new());
    g.add_edge("a", "d", None, Attrs::new());
    g.add_edge("d", "c", None, Attrs::new());
    g.add_node("b", attrs(json!({ "thirdParty": true })));
    g.add_node("d", attrs(json!({ "thirdParty": false })));

    let avoid_third_party: PathHeuristic = Arc::new(|current, _target, graph, _directory| {
        let data = graph.get_node_data(current).unwrap();
        if data.get("thirdParty").and_then(serde_json::Value::as_bool) == Some(true) {
            10.0
        } else {
            0.0
        }
    });

    let opts = AStarOptions {
        heuristic: Some(avoid_third_party),
        ..Default::default()
    };
    let path = astar_path(&g, "a", "c", &opts, None).unwrap();
    assert_eq!(path, Some(vec!["a".into(), "d".into(), "c".into()]));
}

#[test]
fn astar_throws_for_nonexistent_nodes() {
    let g = make_linear();
    assert!(astar_path(&g, "x", "a", &AStarOptions::default(), None).is_err());
    assert!(astar_path(&g, "a", "x", &AStarOptions::default(), None).is_err());
}

#[test]
fn astar_in_diamond() {
    let g = make_diamond();
    let path = astar_path(&g, "a", "d", &AStarOptions::default(), None)
        .unwrap()
        .unwrap();
    assert_eq!(path.len(), 3);
    assert_eq!(path[0], "a");
    assert_eq!(path.last().unwrap(), "d");
}
