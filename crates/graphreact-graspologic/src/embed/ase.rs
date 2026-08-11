//! Adjacency Spectral Embedding.
//!
//! `A ≈ U Σ V^T`; latent positions are `U sqrt(Σ)`. For undirected
//! graphs (`A` symmetric) only the left embedding is returned. For
//! directed graphs an additional right embedding is produced.
//!
//! Reference: Sussman et al., "A Consistent Adjacency Spectral
//! Embedding for Stochastic Blockmodel Graphs", JASA 2012.

use faer::{Mat, MatRef};

use graphreact_core::CommunicationGraph;

use super::error::{EmbedError, EmbedResult};
use super::matrix::{adjacency_matrix, augment_diagonal_in_place, is_almost_symmetric};
use super::select::{select_svd, SelectSvd};

#[derive(Debug, Clone)]
pub struct AseOptions {
    /// Explicit embedding dimension; when `None`, picked via Zhu & Ghodsi.
    pub n_components: Option<usize>,
    /// Number of elbows to walk in the auto-dim path. Ignored when
    /// `n_components` is set.
    pub n_elbows: usize,
    /// Replace the diagonal with degree-derived values before embedding.
    /// Empirically improves latent quality on stochastic blockmodels.
    pub diag_aug: bool,
    /// Edge attribute holding the float weight; missing → 1.0.
    pub weight_attribute: String,
}

impl Default for AseOptions {
    fn default() -> Self {
        Self {
            n_components: None,
            n_elbows: 2,
            diag_aug: true,
            weight_attribute: "weight".to_string(),
        }
    }
}

/// Result of a spectral embedding. `latent_left` is `(n × k)`.
/// `latent_right` is populated only when the input adjacency / Laplacian
/// is asymmetric.
#[derive(Debug, Clone)]
pub struct Embedding {
    pub latent_left: Mat<f64>,
    pub latent_right: Option<Mat<f64>>,
    pub singular_values: Vec<f64>,
    pub n_components: usize,
}

/// Convenience entry point: ASE on a [`CommunicationGraph`].
pub fn adjacency_spectral_embed(
    graph: &CommunicationGraph,
    opts: &AseOptions,
) -> EmbedResult<Embedding> {
    if graph.order() == 0 {
        return Err(EmbedError::Empty);
    }
    let mut a = adjacency_matrix(graph, &opts.weight_attribute);
    if opts.diag_aug {
        augment_diagonal_in_place(&mut a);
    }
    adjacency_spectral_embed_matrix(a.as_ref(), opts.n_components, opts.n_elbows, None)
}

/// Lower-level entry point: ASE on a pre-built adjacency matrix.
///
/// `directed = None` infers from `is_almost_symmetric`. Pass
/// `Some(true|false)` to override (useful when callers know the input
/// is symmetric by construction and want to skip the right embedding).
pub fn adjacency_spectral_embed_matrix(
    a: MatRef<'_, f64>,
    n_components: Option<usize>,
    n_elbows: usize,
    directed: Option<bool>,
) -> EmbedResult<Embedding> {
    let svd = select_svd(a, n_components, n_elbows)?;
    let directed = directed.unwrap_or_else(|| !is_almost_symmetric(a, 1e-15));
    Ok(embedding_from_svd(svd, directed))
}

/// Build an [`Embedding`] from an already-truncated SVD.
pub(super) fn embedding_from_svd(svd: SelectSvd, directed: bool) -> Embedding {
    let SelectSvd {
        u,
        s,
        vt,
        n_components,
    } = svd;
    let sqrt_s: Vec<f64> = s.iter().map(|&v| v.max(0.0).sqrt()).collect();

    let n_left = u.nrows();
    let mut latent_left = Mat::<f64>::zeros(n_left, n_components);
    for r in 0..n_left {
        for c in 0..n_components {
            latent_left[(r, c)] = u[(r, c)] * sqrt_s[c];
        }
    }

    let latent_right = if directed {
        let n_right = vt.ncols();
        let mut lr = Mat::<f64>::zeros(n_right, n_components);
        for r in 0..n_right {
            for c in 0..n_components {
                lr[(r, c)] = vt[(c, r)] * sqrt_s[c];
            }
        }
        Some(lr)
    } else {
        None
    };

    Embedding {
        latent_left,
        latent_right,
        singular_values: s,
        n_components,
    }
}

#[cfg(test)]
mod tests {
    use super::super::matrix::adjacency_matrix;
    use super::super::procrustes::procrustes_distance;
    use super::*;
    use graphreact_core::Attrs;

    fn block_diagonal_matrix(block_size: usize, n_blocks: usize, p: f64) -> Mat<f64> {
        let n = block_size * n_blocks;
        let mut a = Mat::<f64>::zeros(n, n);
        for b in 0..n_blocks {
            for i in 0..block_size {
                for j in 0..block_size {
                    if i == j {
                        continue;
                    }
                    a[(b * block_size + i, b * block_size + j)] = p;
                }
            }
        }
        a
    }

    fn cluster_centroid(latent: &Mat<f64>, rows: std::ops::Range<usize>) -> Vec<f64> {
        let k = latent.ncols();
        let n = (rows.end - rows.start) as f64;
        let mut c = vec![0.0; k];
        for r in rows {
            for i in 0..k {
                c[i] += latent[(r, i)];
            }
        }
        for v in &mut c {
            *v /= n;
        }
        c
    }

