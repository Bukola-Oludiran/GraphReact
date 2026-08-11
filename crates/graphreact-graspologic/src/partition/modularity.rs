//! Modularity calculation — direct port of graspologic's
//! `partition/modularity.py` semantics, adapted to the
//! `CommunicationGraph` data model.
//!
//! The Python reference computes modularity per Newman & Girvan (2004)
//! with the Leicht–Newman resolution parameter:
//!
//! ```text
//! Q = (1 / 2m) * Σ_i (e_ii - γ * (k_i^2 / 2m))
//! ```
//!
//! where `e_ii` is twice the sum of edge weights internal to community
//! `i` (counting each edge twice and self-loops once), `k_i` is the
//! sum of all edge endpoints attributed to community `i`, `m` is the
//! total edge weight, and `γ` is the resolution parameter.
//!
//! The implementation walks edges once, accumulating
//! `degree_sums_within_community` and `degree_sums_for_community` per
//! community id, then applies the formula component-by-component.

use rustc_hash::FxHashMap;
use thiserror::Error;

use graphreact_core::CommunicationGraph;

use super::leiden::PartitionMap;

#[derive(Debug, Error)]
pub enum ModularityError {
    #[error("partition map references node `{0}` which is not in the graph")]
    UnknownNode(String),
    #[error("partition map is missing node `{0}` from the graph")]
    MissingNode(String),
}

/// Compute the per-community modularity components. Sum to total
/// modularity; the breakdown is sometimes useful for diagnosing which
/// communities are well-formed vs. weakly-defined.
///
/// `weight_attribute` is read from each edge's `Attrs`; missing /
/// non-numeric values fall back to `1.0`. `resolution = 1.0` recovers
/// classic Newman–Girvan modularity.
pub fn modularity_components(
    graph: &CommunicationGraph,
    partitions: &PartitionMap,
    weight_attribute: &str,
    resolution: f64,
) -> Result<FxHashMap<u64, f64>, ModularityError> {
    validate_partition(graph, partitions)?;

    let mut total_edge_weight = 0.0_f64;
    let mut within_community: FxHashMap<u64, f64> = FxHashMap::default();
    let mut total_community: FxHashMap<u64, f64> = FxHashMap::default();

    for (u, v, key) in graph.edges() {
        let attrs = graph
            .get_edge_data(&u, &v, Some(&key))
            .expect("edge from edges() must have data");
        let weight = edge_weight(attrs, weight_attribute);

        // SAFETY: validate_partition ensured every node has a community
        let u_comm = *partitions.get(&u).expect("validated");
        let v_comm = *partitions.get(&v).expect("validated");

        if u_comm == v_comm {
            // Self-loop contributes once; cross-loop contributes
            // twice (once from each endpoint side, matching the
            // Python `weight * 2.0` for non-self-loop intra-community
            // edges).
            let contribution = if u == v { weight } else { weight * 2.0 };
            *within_community.entry(u_comm).or_insert(0.0) += contribution;
        }

        *total_community.entry(u_comm).or_insert(0.0) += weight;
        *total_community.entry(v_comm).or_insert(0.0) += weight;
        total_edge_weight += weight;
    }

    let two_m = 2.0 * total_edge_weight;
    let mut components: FxHashMap<u64, f64> = FxHashMap::default();

    let community_ids: std::collections::BTreeSet<u64> = partitions.values().copied().collect();
    for comm in community_ids {
        let intra = within_community.get(&comm).copied().unwrap_or(0.0);
        let total_deg = total_community.get(&comm).copied().unwrap_or(0.0);
        let component = if two_m == 0.0 {
            0.0
        } else {
            let degree_ratio = (total_deg * total_deg) / two_m;
            (intra - resolution * degree_ratio) / two_m
        };
        components.insert(comm, component);
    }

    Ok(components)
}

