//! Topological sort and topological generations via Kahn's algorithm.
//!
//! These work on an externally-supplied directed adjacency list because
//! [`graphreact_core::CommunicationGraph`] is undirected. The directed view
//! is typically derived from `.grx` parent-child nesting or some other
//! caller-defined direction.

use indexmap::IndexMap;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
use thiserror::Error;

/// Errors raised by topological algorithms.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DagError {
    #[error("graph contains a cycle — sorted {sorted} of {total} nodes")]
    CycleDetected { sorted: usize, total: usize },
}

/// Directed adjacency map. Iteration order matches insertion order.
pub type DirectedAdj = IndexMap<String, Vec<String>>;

/// Build a [`DirectedAdj`] from a slice of `(source, targets)` pairs.
///
/// Convenience for tests and ergonomic API: callers don't need to fight
/// `IndexMap` directly.
pub fn directed_adj_from<I, S>(pairs: I) -> DirectedAdj
where
    I: IntoIterator<Item = (S, Vec<S>)>,
    S: Into<String>,
{
    pairs
        .into_iter()
        .map(|(src, targets)| (src.into(), targets.into_iter().map(Into::into).collect()))
        .collect()
}

/// Compute in-degrees for every node referenced in `adj`, including target
/// nodes that have no outgoing edges.
fn compute_in_degree(
    adj: &DirectedAdj,
) -> (FxHashMap<String, usize>, FxHashMap<String, Vec<String>>) {
    let mut in_degree: FxHashMap<String, usize> = FxHashMap::default();
    let mut owned_adj: FxHashMap<String, Vec<String>> = FxHashMap::default();

    for (src, targets) in adj {
        in_degree.entry(src.clone()).or_insert(0);
        owned_adj
            .entry(src.clone())
            .or_default()
            .extend(targets.iter().cloned());
        for target in targets {
            *in_degree.entry(target.clone()).or_insert(0) += 1;
            owned_adj.entry(target.clone()).or_default();
        }
    }

    (in_degree, owned_adj)
}

/// Kahn's algorithm — single linear ordering. Errors if the graph cycles.
pub fn topological_sort(adj: &DirectedAdj) -> Result<Vec<String>, DagError> {
    let (mut in_degree, owned_adj) = compute_in_degree(adj);
    let total = in_degree.len();

    let mut queue: VecDeque<String> = VecDeque::new();
    // Iterate in `adj` insertion order first to make the result stable for
    // tests, then catch any nodes that only appear as targets.
    for node in adj.keys() {
        if in_degree.get(node).copied() == Some(0) {
            queue.push_back(node.clone());
        }
    }
    for node in owned_adj.keys() {
        if !adj.contains_key(node) && in_degree.get(node).copied() == Some(0) {
            queue.push_back(node.clone());
        }
    }

    let mut result = Vec::with_capacity(total);
    while let Some(node) = queue.pop_front() {
        result.push(node.clone());
        if let Some(targets) = owned_adj.get(&node) {
            for target in targets {
                let entry = in_degree.entry(target.clone()).or_insert(0);
                *entry = entry.saturating_sub(1);
                if *entry == 0 {
                    queue.push_back(target.clone());
                }
            }
        }
    }

    if result.len() != total {
        return Err(DagError::CycleDetected {
            sorted: result.len(),
            total,
        });
    }
    Ok(result)
}

/// Layered ordering: nodes are grouped into generations such that every
/// dependency of a generation is in an earlier generation.
pub fn topological_generations(adj: &DirectedAdj) -> Result<Vec<Vec<String>>, DagError> {
    let (mut in_degree, owned_adj) = compute_in_degree(adj);
    let total = in_degree.len();

    let mut current_gen: Vec<String> = Vec::new();
    for node in adj.keys() {
        if in_degree.get(node).copied() == Some(0) {
            current_gen.push(node.clone());
        }
    }
    for node in owned_adj.keys() {
        if !adj.contains_key(node) && in_degree.get(node).copied() == Some(0) {
            current_gen.push(node.clone());
        }
    }

    let mut generations: Vec<Vec<String>> = Vec::new();
    let mut total_processed = 0usize;
    while !current_gen.is_empty() {
        generations.push(current_gen.clone());
        total_processed += current_gen.len();

        let mut next_gen: Vec<String> = Vec::new();
        for node in &current_gen {
            if let Some(targets) = owned_adj.get(node) {
                for target in targets {
                    let entry = in_degree.entry(target.clone()).or_insert(0);
                    *entry = entry.saturating_sub(1);
                    if *entry == 0 {
                        next_gen.push(target.clone());
                    }
                }
            }
        }
        current_gen = next_gen;
    }

    if total_processed != total {
        return Err(DagError::CycleDetected {
            sorted: total_processed,
            total,
        });
    }
    Ok(generations)
}
