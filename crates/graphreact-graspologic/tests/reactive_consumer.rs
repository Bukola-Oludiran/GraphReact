//! Integration test demonstrating that spectral embeddings compose with
//! `graphreact-core`'s `Memo<T>` primitive. The plan ("Lands before
//! graspologic ports — algorithms become Memo consumers") motivates this:
//! algorithms wrapped in `Memo<T>` skip recompute when the upstream
//! reactive source hasn't changed.
//!
//! Here we embed a fixed graph in a `Memo<Vec<f64>>` (singular values),
//! tracked by a `ReactiveSignal<u64>` representing a topology fingerprint.
//! Setting the signal to its current value must not trigger recompute.

use std::cell::Cell;
use std::rc::Rc;

use graphreact_core::{Attrs, CommunicationGraph, Memo, ReactiveSignal};
use graphreact_graspologic::{adjacency_spectral_embed, AseOptions};

fn make_graph() -> CommunicationGraph {
    let mut g = CommunicationGraph::new();
    for i in 0..5 {
        g.add_node(format!("n{}", i), Attrs::new());
    }
    for &(u, v) in &[("n0", "n1"), ("n1", "n2"), ("n2", "n3"), ("n3", "n4"), ("n0", "n4")] {
        g.add_edge(u, v, None, Attrs::new());
    }
    g
}

#[test]
fn ase_inside_memo_skips_recompute_on_identical_signal() {
    let graph = Rc::new(make_graph());
    let runs: Rc<Cell<u32>> = Rc::new(Cell::new(0));

    let topology_fingerprint = ReactiveSignal::new(1_u64);

    let runs_for_memo = Rc::clone(&runs);
    let signal_for_memo = topology_fingerprint.clone();
    let graph_for_memo = Rc::clone(&graph);

    let embedding = Memo::new(move || {
        runs_for_memo.set(runs_for_memo.get() + 1);
        let _ = signal_for_memo.get();
        let opts = AseOptions {
            n_components: Some(2),
            diag_aug: false,
            ..Default::default()
        };
        adjacency_spectral_embed(&graph_for_memo, &opts)
            .expect("ase ok")
            .singular_values
    });

    let first = embedding.get();
    assert_eq!(runs.get(), 1);
    assert_eq!(first.len(), 2);

    // Setting the signal to the *same* value must not invalidate the memo.
    topology_fingerprint.set(1);
    let _ = embedding.get();
    assert_eq!(runs.get(), 1, "no recompute on identical signal value");

    // Setting to a different value invalidates and recomputes — but the
    // graph is the same, so the singular values match exactly.
    topology_fingerprint.set(2);
    let second = embedding.get();
    assert_eq!(runs.get(), 2);
    for (a, b) in first.iter().zip(second.iter()) {
        assert!((a - b).abs() < 1e-12);
    }
}
