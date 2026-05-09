//! Edge-query helpers — surface common edge listings as plain `Vec<EdgeInfo>`
//! instead of iterators, since callers typically want the full set.

use graphreact_core::{Attrs, CommunicationGraph, GraphError};

/// A single edge with both endpoints, key, and attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeInfo {
    pub source: String,
    pub target: String,
    pub key: String,
    pub attrs: Attrs,
}

/// Whether `u` and `v` are connected by at least one edge.
pub fn are_connected(graph: &CommunicationGraph, u: &str, v: &str) -> bool {
    graph.has_edge(u, v, None)
}

/// All edges in the graph, each undirected edge listed once.
pub fn list_edges(graph: &CommunicationGraph) -> Vec<EdgeInfo> {
    graph
        .edges()
        .map(|(u, v, key)| {
            let attrs = graph
                .get_edge_data(&u, &v, Some(&key))
                .expect("edge exists during iteration")
                .clone();
            EdgeInfo {
                source: u,
                target: v,
                key,
                attrs,
            }
        })
        .collect()
}

/// All edges between two specific nodes (including parallel edges).
///
/// Returns an empty vec if the nodes aren't connected.
pub fn list_edges_between(graph: &CommunicationGraph, u: &str, v: &str) -> Vec<EdgeInfo> {
    if !graph.has_edge(u, v, None) {
        return Vec::new();
    }
    graph
        .edge_keys(u, v)
        .into_iter()
        .map(|key| {
            let attrs = graph
                .get_edge_data(u, v, Some(&key))
                .expect("edge key exists")
                .clone();
            EdgeInfo {
                source: u.to_string(),
                target: v.to_string(),
                key,
                attrs,
            }
        })
        .collect()
}

/// All edges incident to a specific node. Errors if the node is missing.
pub fn list_edges_of(
    graph: &CommunicationGraph,
    node_id: &str,
) -> Result<Vec<EdgeInfo>, GraphError> {
    let edges = graph.edges_of(node_id)?;
    Ok(edges
        .into_iter()
        .map(|(u, v, key)| {
            let attrs = graph
                .get_edge_data(&u, &v, Some(&key))
                .expect("edge exists during iteration")
                .clone();
            EdgeInfo {
                source: u,
                target: v,
                key,
                attrs,
            }
        })
        .collect())
}

/// Distinct connected pairs in canonical `(min, max)` order. Self-loops appear
/// once as `(node, node)`.
pub fn connected_pairs(graph: &CommunicationGraph) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut pairs = Vec::new();
    for (u, v, _) in graph.edges() {
        let canonical = if u <= v {
            (u.clone(), v.clone())
        } else {
            (v.clone(), u.clone())
        };
        if seen.insert(canonical.clone()) {
            pairs.push(canonical);
        }
    }
    pairs
}
