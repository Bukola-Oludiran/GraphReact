//! Community partitioning algorithms — Leiden + modularity.
//!
//! Mirrors graspologic's `partition` module: takes an undirected
//! [`CommunicationGraph`] and a weight attribute, returns a
//! community-id-per-node mapping (Leiden) or a quality score
//! (modularity).
//!
//! Edge weights default to `1.0` when the named attribute is absent
//! or non-numeric. Self-loops contribute single-counted weight to the
//! intra-community degree (matching the Python reference's branch on
//! `vertex == neighbor_vertex`).

mod leiden;
mod modularity;

pub use leiden::{leiden, LeidenError, LeidenOptions, PartitionMap};
pub use modularity::{modularity, modularity_components, ModularityError};
