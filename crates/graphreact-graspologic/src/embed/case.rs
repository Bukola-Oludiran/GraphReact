//! Covariate-Assisted Spectral Embedding.
//!
//! Embeds a graph alongside per-vertex covariates by spectrally
//! decomposing
//!
//! * `L + α·Y Yᵀ`     (assortative graphs)  or
//! * `L·L + α·Y Yᵀ`   (non-assortative graphs)
//!
//! where `L` is the regularised normalised adjacency. When `α` is not
//! supplied it is chosen so that the leading eigenvalues of the two
//! summands are balanced.
//!
//! Reference: Binkiewicz, Vogelstein & Rohe — "Covariate-assisted
//! spectral clustering", Biometrika, 2017.
//!
//! ## Implementation note (feature parity, not line-by-line)
//!
//! Python uses `scipy.sparse.linalg.eigsh` over a `LinearOperator`,
//! avoiding materialisation of the `(n × n)` mixture matrix. This Rust
//! port materialises the matrix and runs faer-backed dense
//! eigendecomposition. The capability is identical; the numerical method
//! differs. This matches the project's "feature parity ≠ line-by-line"
//! stance — Iterative LinearOperator-style eigensolvers are deferred
//! until a real consumer demands them.

use faer::{Mat, MatRef};

use graphreact_core::CommunicationGraph;

use graphreact_linalg::{eigh as linalg_eigh, svd as linalg_svd};

use super::ase::Embedding;
use super::error::{EmbedError, EmbedResult};
use super::matrix::{adjacency_matrix, is_almost_symmetric, to_laplacian, LaplacianForm};

#[derive(Debug, Clone)]
pub struct CaseOptions {
    pub n_components: Option<usize>,
    pub n_elbows: usize,
    /// Mixing parameter. `None` derives `α` from the leading eigenvalues
    /// of `L` and `Y Yᵀ` so they contribute comparable signal.
    pub alpha: Option<f64>,
    /// `true` (default) — embed `L + α Y Yᵀ`. Better for assortative
    /// graphs (within-block edges denser than between-block).
    pub assortative: bool,
    /// Center each covariate column to zero mean before embedding.
    pub center_covariates: bool,
    /// Scale each covariate column to unit L2 norm after centering.
    pub scale_covariates: bool,
    pub weight_attribute: String,
}

