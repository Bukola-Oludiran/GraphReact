//! Cycle detection on undirected graphs via DFS.
//!
//! A back-edge to an already-visited node that isn't the immediate parent
//! indicates a cycle.

use graphreact_core::{CommunicationGraph, GraphError};
use rustc_hash::{FxHashMap, FxHashSet};

/// DFS frame for cycle detection — module-level to avoid `items_after_statements`.
struct CycleFrame {
    node: String,
    parent: Option<String>,
    neighbors: Vec<String>,
    index: usize,
}

/// Find a cycle in `graph`. Returns the cycle as a sequence of `(u, v)`
/// edge pairs, or `None` if the graph is acyclic.
///
/// Self-loops do NOT count as cycles for this routine — they pass the
/// `nbr != parent` test only if you have a true loop, which we avoid here
/// to match the TS implementation.
pub fn find_cycle(graph: &CommunicationGraph) -> Result<Option<Vec<(String, String)>>, GraphError> {
    let mut visited: FxHashSet<String> = FxHashSet::default();

    for start in graph.nodes() {
        if visited.contains(start) {
            continue;
        }

        let start_owned = start.to_string();
        visited.insert(start_owned.clone());

        let mut parent_map: FxHashMap<String, Option<String>> = FxHashMap::default();
        parent_map.insert(start_owned.clone(), None);

        let mut stack: Vec<CycleFrame> = vec![CycleFrame {
            node: start_owned.clone(),
            parent: None,
            neighbors: graph.neighbors(&start_owned)?,
            index: 0,
        }];

        while let Some(top) = stack.last_mut() {
            if top.index >= top.neighbors.len() {
                stack.pop();
                continue;
            }
            let nbr = top.neighbors[top.index].clone();
            top.index += 1;

            if !visited.contains(&nbr) {
                visited.insert(nbr.clone());
                parent_map.insert(nbr.clone(), Some(top.node.clone()));
                let neighbors = graph.neighbors(&nbr)?;
                let child_parent = Some(top.node.clone());
                stack.push(CycleFrame {
                    node: nbr,
                    parent: child_parent,
                    neighbors,
                    index: 0,
                });
            } else if Some(&nbr) != top.parent.as_ref() {
                // Found a cycle — reconstruct.
                let mut cycle: Vec<(String, String)> = vec![(top.node.clone(), nbr.clone())];
                let mut current = top.node.clone();
                while current != nbr {
                    let Some(prev) = parent_map.get(&current).cloned().flatten() else {
                        break;
                    };
                    cycle.push((prev.clone(), current.clone()));
                    current = prev;
                }
                cycle.reverse();
                return Ok(Some(cycle));
            }
        }
    }

    Ok(None)
}

/// Whether `graph` contains a cycle.
pub fn has_cycle(graph: &CommunicationGraph) -> Result<bool, GraphError> {
    Ok(find_cycle(graph)?.is_some())
}
