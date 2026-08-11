//! Build-time graph validation — surfaces likely-misconfigured topology
//! before runtime. Mirrors the TypeScript `validateGraph` function from
//! `core/dsl/validation.ts`.
//!
//! Three checks:
//! 1. Cycle detection — warns if cycles exist (potential infinite loops
//!    without TTL).
//! 2. Permission compatibility — warns if connected nodes share no
//!    compatible message types in either direction.
//! 3. Isolated nodes — warns about nodes with no edges (can neither
//!    send nor receive).
//!
//! Manifests are passed in as a map (keyed by node ID) because they live
//! on `NodeInstance` in `graphreact-core`'s scheduler — algorithms code
//! intentionally has no scheduler dependency.

use graphreact_core::{CommunicationGraph, PermissionManifest};
use std::collections::BTreeMap;

use crate::cycles::has_cycle;
use crate::edges::connected_pairs;

/// Severity of a validation finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningLevel {
    Warn,
    Error,
}

/// A single validation finding.
#[derive(Debug, Clone)]
pub struct ValidationWarning {
    pub level: WarningLevel,
    pub message: String,
}

/// Validate a graph against a per-node manifest map. Returns the list of
/// findings without throwing — callers decide whether to log, fail, or
/// continue.
///
/// `manifests` is keyed by node ID; nodes without an entry are skipped
/// for the compatibility check (their permission state is unknown).
pub fn validate_graph(
    graph: &CommunicationGraph,
    manifests: &BTreeMap<String, PermissionManifest>,
) -> Vec<ValidationWarning> {
    let mut warnings: Vec<ValidationWarning> = Vec::new();

    if matches!(has_cycle(graph), Ok(true)) {
        warnings.push(ValidationWarning {
            level: WarningLevel::Warn,
            message: "Graph contains cycles. Ensure TTL is configured on cyclic edges to prevent \
                 infinite message loops."
                .into(),
        });
    }

    for (u, v) in connected_pairs(graph) {
        let (Some(mu), Some(mv)) = (manifests.get(&u), manifests.get(&v)) else {
            continue;
        };
        let u_to_v = mu.emits.iter().any(|t| mv.accepts.contains(t));
        let v_to_u = mv.emits.iter().any(|t| mu.accepts.contains(t));
        if !u_to_v && !v_to_u {
            warnings.push(ValidationWarning {
                level: WarningLevel::Warn,
                message: format!(
                    "Nodes '{u}' and '{v}' are connected but share no compatible message types. \
                     '{u}' emits [{}], '{v}' accepts [{}]. \
                     '{v}' emits [{}], '{u}' accepts [{}].",
                    mu.emits.join(", "),
                    mv.accepts.join(", "),
                    mv.emits.join(", "),
                    mu.accepts.join(", "),
                ),
            });
        }
    }

    for id in graph.nodes() {
        if graph.neighbors(id).map(|ns| ns.is_empty()).unwrap_or(true) {
            warnings.push(ValidationWarning {
                level: WarningLevel::Warn,
                message: format!(
                    "Node '{id}' is isolated (no edges). It cannot send or receive messages."
                ),
            });
        }
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::Attrs;

    fn manifest_for(accepts: &[&str], emits: &[&str]) -> PermissionManifest {
        PermissionManifest::new(
            accepts
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
            emits
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn isolated_node_warns() {
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        let warnings = validate_graph(&g, &BTreeMap::new());
        assert!(warnings.iter().any(|w| w.message.contains("isolated")));
    }

    #[test]
    fn cycle_warns() {
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        g.add_node("b", Attrs::new());
        g.add_node("c", Attrs::new());
        // Triangle — cycle.
        g.add_edge("a", "b", None, Attrs::new());
        g.add_edge("b", "c", None, Attrs::new());
        g.add_edge("c", "a", None, Attrs::new());
        let warnings = validate_graph(&g, &BTreeMap::new());
        assert!(warnings.iter().any(|w| w.message.contains("cycles")));
    }

    #[test]
    fn incompatible_neighbors_warn() {
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        g.add_node("b", Attrs::new());
        g.add_edge("a", "b", None, Attrs::new());
        let mut manifests = BTreeMap::new();
        manifests.insert("a".to_string(), manifest_for(&["ping"], &["foo"]));
        manifests.insert("b".to_string(), manifest_for(&["bar"], &["pong"]));
        let warnings = validate_graph(&g, &manifests);
        assert!(warnings
            .iter()
            .any(|w| w.message.contains("no compatible message types")));
    }

    #[test]
    fn compatible_neighbors_no_warning() {
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        g.add_node("b", Attrs::new());
        g.add_edge("a", "b", None, Attrs::new());
        let mut manifests = BTreeMap::new();
        manifests.insert("a".to_string(), manifest_for(&["pong"], &["ping"]));
        manifests.insert("b".to_string(), manifest_for(&["ping"], &["pong"]));
        let warnings = validate_graph(&g, &manifests);
        assert!(!warnings
            .iter()
            .any(|w| w.message.contains("no compatible message types")));
    }
}
