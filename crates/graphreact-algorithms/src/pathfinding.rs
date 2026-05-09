//! Pathfinding algorithms — `shortest_path` (BFS), `all_paths` (DFS), and
//! `astar_path` (A*). All accept an optional [`EdgeFilter`] so callers can
//! restrict traversal to edges that satisfy a predicate.
//!
//! `astar_path` accepts a pluggable [`PathHeuristic`]; without one it
//! degrades to BFS shortest path (uniform edge weight of 1).

use graphreact_core::{Attrs, CommunicationGraph, GraphDirectory, GraphError};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

/// DFS frame used by `all_paths` — module-level to avoid `items_after_statements`.
struct PathFrame {
    neighbors: Vec<String>,
    index: usize,
}

/// Predicate over a single edge — return `true` to allow traversal.
///
/// `Arc<dyn>` so callers can pass owned closures cheaply across the API
/// boundary; this also matches the `NodeFilter` / `EdgeFilter` pattern in
/// [`graphreact_core::graph::views`].
pub type EdgeFilter = Arc<dyn Fn(&str, &str, &str, &Attrs) -> bool + Send + Sync>;

/// A* heuristic — a non-negative estimate of cost from `current` to `target`.
///
/// Must be admissible (never overestimate) for the result to be optimal.
///
/// The optional [`GraphDirectory`] gives heuristics access to structural
/// metadata (node depth, parent path, hierarchy) that isn't carried on
/// the [`CommunicationGraph`] itself — matching the TS reference's
/// four-argument heuristic signature so hierarchical heuristics
/// (depth-aware, ancestor-aware) can be implemented identically across
/// the JS and Rust ports. Pass `None` when the directory hasn't been
/// built (e.g., outside a scheduler context); heuristics that need the
/// directory should treat `None` as "no structural info available."
pub type PathHeuristic = Arc<
    dyn Fn(&str, &str, &CommunicationGraph, Option<&GraphDirectory>) -> f64 + Send + Sync,
>;

/// Options for `shortest_path` and `all_paths`.
#[derive(Default, Clone)]
pub struct PathOptions {
    pub edge_filter: Option<EdgeFilter>,
}

/// Options for `all_paths` — extends [`PathOptions`] with an upper bound on
/// path length so dense graphs don't blow up.
#[derive(Default, Clone)]
pub struct AllPathsOptions {
    pub edge_filter: Option<EdgeFilter>,
    pub max_depth: Option<usize>,
}

/// Options for `astar_path`.
#[derive(Default, Clone)]
pub struct AStarOptions {
    pub edge_filter: Option<EdgeFilter>,
    pub heuristic: Option<PathHeuristic>,
}

/// Filtered neighbors of `node` — only those reachable through at least one
/// edge that passes the filter.
fn filtered_neighbors(
    graph: &CommunicationGraph,
    node: &str,
    edge_filter: Option<&EdgeFilter>,
) -> Result<Vec<String>, GraphError> {
    let neighbors = graph.neighbors(node)?;
    let Some(filter) = edge_filter else {
        return Ok(neighbors);
    };
    let mut out = Vec::with_capacity(neighbors.len());
    for nbr in neighbors {
        let any_pass = graph.edge_keys(node, &nbr).into_iter().any(|key| {
            graph
                .get_edge_data(node, &nbr, Some(&key))
                .is_ok_and(|attrs| filter(node, &nbr, &key, attrs))
        });
        if any_pass {
            out.push(nbr);
        }
    }
    Ok(out)
}

fn ensure_node(graph: &CommunicationGraph, id: &str) -> Result<(), GraphError> {
    if !graph.has_node(id) {
        return Err(GraphError::NodeMissing { id: id.to_string() });
    }
    Ok(())
}

/// BFS shortest path from `source` to `target`.
///
/// Returns the node sequence, or `None` if unreachable. `O(V + E)`.
pub fn shortest_path(
    graph: &CommunicationGraph,
    source: &str,
    target: &str,
    opts: &PathOptions,
) -> Result<Option<Vec<String>>, GraphError> {
    ensure_node(graph, source)?;
    ensure_node(graph, target)?;

    if source == target {
        return Ok(Some(vec![source.to_string()]));
    }

    let mut visited: FxHashSet<String> = FxHashSet::default();
    visited.insert(source.to_string());
    let mut parent: FxHashMap<String, String> = FxHashMap::default();
    let mut frontier: Vec<String> = vec![source.to_string()];

    while !frontier.is_empty() {
        let mut next_frontier: Vec<String> = Vec::new();
        for node in &frontier {
            for nbr in filtered_neighbors(graph, node, opts.edge_filter.as_ref())? {
                if visited.contains(&nbr) {
                    continue;
                }
                visited.insert(nbr.clone());
                parent.insert(nbr.clone(), node.clone());

                if nbr == target {
                    return Ok(Some(reconstruct_path(&parent, source, target)));
                }
                next_frontier.push(nbr);
            }
        }
        frontier = next_frontier;
    }

    Ok(None)
}

