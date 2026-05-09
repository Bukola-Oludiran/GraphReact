//! Core types for the graph layer.
//!
//! `NodeKey` and `EdgeSlot` are stable typed `slotmap` keys — Copy, Eq, and
//! Ord. They're the internal identity layer; user code identifies nodes by
//! `&str` and we maintain a string ↔ key map.

use indexmap::IndexMap;
use serde_json::Value;
use slotmap::new_key_type;

/// Arbitrary key-value attribute container, preserving insertion order.
///
/// Mirrors TypeScript's `Record<string, unknown>` with deterministic
/// iteration so `toJSON` / `toGraphML` output matches insertion order.
pub type Attrs = IndexMap<String, Value>;

new_key_type! {
    /// Stable identity for a node within a [`CommunicationGraph`].
    ///
    /// Cheap to copy. Survives across mutations of unrelated nodes.
    /// Recycled by `slotmap` after the node is removed.
    pub struct NodeKey;

    /// Stable identity for an edge within a [`CommunicationGraph`].
    pub struct EdgeSlot;
}

/// User-visible identifier for an edge in a multigraph: `(u, v, key)`.
///
/// `u` and `v` are node string IDs as supplied by the caller (not canonical).
/// `key` distinguishes parallel edges between the same pair.
pub type EdgeTuple = (String, String, String);

/// Events fired when the graph structure changes.
///
/// Mirrors the TypeScript `TopologyEvent` union — listeners registered via
/// [`crate::graph::CommunicationGraph::on_topology_change`] receive these
/// synchronously, in the order they were produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyEvent {
    NodeAdded { node_id: String },
    NodeRemoved { node_id: String },
    EdgeAdded { u: String, v: String, key: String },
    EdgeRemoved { u: String, v: String, key: String },
    EdgeUpdated { u: String, v: String, key: String },
}
