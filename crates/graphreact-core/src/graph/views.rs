//! Lazy subgraph views — filtered views over a [`CommunicationGraph`]
//! without copying data.
//!
//! Filters are applied on-the-fly during iteration; mutating the source
//! graph is reflected in the view automatically.

use std::sync::Arc;

use super::graph::CommunicationGraph;
use super::types::{Attrs, EdgeTuple};
use crate::error::GraphError;

/// A predicate over a node ID. Implementors must be `Send + Sync` because
/// the view holds the closure across thread boundaries (e.g., when used
/// from within tokio tasks).
pub type NodeFilter = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// A predicate over an edge `(u, v, key)`.
pub type EdgeFilter = Arc<dyn Fn(&str, &str, &str) -> bool + Send + Sync>;

/// Read-only view of a [`CommunicationGraph`] with optional node and edge
/// filtering.
///
/// Holds a borrow of the source graph for the lifetime of the view.
pub struct SubgraphView<'a> {
    source: &'a CommunicationGraph,
    node_filter: NodeFilter,
    edge_filter: EdgeFilter,
}

impl<'a> SubgraphView<'a> {
    /// Build a view with the given filters. Pass [`accept_all_nodes`] /
    /// [`accept_all_edges`] for unfiltered axes.
    pub fn new(
        source: &'a CommunicationGraph,
        node_filter: NodeFilter,
        edge_filter: EdgeFilter,
    ) -> Self {
        Self {
            source,
            node_filter,
            edge_filter,
        }
    }

    /// Whether the node is in both the source graph and the view's filter.
    pub fn has_node(&self, id: &str) -> bool {
        self.source.has_node(id) && (self.node_filter)(id)
    }

    /// Get a node's attributes through the view.
    pub fn get_node_data(&self, id: &str) -> Result<&Attrs, GraphError> {
        if !self.has_node(id) {
            return Err(GraphError::NodeMissing { id: id.to_string() });
        }
        self.source.get_node_data(id)
    }

    /// Iterate over node IDs in the view.
    pub fn nodes(&self) -> impl Iterator<Item = String> + '_ {
        self.source
            .nodes()
            .filter(|id| (self.node_filter)(id))
            .map(str::to_owned)
    }

    /// Number of nodes in the view.
    pub fn order(&self) -> usize {
        self.nodes().count()
    }

    /// Neighbors of `id` that pass the view's node filter and have at least
    /// one passing edge.
    pub fn neighbors(&self, id: &str) -> Result<Vec<String>, GraphError> {
        if !self.has_node(id) {
            return Err(GraphError::NodeMissing { id: id.to_string() });
        }
        let all = self.source.neighbors(id)?;
        Ok(all
            .into_iter()
            .filter(|nbr| (self.node_filter)(nbr) && self.has_edge(id, nbr, None))
            .collect())
    }

    /// Whether the edge is in the view. If `key` is provided, requires that
    /// key passes the edge filter; otherwise true if any key does.
    pub fn has_edge(&self, u: &str, v: &str, key: Option<&str>) -> bool {
        if !(self.node_filter)(u) || !(self.node_filter)(v) {
            return false;
        }
        if !self.source.has_edge(u, v, None) {
            return false;
        }
        match key {
            Some(k) => self.source.has_edge(u, v, Some(k)) && (self.edge_filter)(u, v, k),
            None => self
                .source
                .edge_keys(u, v)
                .iter()
                .any(|k| (self.edge_filter)(u, v, k)),
        }
    }

    /// Get edge attributes through the view.
    pub fn get_edge_data(&self, u: &str, v: &str, key: Option<&str>) -> Result<&Attrs, GraphError> {
        if !self.has_edge(u, v, key) {
            return Err(GraphError::edge_missing(u, v));
        }
        self.source.get_edge_data(u, v, key)
    }

    /// Iterate over visible edges as `(u, v, key)` tuples.
    pub fn edges(&self) -> impl Iterator<Item = EdgeTuple> + '_ {
        self.source.edges().filter(|(u, v, k)| {
            (self.node_filter)(u) && (self.node_filter)(v) && (self.edge_filter)(u, v, k)
        })
    }

    /// Number of edges in the view.
    pub fn size(&self) -> usize {
        self.edges().count()
    }
}

/// A node filter that accepts every node.
pub fn accept_all_nodes() -> NodeFilter {
    Arc::new(|_| true)
}

/// An edge filter that accepts every edge.
pub fn accept_all_edges() -> EdgeFilter {
    Arc::new(|_, _, _| true)
}

/// Build a [`SubgraphView`] filtered by node membership in the given collection.
///
/// Accepts any iterator of `String` IDs — typically a `HashSet` or `Vec`.
/// The IDs are collected into an `Arc<HashSet>` for cheap predicate use.
pub fn subgraph_by_nodes<I>(graph: &CommunicationGraph, node_ids: I) -> SubgraphView<'_>
where
    I: IntoIterator<Item = String>,
{
    let ids: Arc<std::collections::HashSet<String>> = Arc::new(node_ids.into_iter().collect());
    let node_filter: NodeFilter = Arc::new(move |id: &str| ids.contains(id));
    SubgraphView::new(graph, node_filter, accept_all_edges())
}

/// Build a [`SubgraphView`] with optional custom node and edge predicates.
pub fn subgraph_view(
    graph: &CommunicationGraph,
    node_filter: Option<NodeFilter>,
    edge_filter: Option<EdgeFilter>,
) -> SubgraphView<'_> {
    SubgraphView::new(
        graph,
        node_filter.unwrap_or_else(accept_all_nodes),
        edge_filter.unwrap_or_else(accept_all_edges),
    )
}
