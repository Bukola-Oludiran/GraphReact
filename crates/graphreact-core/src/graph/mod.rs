//! Graph layer — the [`CommunicationGraph`] data structure and supporting
//! types, views, and serialization.

pub mod attributes;
pub mod graph;
pub mod serialization;
pub mod types;
pub mod views;

pub use graph::{attrs_from, CommunicationGraph, Subscription};
pub use serialization::{from_json, to_graphml, to_json, JsonEdge, JsonGraph, JsonNode};
pub use types::{Attrs, EdgeSlot, EdgeTuple, NodeKey, TopologyEvent};
pub use views::{
    accept_all_edges, accept_all_nodes, subgraph_by_nodes, subgraph_view, EdgeFilter, NodeFilter,
    SubgraphView,
};