fn reconstruct_path(parent: &FxHashMap<String, String>, source: &str, target: &str) -> Vec<String> {
    let mut path: Vec<String> = vec![target.to_string()];
    let mut current = target.to_string();
    while current != source {
        let prev = parent
            .get(&current)
            .expect("parent map must reach source")
            .clone();
        path.push(prev.clone());
        current = prev;
    }
    path.reverse();
    path
}

/// All simple paths from `source` to `target` via iterative DFS.
///
/// `max_depth` bounds the number of edges in any returned path so dense
/// graphs don't combinatorially explode. Each path is a sequence of node
/// IDs.
pub fn all_paths(
    graph: &CommunicationGraph,
    source: &str,
    target: &str,
    opts: &AllPathsOptions,
) -> Result<Vec<Vec<String>>, GraphError> {
    ensure_node(graph, source)?;
    ensure_node(graph, target)?;

    if source == target {
        return Ok(vec![vec![source.to_string()]]);
    }

    let mut results: Vec<Vec<String>> = Vec::new();
    let mut visited: FxHashSet<String> = FxHashSet::default();
    visited.insert(source.to_string());
    let mut path: Vec<String> = vec![source.to_string()];

    let mut stack: Vec<PathFrame> = vec![PathFrame {
        neighbors: filtered_neighbors(graph, source, opts.edge_filter.as_ref())?,
        index: 0,
    }];

    while !stack.is_empty() {
        let top = stack.last_mut().expect("non-empty");
        if top.index >= top.neighbors.len() {
            stack.pop();
            if let Some(removed) = path.pop() {
                visited.remove(&removed);
            }
            continue;
        }

        let nbr = top.neighbors[top.index].clone();
        top.index += 1;

        if nbr == target {
            let mut found = path.clone();
            found.push(target.to_string());
            results.push(found);
            continue;
        }

        if visited.contains(&nbr) {
            continue;
        }
        if let Some(limit) = opts.max_depth {
            if path.len() >= limit {
                continue;
            }
        }

        visited.insert(nbr.clone());
        path.push(nbr.clone());
        let neighbors = filtered_neighbors(graph, &nbr, opts.edge_filter.as_ref())?;
        stack.push(PathFrame {
            neighbors,
            index: 0,
        });
    }

    Ok(results)
}

/// A* search from `source` to `target` with a pluggable heuristic.
///
/// Without a heuristic, this is equivalent to BFS shortest path. Edge weight
/// is uniform (1 hop). Returns the node sequence, or `None` if unreachable.
///
/// `directory` is forwarded to the heuristic on each invocation so
/// hierarchical heuristics (depth-aware, ancestor-aware) can read
/// structural metadata that isn't carried on the
/// [`CommunicationGraph`]. Pass `None` if you don't have a directory
/// built — heuristics that don't need it ignore the argument.
pub fn astar_path(
    graph: &CommunicationGraph,
    source: &str,
    target: &str,
    opts: &AStarOptions,
    directory: Option<&GraphDirectory>,
) -> Result<Option<Vec<String>>, GraphError> {
    ensure_node(graph, source)?;
    ensure_node(graph, target)?;

    if source == target {
        return Ok(Some(vec![source.to_string()]));
    }

    let zero: PathHeuristic = Arc::new(|_, _, _, _| 0.0);
    let heuristic = opts.heuristic.as_ref().unwrap_or(&zero);

    let mut g_score: FxHashMap<String, f64> = FxHashMap::default();
    g_score.insert(source.to_string(), 0.0);

    let mut f_score: FxHashMap<String, f64> = FxHashMap::default();
    f_score.insert(source.to_string(), heuristic(source, target, graph, directory));

    let mut came_from: FxHashMap<String, String> = FxHashMap::default();
    let mut open: FxHashSet<String> = FxHashSet::default();
    open.insert(source.to_string());
    let mut closed: FxHashSet<String> = FxHashSet::default();

    while !open.is_empty() {
        // Pick the node in `open` with the smallest f-score.
        let current = open
            .iter()
            .min_by(|a, b| {
                let fa = f_score.get(*a).copied().unwrap_or(f64::INFINITY);
                let fb = f_score.get(*b).copied().unwrap_or(f64::INFINITY);
                fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned()
            .expect("open set non-empty");

        if current == target {
            return Ok(Some(reconstruct_path(&came_from, source, target)));
        }

        open.remove(&current);
        closed.insert(current.clone());

        for nbr in filtered_neighbors(graph, &current, opts.edge_filter.as_ref())? {
            if closed.contains(&nbr) {
                continue;
            }
            let tentative_g = g_score.get(&current).copied().unwrap_or(f64::INFINITY) + 1.0;

            let nbr_g = g_score.get(&nbr).copied().unwrap_or(f64::INFINITY);
            if tentative_g < nbr_g {
                came_from.insert(nbr.clone(), current.clone());
                g_score.insert(nbr.clone(), tentative_g);
                f_score.insert(
                    nbr.clone(),
                    tentative_g + heuristic(&nbr, target, graph, directory),
                );
                open.insert(nbr);
            }
        }
    }

    Ok(None)
}
