//! BFS and DFS over an undirected [`CommunicationGraph`].
//!
//! Returns owned `Vec`s (not iterators) — Rust iterators that close over the
//! graph need lifetime annotations that propagate awkwardly through the
//! crate boundary, and the upper bound for these traversals is O(V+E)
//! anyway. If callers need streaming behavior they can break work into
//! chunks via `bfs_edges_with_callback` (M3.5 if needed).

use graphreact_core::{CommunicationGraph, GraphError};
use rustc_hash::FxHashSet;

/// DFS stack frame — kept at module level (clippy `items_after_statements`).
struct DfsFrame {
    parent: String,
    depth: usize,
    neighbors: Vec<String>,
    index: usize,
}

/// BFS edges from `source`. Returns `(parent, child)` pairs in BFS order.
///
/// `depth_limit` of `None` means unlimited; `Some(0)` returns no edges.
pub fn bfs_edges(
    graph: &CommunicationGraph,
    source: &str,
    depth_limit: Option<usize>,
) -> Result<Vec<(String, String)>, GraphError> {
    if !graph.has_node(source) {
        return Err(GraphError::NodeMissing {
            id: source.to_string(),
        });
    }

    let mut visited: FxHashSet<String> = FxHashSet::default();
    visited.insert(source.to_string());

    let mut out = Vec::new();
    // Each frontier entry: (node, depth).
    let mut frontier: Vec<(String, usize)> = vec![(source.to_string(), 0)];

    while !frontier.is_empty() {
        let mut next_frontier: Vec<(String, usize)> = Vec::new();
        for (parent, depth) in &frontier {
            if let Some(limit) = depth_limit {
                if *depth >= limit {
                    continue;
                }
            }
            let neighbors = graph.neighbors(parent)?;
            for child in neighbors {
                if !visited.contains(&child) {
                    visited.insert(child.clone());
                    out.push((parent.clone(), child.clone()));
                    next_frontier.push((child, depth + 1));
                }
            }
        }
        frontier = next_frontier;
    }

    Ok(out)
}

/// BFS nodes from `source`, in BFS order (source first).
pub fn bfs_nodes(
    graph: &CommunicationGraph,
    source: &str,
    depth_limit: Option<usize>,
) -> Result<Vec<String>, GraphError> {
    let edges = bfs_edges(graph, source, depth_limit)?;
    let mut out = Vec::with_capacity(edges.len() + 1);
    out.push(source.to_string());
    for (_, child) in edges {
        out.push(child);
    }
    Ok(out)
}

/// DFS edges from `source` using an iterative stack. Returns `(parent, child)`
/// pairs in DFS order.
pub fn dfs_edges(
    graph: &CommunicationGraph,
    source: &str,
    depth_limit: Option<usize>,
) -> Result<Vec<(String, String)>, GraphError> {
    if !graph.has_node(source) {
        return Err(GraphError::NodeMissing {
            id: source.to_string(),
        });
    }

    let mut visited: FxHashSet<String> = FxHashSet::default();
    visited.insert(source.to_string());

    let mut out = Vec::new();
    let mut stack: Vec<DfsFrame> = vec![DfsFrame {
        parent: source.to_string(),
        depth: 0,
        neighbors: graph.neighbors(source)?,
        index: 0,
    }];

    while let Some(top) = stack.last_mut() {
        if top.index >= top.neighbors.len() {
            stack.pop();
            continue;
        }

        let child = top.neighbors[top.index].clone();
        top.index += 1;

        if !visited.contains(&child) {
            visited.insert(child.clone());
            let parent = top.parent.clone();
            let new_depth = top.depth + 1;
            out.push((parent, child.clone()));

            let push_deeper = depth_limit.is_none_or(|limit| new_depth < limit);
            if push_deeper {
                let neighbors = graph.neighbors(&child)?;
                stack.push(DfsFrame {
                    parent: child,
                    depth: new_depth,
                    neighbors,
                    index: 0,
                });
            }
        }
    }

    Ok(out)
}

/// DFS nodes from `source`, in DFS order (source first).
pub fn dfs_nodes(
    graph: &CommunicationGraph,
    source: &str,
    depth_limit: Option<usize>,
) -> Result<Vec<String>, GraphError> {
    let edges = dfs_edges(graph, source, depth_limit)?;
    let mut out = Vec::with_capacity(edges.len() + 1);
    out.push(source.to_string());
    for (_, child) in edges {
        out.push(child);
    }
    Ok(out)
}