    fn euclid(a: &[f64], b: &[f64]) -> f64 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).powi(2))
            .sum::<f64>()
            .sqrt()
    }

    #[test]
    fn ase_two_block_sbm_separates_blocks() {
        // Two disjoint cliques (block-diagonal adjacency). ASE latent
        // positions for the two blocks should be separated.
        let a = block_diagonal_matrix(5, 2, 1.0);
        let emb = adjacency_spectral_embed_matrix(a.as_ref(), Some(2), 1, Some(false)).expect("ok");
        assert_eq!(emb.n_components, 2);
        assert_eq!(emb.latent_left.nrows(), 10);
        assert!(emb.latent_right.is_none());

        let c0 = cluster_centroid(&emb.latent_left, 0..5);
        let c1 = cluster_centroid(&emb.latent_left, 5..10);
        let between = euclid(&c0, &c1);

        // Within-cluster spread is 0 (every node in a clique has
        // identical row in A) — so any nonzero between-cluster distance
        // is "well separated".
        assert!(between > 1e-6, "blocks must separate (got {between})");
    }

    #[test]
    fn ase_singular_values_match_known_diagonal() {
        // A simple symmetric matrix with known eigenvalues.
        // Triangle graph (n=3 ring): eigenvalues of A are 2, -1, -1; singular values |λ|.
        let mut a = Mat::<f64>::zeros(3, 3);
        for (i, j) in [(0, 1), (1, 2), (0, 2)] {
            a[(i, j)] = 1.0;
            a[(j, i)] = 1.0;
        }
        let emb = adjacency_spectral_embed_matrix(a.as_ref(), Some(3), 1, Some(false)).expect("ok");
        let mut sv = emb.singular_values.clone();
        sv.sort_by(|a, b| b.partial_cmp(a).unwrap());
        assert!((sv[0] - 2.0).abs() < 1e-10);
        assert!((sv[1] - 1.0).abs() < 1e-10);
        assert!((sv[2] - 1.0).abs() < 1e-10);
    }

    #[test]
    fn ase_idempotent_under_procrustes() {
        // Running ASE twice on the same matrix yields the same latent
        // positions up to rotation — so Procrustes-aligned distance is 0.
        let mut a = Mat::<f64>::zeros(5, 5);
        for i in 0..5 {
            for j in 0..5 {
                if i != j {
                    a[(i, j)] = 1.0;
                }
            }
        }
        let e1 = adjacency_spectral_embed_matrix(a.as_ref(), Some(2), 1, Some(false)).expect("ok");
        let e2 = adjacency_spectral_embed_matrix(a.as_ref(), Some(2), 1, Some(false)).expect("ok");
        let d = procrustes_distance(e1.latent_left.as_ref(), e2.latent_left.as_ref()).expect("ok");
        assert!(d < 1e-8, "expected aligned distance ~0, got {d}");
    }

    #[test]
    fn ase_on_communication_graph_smoke() {
        let mut g = CommunicationGraph::new();
        for i in 0..6 {
            g.add_node(format!("n{}", i), Attrs::new());
        }
        // Two cliques: {n0,n1,n2} and {n3,n4,n5}.
        for &(u, v) in &[
            ("n0", "n1"),
            ("n0", "n2"),
            ("n1", "n2"),
            ("n3", "n4"),
            ("n3", "n5"),
            ("n4", "n5"),
        ] {
            g.add_edge(u, v, None, Attrs::new());
        }
        let emb = adjacency_spectral_embed(
            &g,
            &AseOptions {
                n_components: Some(2),
                diag_aug: false,
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(emb.latent_left.nrows(), 6);
        assert_eq!(emb.latent_left.ncols(), 2);
    }

    #[test]
    fn ase_diag_aug_changes_singular_values() {
        let mut g = CommunicationGraph::new();
        for i in 0..4 {
            g.add_node(format!("n{}", i), Attrs::new());
        }
        for &(u, v) in &[("n0", "n1"), ("n1", "n2"), ("n2", "n3"), ("n0", "n3")] {
            g.add_edge(u, v, None, Attrs::new());
        }

        let with_aug = adjacency_spectral_embed(
            &g,
            &AseOptions {
                n_components: Some(2),
                diag_aug: true,
                ..Default::default()
            },
        )
        .expect("ok");
        let no_aug = adjacency_spectral_embed(
            &g,
            &AseOptions {
                n_components: Some(2),
                diag_aug: false,
                ..Default::default()
            },
        )
        .expect("ok");

        // Diagonal augmentation perturbs the spectrum — values should differ.
        let diff: f64 = with_aug
            .singular_values
            .iter()
            .zip(no_aug.singular_values.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(diff > 1e-6, "diag_aug should affect singular values");
    }

    #[test]
    fn ase_directed_yields_right_embedding() {
        // Asymmetric matrix: rectangular nilpotent.
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 1)] = 1.0;
        a[(1, 2)] = 1.0;
        a[(0, 2)] = 0.5;
        let emb = adjacency_spectral_embed_matrix(a.as_ref(), Some(2), 1, None).expect("ok");
        assert!(emb.latent_right.is_some());
    }

    #[test]
    fn ase_uses_adjacency_matrix_helper_consistently() {
        let mut g = CommunicationGraph::new();
        for i in 0..3 {
            g.add_node(format!("n{}", i), Attrs::new());
        }
        g.add_edge("n0", "n1", None, Attrs::new());
        g.add_edge("n1", "n2", None, Attrs::new());

        let direct = adjacency_matrix(&g, "weight");
        let from_graph = adjacency_spectral_embed(
            &g,
            &AseOptions {
                n_components: Some(1),
                diag_aug: false,
                ..Default::default()
            },
        )
        .expect("ok");
        let from_matrix =
            adjacency_spectral_embed_matrix(direct.as_ref(), Some(1), 1, Some(false)).expect("ok");
        assert!((from_graph.singular_values[0] - from_matrix.singular_values[0]).abs() < 1e-12);
    }
}