impl Default for CaseOptions {
    fn default() -> Self {
        Self {
            n_components: None,
            n_elbows: 2,
            alpha: None,
            assortative: true,
            center_covariates: true,
            scale_covariates: true,
            weight_attribute: "weight".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaseEmbedding {
    pub embedding: Embedding,
    /// The mixing parameter actually used (auto-computed when `opts.alpha` was None).
    pub alpha_used: f64,
}

/// CASE on a [`CommunicationGraph`] with a covariate matrix.
///
/// `covariates` must have one row per vertex (in graph insertion order)
/// and one column per covariate.
pub fn covariate_assisted_embed(
    graph: &CommunicationGraph,
    covariates: MatRef<'_, f64>,
    opts: &CaseOptions,
) -> EmbedResult<CaseEmbedding> {
    let n = graph.order();
    if n == 0 {
        return Err(EmbedError::Empty);
    }
    if covariates.nrows() != n {
        return Err(EmbedError::CovariateMismatch {
            cov_rows: covariates.nrows(),
            n_vertices: n,
        });
    }
    let a = adjacency_matrix(graph, &opts.weight_attribute);
    if !is_almost_symmetric(a.as_ref(), 1e-12) {
        // Python raises NotImplementedError for directed input — match.
        return Err(EmbedError::NotSquare {
            rows: a.nrows(),
            cols: a.ncols(),
        });
    }
    covariate_assisted_embed_matrix(a.as_ref(), covariates, opts)
}

/// CASE on a pre-built adjacency matrix.
pub fn covariate_assisted_embed_matrix(
    a: MatRef<'_, f64>,
    covariates: MatRef<'_, f64>,
    opts: &CaseOptions,
) -> EmbedResult<CaseEmbedding> {
    let n = a.nrows();
    if n == 0 {
        return Err(EmbedError::Empty);
    }
    if a.ncols() != n {
        return Err(EmbedError::NotSquare {
            rows: n,
            cols: a.ncols(),
        });
    }
    if covariates.nrows() != n {
        return Err(EmbedError::CovariateMismatch {
            cov_rows: covariates.nrows(),
            n_vertices: n,
        });
    }

    // Regularised Laplacian (`R-DAD` form).
    let l = to_laplacian(a, LaplacianForm::RDad, None)?;

    // Centre + scale covariates, mirroring sklearn's `scale` and
    // `normalize(axis=0)`.
    let y = preprocess_covariates(covariates, opts.center_covariates, opts.scale_covariates);

    let alpha = match opts.alpha {
        Some(v) => {
            if !v.is_finite() || v < 0.0 {
                return Err(EmbedError::InvalidAlpha { value: v });
            }
            v
        }
        None => derive_alpha(l.as_ref(), y.as_ref(), opts.assortative)?,
    };

    let mixed = build_mixed(l.as_ref(), y.as_ref(), alpha, opts.assortative);
    let n_components = opts.n_components;

    // eigsh-equivalent via dense full eigendecomposition, then sort by |λ|.
    let decomp = linalg_eigh(mixed.as_ref(), n)?;
    let pairs: Vec<(f64, usize)> = decomp
        .eigenvalues
        .iter()
        .enumerate()
        .map(|(i, &v)| (v, i))
        .collect();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&i, &j| {
        pairs[j]
            .0
            .abs()
            .partial_cmp(&pairs[i].0.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let k = match n_components {
        Some(k) => k,
        None => zhu_ghodsi_pick(&decomp.eigenvalues, opts.n_elbows.max(1)),
    };
    if k == 0 || k > n {
        return Err(EmbedError::InvalidNComponents {
            n_components: k,
            min_shape: n,
        });
    }

    let mut s: Vec<f64> = (0..k).map(|i| decomp.eigenvalues[order[i]].abs()).collect();
    let mut u = Mat::<f64>::zeros(n, k);
    for (col, &src) in order.iter().take(k).enumerate() {
        for r in 0..n {
            u[(r, col)] = decomp.eigenvectors[(r, src)];
        }
    }

    // Sort top-k by descending singular value (already abs-sorted, but
    // numerical ties could be misordered).
    let mut idxs: Vec<usize> = (0..k).collect();
    idxs.sort_by(|&i, &j| s[j].partial_cmp(&s[i]).unwrap_or(std::cmp::Ordering::Equal));
    let mut sorted_u = Mat::<f64>::zeros(n, k);
    let mut sorted_s = vec![0.0; k];
    for (out_col, &src) in idxs.iter().enumerate() {
        sorted_s[out_col] = s[src];
        for r in 0..n {
            sorted_u[(r, out_col)] = u[(r, src)];
        }
    }
    s = sorted_s;
    let u = sorted_u;

    let sqrt_s: Vec<f64> = s.iter().map(|&v| v.max(0.0).sqrt()).collect();
    let mut latent_left = Mat::<f64>::zeros(n, k);
    for r in 0..n {
        for c in 0..k {
            latent_left[(r, c)] = u[(r, c)] * sqrt_s[c];
        }
    }

    Ok(CaseEmbedding {
        embedding: Embedding {
            latent_left,
            latent_right: None,
            singular_values: s,
            n_components: k,
        },
        alpha_used: alpha,
    })
}

fn preprocess_covariates(y: MatRef<'_, f64>, center: bool, scale: bool) -> Mat<f64> {
    let n = y.nrows();
    let d = y.ncols();
    let mut out = Mat::<f64>::zeros(n, d);
    for r in 0..n {
        for c in 0..d {
            out[(r, c)] = y[(r, c)];
        }
    }
    if center {
        for c in 0..d {
            let mut mean = 0.0;
            for r in 0..n {
                mean += out[(r, c)];
            }
            mean /= n as f64;
            for r in 0..n {
                out[(r, c)] -= mean;
            }
        }
    }
    if scale {
        for c in 0..d {
            let mut norm_sq = 0.0;
            for r in 0..n {
                norm_sq += out[(r, c)] * out[(r, c)];
            }
            let norm = norm_sq.sqrt();
            if norm > 0.0 {
                for r in 0..n {
                    out[(r, c)] /= norm;
                }
            }
        }
    }
    out
}

fn derive_alpha(l: MatRef<'_, f64>, y: MatRef<'_, f64>, assortative: bool) -> EmbedResult<f64> {
    let n = l.nrows();
    let l_eig = linalg_eigh(l, n)?;
    let mut l_top = 0.0_f64;
    for v in &l_eig.eigenvalues {
        if v.abs() > l_top {
            l_top = v.abs();
        }
    }
    // Mirror Python's `if assortative: L_top **= 2` — graspologic's
    // get_tuning_parameter squares the Laplacian's top eigenvalue when
    // the matrix to be embedded includes a single L (assortative case);
    // we follow the upstream convention even though the asymmetry is
    // unintuitive.
    if assortative {
        l_top *= l_top;
    }

    // Top eigenvalue of YYᵀ via SVD of Y: top singular value squared.
    let svd_y = linalg_svd(y, y.ncols().min(y.nrows()).max(1))?;
    let yy_top = svd_y.s.first().copied().unwrap_or(0.0).powi(2);

    if yy_top <= 0.0 {
        return Ok(0.0);
    }
    Ok(l_top / yy_top)
}

fn build_mixed(l: MatRef<'_, f64>, y: MatRef<'_, f64>, alpha: f64, assortative: bool) -> Mat<f64> {
    let n = l.nrows();
    let mut out = Mat::<f64>::zeros(n, n);

    // YYᵀ
    let mut yyt = Mat::<f64>::zeros(n, n);
    for r in 0..n {
        for c in 0..n {
            let mut acc = 0.0;
            for k in 0..y.ncols() {
                acc += y[(r, k)] * y[(c, k)];
            }
            yyt[(r, c)] = acc;
        }
    }

    if assortative {
        for r in 0..n {
            for c in 0..n {
                out[(r, c)] = l[(r, c)] + alpha * yyt[(r, c)];
            }
        }
    } else {
        // L*L
        let mut ll = Mat::<f64>::zeros(n, n);
        for r in 0..n {
            for c in 0..n {
                let mut acc = 0.0;
                for k in 0..n {
                    acc += l[(r, k)] * l[(k, c)];
                }
                ll[(r, c)] = acc;
            }
        }
        for r in 0..n {
            for c in 0..n {
                out[(r, c)] = ll[(r, c)] + alpha * yyt[(r, c)];
            }
        }
    }

    // Symmetrise to absorb any floating-point drift.
    for r in 0..n {
        for c in (r + 1)..n {
            let avg = (out[(r, c)] + out[(c, r)]) / 2.0;
            out[(r, c)] = avg;
            out[(c, r)] = avg;
        }
    }
    out
}

fn zhu_ghodsi_pick(eigenvalues: &[f64], n_elbows: usize) -> usize {
    let mut sorted: Vec<f64> = eigenvalues.iter().map(|v| v.abs()).collect();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let elbows = super::select::select_dimension(&sorted, n_elbows);
    elbows
        .last()
        .map(|(idx, _)| *idx)
        .unwrap_or(sorted.len())
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::Attrs;

    fn block_clique_graph() -> CommunicationGraph {
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
        g
    }

    #[test]
    fn case_runs_on_block_clique_with_block_indicator_covariates() {
        let g = block_clique_graph();

        // One column: 0 for first clique, 1 for second.
        let mut y = Mat::<f64>::zeros(6, 1);
        for r in 0..3 {
            y[(r, 0)] = 0.0;
        }
        for r in 3..6 {
            y[(r, 0)] = 1.0;
        }
        let emb = covariate_assisted_embed(
            &g,
            y.as_ref(),
            &CaseOptions {
                n_components: Some(2),
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(emb.embedding.latent_left.nrows(), 6);
        assert_eq!(emb.embedding.latent_left.ncols(), 2);
        assert!(emb.alpha_used.is_finite());
    }

    #[test]
    fn case_explicit_alpha_used_verbatim() {
        let g = block_clique_graph();
        let mut y = Mat::<f64>::zeros(6, 1);
        for r in 3..6 {
            y[(r, 0)] = 1.0;
        }
        let emb = covariate_assisted_embed(
            &g,
            y.as_ref(),
            &CaseOptions {
                n_components: Some(2),
                alpha: Some(0.42),
                ..Default::default()
            },
        )
        .expect("ok");
        assert!((emb.alpha_used - 0.42).abs() < 1e-15);
    }

    #[test]
    fn case_invalid_alpha_errors() {
        let g = block_clique_graph();
        let y = Mat::<f64>::zeros(6, 1);
        let result = covariate_assisted_embed(
            &g,
            y.as_ref(),
            &CaseOptions {
                n_components: Some(2),
                alpha: Some(-1.0),
                ..Default::default()
            },
        );
        assert!(matches!(result, Err(EmbedError::InvalidAlpha { .. })));
    }

    #[test]
    fn case_covariate_mismatch_errors() {
        let g = block_clique_graph();
        let y = Mat::<f64>::zeros(5, 1);
        let result = covariate_assisted_embed(&g, y.as_ref(), &CaseOptions::default());
        assert!(matches!(result, Err(EmbedError::CovariateMismatch { .. })));
    }

    #[test]
    fn case_non_assortative_path_runs() {
        let g = block_clique_graph();
        let mut y = Mat::<f64>::zeros(6, 2);
        for r in 0..3 {
            y[(r, 0)] = 1.0;
        }
        for r in 3..6 {
            y[(r, 1)] = 1.0;
        }
        let emb = covariate_assisted_embed(
            &g,
            y.as_ref(),
            &CaseOptions {
                n_components: Some(2),
                assortative: false,
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(emb.embedding.n_components, 2);
    }

    #[test]
    fn case_singular_values_descending() {
        let g = block_clique_graph();
        let mut y = Mat::<f64>::zeros(6, 1);
        for r in 3..6 {
            y[(r, 0)] = 1.0;
        }
        let emb = covariate_assisted_embed(
            &g,
            y.as_ref(),
            &CaseOptions {
                n_components: Some(3),
                ..Default::default()
            },
        )
        .expect("ok");
        for w in emb.embedding.singular_values.windows(2) {
            assert!(w[0] >= w[1] - 1e-10, "expected descending: {:?}", w);
        }
    }
}
