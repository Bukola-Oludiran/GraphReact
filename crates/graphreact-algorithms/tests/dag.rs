//! Behavioral spec for topological sort/generations — ported from
//! `graphreact/src/__tests__/algorithms/dag.test.ts`.

use graphreact_algorithms::{
    directed_adj_from, topological_generations, topological_sort, DagError,
};

// ---- topological_sort ----

#[test]
fn sorts_a_simple_dag() {
    let adj = directed_adj_from([
        ("a", vec!["b", "c"]),
        ("b", vec!["d"]),
        ("c", vec!["d"]),
        ("d", vec![]),
    ]);
    let result = topological_sort(&adj).unwrap();
    let pos = |s: &str| result.iter().position(|n| n == s).unwrap();
    assert!(pos("a") < pos("b"));
    assert!(pos("a") < pos("c"));
    assert!(pos("b") < pos("d"));
    assert!(pos("c") < pos("d"));
}

#[test]
fn topological_sort_throws_on_cycle() {
    let adj = directed_adj_from([("a", vec!["b"]), ("b", vec!["c"]), ("c", vec!["a"])]);
    let err = topological_sort(&adj).unwrap_err();
    assert!(matches!(err, DagError::CycleDetected { .. }));
}

#[test]
fn topological_sort_handles_single_node() {
    let adj = directed_adj_from([("a", Vec::<&str>::new())]);
    assert_eq!(topological_sort(&adj).unwrap(), vec!["a".to_string()]);
}

// ---- topological_generations ----

#[test]
fn topological_generations_groups_nodes_into_layers() {
    let adj = directed_adj_from([
        ("a", vec!["c"]),
        ("b", vec!["c"]),
        ("c", vec!["d"]),
        ("d", vec![]),
    ]);
    let gens = topological_generations(&adj).unwrap();
    assert_eq!(gens.len(), 3);

    let mut layer0 = gens[0].clone();
    layer0.sort();
    assert_eq!(layer0, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(gens[1], vec!["c".to_string()]);
    assert_eq!(gens[2], vec!["d".to_string()]);
}

#[test]
fn topological_generations_throws_on_cycle() {
    let adj = directed_adj_from([("a", vec!["b"]), ("b", vec!["a"])]);
    let err = topological_generations(&adj).unwrap_err();
    assert!(matches!(err, DagError::CycleDetected { .. }));
}
