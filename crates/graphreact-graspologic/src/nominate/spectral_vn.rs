//! Spectral Vertex Nomination on a single graph.
//!
//! Given a graph and a "seed" subset of its vertices, rank the
//! remaining vertices by relevance to the seed. The simple unattributed
//! procedure: embed the graph spectrally, then for each seed vertex
//! return the closest non-seed vertices in latent space.
//!
//! Reference: Fishkind, Lyzinski, Pao, Chen, Priebe — "Vertex
//! nomination schemes for membership prediction", Annals of Applied
//! Statistics, 2015.

use faer::{Mat, MatRef};
use thiserror::Error;

use graphreact_core::CommunicationGraph;

use crate::embed::{
    adjacency_matrix, adjacency_spectral_embed, laplacian_spectral_embed, AseOptions, EmbedError,
    LseOptions,
};

#[derive(Debug, Error)]
pub enum NominationError {
    #[error("graph has no vertices")]
    Empty,
    #[error("seed list is empty")]
    EmptySeed,
    #[error("seed contains unknown vertex `{0}`")]
    UnknownSeed(String),
    #[error("seed index {0} is out of bounds for n_vertices={1}")]
    SeedOutOfBounds(usize, usize),
    #[error("n_neighbors must be ≥ 1")]
    InvalidNeighbors,
    #[error("embed error: {0}")]
    Embed(#[from] EmbedError),
}

pub type NominationResult<T> = Result<T, NominationError>;

#[derive(Debug, Clone, Copy)]
pub enum NominationEmbedder {
    Ase,
    Lse,
}

#[derive(Debug, Clone)]
pub struct NominationOptions {
    pub n_components: usize,
    pub embedder: NominationEmbedder,
    /// Maximum number of nominees per seed. `None` ⇒ rank everything.
    pub n_neighbors: Option<usize>,
    pub weight_attribute: String,
}

impl Default for NominationOptions {
    fn default() -> Self {
        Self {
            n_components: 2,
            embedder: NominationEmbedder::Ase,
            n_neighbors: None,
            weight_attribute: "weight".to_string(),
        }
    }
}

/// One seed's nomination list — sorted ascending by distance.
#[derive(Debug, Clone)]
pub struct Nomination {
    pub seed_index: usize,
    /// Vertex indices ranked from closest to farthest.
    pub ranking: Vec<usize>,
    pub distances: Vec<f64>,
}

/// Nominate vertices of a [`CommunicationGraph`] using spectral
/// embedding + nearest-neighbor search.
pub fn spectral_vertex_nomination(
    graph: &CommunicationGraph,
    seed: &[String],
    opts: &NominationOptions,
) -> NominationResult<Vec<Nomination>> {
    let n = graph.order();
    if n == 0 {
        return Err(NominationError::Empty);
    }
    if seed.is_empty() {
        return Err(NominationError::EmptySeed);
    }
    if matches!(opts.n_neighbors, Some(0)) {
        return Err(NominationError::InvalidNeighbors);
    }

    let nodes: Vec<&str> = graph.nodes().collect();
    let mut idx_of = std::collections::HashMap::with_capacity(n);
    for (i, id) in nodes.iter().enumerate() {
        idx_of.insert(id.to_string(), i);
    }
    let seed_indices: Vec<usize> = seed
        .iter()
        .map(|id| {
            idx_of
                .get(id)
                .copied()
                .ok_or_else(|| NominationError::UnknownSeed(id.clone()))
        })
        .collect::<Result<_, _>>()?;

    let latent = match opts.embedder {
        NominationEmbedder::Ase => {
            adjacency_spectral_embed(
                graph,
                &AseOptions {
                    n_components: Some(opts.n_components),
                    weight_attribute: opts.weight_attribute.clone(),
                    diag_aug: true,
                    n_elbows: 1,
                },
            )?
            .latent_left
        }
        NominationEmbedder::Lse => {
            laplacian_spectral_embed(
                graph,
                &LseOptions {
                    n_components: Some(opts.n_components),
                    weight_attribute: opts.weight_attribute.clone(),
                    n_elbows: 1,
                    ..Default::default()
                },
            )?
            .latent_left
        }
    };
    spectral_vertex_nomination_matrix(latent.as_ref(), &seed_indices, opts.n_neighbors)
}

/// Lower-level entry point: nominate from a precomputed embedding.
pub fn spectral_vertex_nomination_matrix(
    latent: MatRef<'_, f64>,
    seed_indices: &[usize],
    n_neighbors: Option<usize>,
) -> NominationResult<Vec<Nomination>> {
    let n = latent.nrows();
    if n == 0 {
        return Err(NominationError::Empty);
    }
    if seed_indices.is_empty() {
        return Err(NominationError::EmptySeed);
    }
    if matches!(n_neighbors, Some(0)) {
        return Err(NominationError::InvalidNeighbors);
    }
    for &s in seed_indices {
        if s >= n {
            return Err(NominationError::SeedOutOfBounds(s, n));
        }
    }
    let dist = pairwise_distance(latent);
    let mut out = Vec::with_capacity(seed_indices.len());
    for &s in seed_indices {
        let mut ranked: Vec<(usize, f64)> = (0..n)
            .filter(|&v| v != s)
            .map(|v| (v, dist[(s, v)]))
            .collect();
        ranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(k) = n_neighbors {
            ranked.truncate(k);
        }
        let (ranking, distances) = ranked.into_iter().unzip();
        out.push(Nomination {
            seed_index: s,
            ranking,
            distances,
        });
    }
    Ok(out)
}

/// Convenience: nominate from an adjacency matrix (skipping the
/// CommunicationGraph helpers).
pub fn spectral_vertex_nomination_from_adjacency(
    a: MatRef<'_, f64>,
    seed_indices: &[usize],
    n_components: usize,
    n_neighbors: Option<usize>,
) -> NominationResult<Vec<Nomination>> {
    let emb = crate::embed::adjacency_spectral_embed_matrix(a, Some(n_components), 1, Some(false))?;
    spectral_vertex_nomination_matrix(emb.latent_left.as_ref(), seed_indices, n_neighbors)
}

fn pairwise_distance(x: MatRef<'_, f64>) -> Mat<f64> {
    let n = x.nrows();
    let d = x.ncols();
    let mut out = Mat::<f64>::zeros(n, n);
    for i in 0..n {
        for j in (i + 1)..n {
            let mut s = 0.0_f64;
            for c in 0..d {
                let dif = x[(i, c)] - x[(j, c)];
                s += dif * dif;
            }
            let v = s.sqrt();
            out[(i, j)] = v;
            out[(j, i)] = v;
        }
    }
    out
}

// Suppress unused warning when `adjacency_matrix` is the lowest-LOC
// helper we expose for advanced callers.
#[allow(dead_code)]
fn _adjacency_helper(g: &CommunicationGraph, weight: &str) -> Mat<f64> {
    adjacency_matrix(g, weight)
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::Attrs;

    fn two_clique_graph() -> CommunicationGraph {
        let mut g = CommunicationGraph::new();
        for i in 0..6 {
            g.add_node(format!("n{}", i), Attrs::new());
        }
        for &(u, v) in &[
            ("n0", "n1"),
            ("n0", "n2"),
            ("n1", "n2"),
            ("n3", "n4"),
            ("n3", "n5"),
            ("n4", "n5"),
            ("n2", "n3"), // single bridge
        ] {
            g.add_edge(u, v, None, Attrs::new());
        }
        g
    }

    #[test]
    fn nominee_list_excludes_seed() {
        let g = two_clique_graph();
        let result = spectral_vertex_nomination(
            &g,
            &["n0".to_string()],
            &NominationOptions {
                n_components: 2,
                ..Default::default()
            },
        )
        .expect("ok");
        let nom = &result[0];
        assert_eq!(nom.seed_index, 0);
        assert!(!nom.ranking.contains(&0));
        assert_eq!(nom.ranking.len(), 5);
    }

    #[test]
    fn n_neighbors_truncates_ranking() {
        let g = two_clique_graph();
        let result = spectral_vertex_nomination(
            &g,
            &["n0".to_string()],
            &NominationOptions {
                n_components: 2,
                n_neighbors: Some(2),
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(result[0].ranking.len(), 2);
    }

    #[test]
    fn ranking_is_sorted_by_distance() {
        let g = two_clique_graph();
        let result =
            spectral_vertex_nomination(&g, &["n0".to_string()], &NominationOptions::default())
                .expect("ok");
        for w in result[0].distances.windows(2) {
            assert!(w[0] <= w[1] + 1e-12);
        }
    }

    #[test]
    fn nomination_prioritises_same_clique() {
        let g = two_clique_graph();
        let result = spectral_vertex_nomination(
            &g,
            &["n0".to_string()],
            &NominationOptions {
                n_components: 2,
                ..Default::default()
            },
        )
        .expect("ok");
        // First two nominees should be from {n1, n2} (same triangle)
        // since they're identical to n0 in the latent space.
        let first_two: Vec<usize> = result[0].ranking.iter().take(2).copied().collect();
        for &v in &first_two {
            assert!(v == 1 || v == 2, "got nominee {v}");
        }
    }

    #[test]
    fn unknown_seed_errors() {
        let g = two_clique_graph();
        let result =
            spectral_vertex_nomination(&g, &["nope".to_string()], &NominationOptions::default());
        assert!(matches!(result, Err(NominationError::UnknownSeed(_))));
    }

    #[test]
    fn matrix_entry_point_handles_seed_oob() {
        let mut latent = Mat::<f64>::zeros(3, 2);
        latent[(0, 0)] = 1.0;
        latent[(1, 1)] = 1.0;
        let result = spectral_vertex_nomination_matrix(latent.as_ref(), &[5], None);
        assert!(matches!(
            result,
            Err(NominationError::SeedOutOfBounds(5, 3))
        ));
    }

    #[test]
    fn lse_embedder_path_runs() {
        let g = two_clique_graph();
        let result = spectral_vertex_nomination(
            &g,
            &["n0".to_string()],
            &NominationOptions {
                n_components: 2,
                embedder: NominationEmbedder::Lse,
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(result[0].ranking.len(), 5);
    }
}