/// Total modularity = sum of all per-community components.
pub fn modularity(
    graph: &CommunicationGraph,
    partitions: &PartitionMap,
    weight_attribute: &str,
    resolution: f64,
) -> Result<f64, ModularityError> {
    let components = modularity_components(graph, partitions, weight_attribute, resolution)?;
    Ok(components.values().sum())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(super) fn edge_weight(attrs: &graphreact_core::Attrs, attr_name: &str) -> f64 {
    // `as_f64` is `serde_json::Value::as_f64` but we don't depend on
    // `serde_json` directly — closure form keeps the type inferred
    // through `graphreact_core::Attrs`'s value alias.
    #[allow(clippy::redundant_closure_for_method_calls)]
    attrs.get(attr_name).and_then(|v| v.as_f64()).unwrap_or(1.0)
}

fn validate_partition(
    graph: &CommunicationGraph,
    partitions: &PartitionMap,
) -> Result<(), ModularityError> {
    for node in graph.nodes() {
        if !partitions.contains_key(node) {
            return Err(ModularityError::MissingNode(node.to_string()));
        }
    }
    for node in partitions.keys() {
        if !graph.has_node(node) {
            return Err(ModularityError::UnknownNode(node.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::leiden::PartitionMap;
    use super::*;
    use graphreact_core::{Attrs, CommunicationGraph};
    use serde_json::json;

    fn weighted_edge(g: &mut CommunicationGraph, u: &str, v: &str, w: f64) {
        let mut attrs = Attrs::new();
        attrs.insert("weight".into(), json!(w));
        g.add_edge(u, v, None, attrs);
    }

    fn unweighted_edge(g: &mut CommunicationGraph, u: &str, v: &str) {
        weighted_edge(g, u, v, 1.0);
    }

    fn singleton_partition(graph: &CommunicationGraph) -> PartitionMap {
        graph
            .nodes()
            .enumerate()
            .map(|(i, id)| (id.to_string(), i as u64))
            .collect()
    }

    fn one_community_partition(graph: &CommunicationGraph) -> PartitionMap {
        graph.nodes().map(|id| (id.to_string(), 0u64)).collect()
    }

    /// Two well-separated triangles connected by a single edge. The
    /// natural community partition (each triangle = one community)
    /// should produce positive modularity.
    fn two_triangles() -> CommunicationGraph {
        let mut g = CommunicationGraph::new();
        for n in ["a", "b", "c", "d", "e", "f"] {
            g.add_node(n, Attrs::new());
        }
        unweighted_edge(&mut g, "a", "b");
        unweighted_edge(&mut g, "b", "c");
        unweighted_edge(&mut g, "a", "c");
        unweighted_edge(&mut g, "d", "e");
        unweighted_edge(&mut g, "e", "f");
        unweighted_edge(&mut g, "d", "f");
        unweighted_edge(&mut g, "c", "d");
        g
    }

    #[test]
    fn singleton_partition_modularity_is_negative_or_zero() {
        // Singletons have no intra-community edges → first term zero.
        // The expected term is positive → modularity ≤ 0.
        let g = two_triangles();
        let p = singleton_partition(&g);
        let q = modularity(&g, &p, "weight", 1.0).unwrap();
        assert!(q <= 1e-9, "expected non-positive modularity, got {q}");
    }

    #[test]
    fn one_community_modularity_is_zero() {
        // All in one community: intra term = 2m, expected = 2m,
        // (2m - 2m) / 2m = 0.
        let g = two_triangles();
        let p = one_community_partition(&g);
        let q = modularity(&g, &p, "weight", 1.0).unwrap();
        assert!(q.abs() < 1e-9, "expected zero modularity, got {q}");
    }

    #[test]
    fn natural_partition_has_higher_modularity_than_singleton() {
        let g = two_triangles();
        let mut natural: PartitionMap = PartitionMap::default();
        for n in ["a", "b", "c"] {
            natural.insert(n.into(), 0);
        }
        for n in ["d", "e", "f"] {
            natural.insert(n.into(), 1);
        }
        let q_natural = modularity(&g, &natural, "weight", 1.0).unwrap();
        let q_singleton = modularity(&g, &singleton_partition(&g), "weight", 1.0).unwrap();
        assert!(
            q_natural > q_singleton,
            "natural ({q_natural}) should beat singleton ({q_singleton})"
        );
        assert!(
            q_natural > 0.3,
            "natural partition should be a strong split, got {q_natural}"
        );
    }

    #[test]
    fn natural_partition_modularity_known_value() {
        // Two K_3 triangles linked by single edge c-d.
        // m = 7 edges, all weight 1.
        // 2m = 14
        // Community A = {a,b,c}: intra edges = (a-b, b-c, a-c) = 3 → e_AA = 6
        //   total degree = a:2, b:2, c:3 (c has cross edge) → k_A = 7
        // Community B = {d,e,f}: intra edges = 3 → e_BB = 6
        //   total degree = d:3, e:2, f:2 → k_B = 7
        // Q = (6 - 7^2/14)/14 + (6 - 7^2/14)/14
        //   = 2 * ( (6 - 3.5) / 14 )
        //   = 2 * (2.5 / 14)
        //   = 5 / 14
        //   ≈ 0.357142857...
        let g = two_triangles();
        let mut natural: PartitionMap = PartitionMap::default();
        for n in ["a", "b", "c"] {
            natural.insert(n.into(), 0);
        }
        for n in ["d", "e", "f"] {
            natural.insert(n.into(), 1);
        }
        let q = modularity(&g, &natural, "weight", 1.0).unwrap();
        let expected = 5.0 / 14.0;
        assert!((q - expected).abs() < 1e-12, "expected {expected}, got {q}");
    }

    #[test]
    fn weighted_edges_are_respected() {
        // Single edge a-b with weight 2.0; one-community partition.
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        g.add_node("b", Attrs::new());
        weighted_edge(&mut g, "a", "b", 2.0);
        let p: PartitionMap = [("a", 0u64), ("b", 0)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let q = modularity(&g, &p, "weight", 1.0).unwrap();
        // m = 2, intra = 4, k = 4, expected = 16/4 = 4, (4-4)/4 = 0
        assert!(q.abs() < 1e-12);
    }

    #[test]
    fn missing_weight_attr_defaults_to_one() {
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        g.add_node("b", Attrs::new());
        g.add_edge("a", "b", None, Attrs::new()); // no weight attribute
        let p: PartitionMap = [("a", 0u64), ("b", 0)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let q = modularity(&g, &p, "weight", 1.0).unwrap();
        assert!(q.abs() < 1e-12, "default-1.0 weight should yield Q=0");
    }

    #[test]
    fn missing_node_in_partition_errors() {
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        g.add_node("b", Attrs::new());
        unweighted_edge(&mut g, "a", "b");
        let mut p = PartitionMap::default();
        p.insert("a".into(), 0);
        let err = modularity(&g, &p, "weight", 1.0).unwrap_err();
        assert!(matches!(err, ModularityError::MissingNode(ref id) if id == "b"));
    }

    #[test]
    fn unknown_node_in_partition_errors() {
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        let mut p = PartitionMap::default();
        p.insert("a".into(), 0);
        p.insert("phantom".into(), 1);
        let err = modularity(&g, &p, "weight", 1.0).unwrap_err();
        assert!(matches!(err, ModularityError::UnknownNode(ref id) if id == "phantom"));
    }

    #[test]
    fn resolution_parameter_lowers_modularity_when_increased() {
        // Higher γ penalises the expected term → modularity decreases.
        let g = two_triangles();
        let mut natural = PartitionMap::default();
        for n in ["a", "b", "c"] {
            natural.insert(n.into(), 0);
        }
        for n in ["d", "e", "f"] {
            natural.insert(n.into(), 1);
        }
        let q1 = modularity(&g, &natural, "weight", 1.0).unwrap();
        let q2 = modularity(&g, &natural, "weight", 2.0).unwrap();
        assert!(q2 < q1, "γ=2 ({q2}) should be less than γ=1 ({q1})");
    }

    #[test]
    fn empty_graph_has_zero_modularity() {
        let g = CommunicationGraph::new();
        let p = PartitionMap::default();
        let q = modularity(&g, &p, "weight", 1.0).unwrap();
        assert_eq!(q, 0.0);
    }

    #[test]
    fn modularity_components_sum_to_total() {
        let g = two_triangles();
        let mut natural = PartitionMap::default();
        for n in ["a", "b", "c"] {
            natural.insert(n.into(), 0);
        }
        for n in ["d", "e", "f"] {
            natural.insert(n.into(), 1);
        }
        let total = modularity(&g, &natural, "weight", 1.0).unwrap();
        let comps = modularity_components(&g, &natural, "weight", 1.0).unwrap();
        let sum: f64 = comps.values().sum();
        assert!((total - sum).abs() < 1e-12);
        assert_eq!(comps.len(), 2);
    }
}
