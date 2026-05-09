//! Connected components of an undirected [`CommunicationGraph`].

use graphreact_core::{CommunicationGraph, GraphError};
use rustc_hash::FxHashSet;

use crate::traversal::bfs_nodes;

/// All connected components of `graph`. Each component is a set of node IDs.
pub fn connected_components(
    graph: &CommunicationGraph,
) -> Result<Vec<FxHashSet<String>>, GraphError> {
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut components = Vec::new();

    for node in graph.nodes() {
        if seen.contains(node) {
            continue;
        }

        let mut component: FxHashSet<String> = FxHashSet::default();
        for n in bfs_nodes(graph, node, None)? {
            seen.insert(n.clone());
            component.insert(n);
        }
        components.push(component);
    }

    Ok(components)
}

/// Whether `graph` is connected: all nodes reachable from any node.
pub fn is_connected(graph: &CommunicationGraph) -> Result<bool, GraphError> {
    if graph.order() == 0 {
        return Ok(true);
    }
    let first = graph.nodes().next().expect("nonzero order").to_string();
    let reached = bfs_nodes(graph, &first, None)?;
    Ok(reached.len() == graph.order())
}

/// Number of connected components in `graph`.
pub fn number_of_connected_components(graph: &CommunicationGraph) -> Result<usize, GraphError> {
    Ok(connected_components(graph)?.len())
}
