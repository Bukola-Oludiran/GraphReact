//! NetworkX-parity graph algorithms over [`graphreact_core::CommunicationGraph`].
//!
//! Pure functions — no scheduler dependency. Each module is independently
//! testable. Returns owned `Vec`/`Result<T>` rather than streaming iterators
//! to keep the API ergonomic at the crate boundary.

pub mod components;
pub mod cycles;
pub mod dag;
pub mod edges;
pub mod pathfinding;
pub mod traversal;
pub mod validation;

pub use components::{connected_components, is_connected, number_of_connected_components};
pub use cycles::{find_cycle, has_cycle};
pub use dag::{
    directed_adj_from, topological_generations, topological_sort, DagError, DirectedAdj,
};
pub use edges::{
    are_connected, connected_pairs, list_edges, list_edges_between, list_edges_of, EdgeInfo,
};
pub use pathfinding::{
    all_paths, astar_path, shortest_path, AStarOptions, AllPathsOptions, EdgeFilter, PathHeuristic,
    PathOptions,
};
pub use traversal::{bfs_edges, bfs_nodes, dfs_edges, dfs_nodes};
pub use validation::{validate_graph, ValidationWarning, WarningLevel};
