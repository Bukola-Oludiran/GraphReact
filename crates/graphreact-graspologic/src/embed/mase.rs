//! Multiple Adjacency Spectral Embedding (MASE).
//!
//! Two-stage joint embedding of graphs sharing a vertex set:
//!
//! 1. Per-graph SVD with shared rank `d`. Optionally scale the columns
//!    by `sqrt(Σ)` (`scaled = true`).
//! 2. Stack the per-graph `U`s horizontally and SVD the stack to obtain
//!    a shared latent matrix `Û`. (Same for `V` when graphs are
//!    asymmetric.)
//!
//! Each input graph also gets a score matrix `R_i = Û^T A_i V̂`
//! (or `Û^T A_i Û` when undirected), which captures the graph-specific
//! signal in the shared latent space.

use faer::Mat;

use super::error::{EmbedError, EmbedResult};
use super::matrix::{augment_diagonal_in_place, is_almost_symmetric};
use super::select::{select_dimension, select_svd, SelectSvd};

#[derive(Debug, Clone)]
pub struct MaseOptions {
    pub n_components: Option<usize>,
    pub n_elbows: usize,
    /// When true, scale per-graph singular vectors by `sqrt(Σ)` before
    /// the second-stage stacking. Equivalent to ASE-then-stack.
    pub scaled: bool,
    pub diag_aug: bool,
}

impl Default for MaseOptions {
    fn default() -> Self {
        Self {
            n_components: None,
            n_elbows: 2,
            scaled: true,
            diag_aug: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MaseEmbedding {
    pub latent_left: Mat<f64>,
    pub latent_right: Option<Mat<f64>>,
    /// One score matrix per input graph. Shape `(k_left × k_right)` —
    /// `k_right == k_left` when the inputs are all symmetric.
    pub scores: Vec<Mat<f64>>,
    pub singular_values_left: Vec<f64>,
    pub singular_values_right: Option<Vec<f64>>,
    pub n_components_left: usize,
    pub n_components_right: usize,
}

pub fn multiple_adjacency_spectral_embed(
    matrices: &[Mat<f64>],
    opts: &MaseOptions,
) -> EmbedResult<MaseEmbedding> {
    if matrices.len() < 2 {
        return Err(EmbedError::TooFewGraphs { n: matrices.len() });
    }
    let n = matrices[0].nrows();
    if matrices[0].ncols() != n {
        return Err(EmbedError::NotSquare {
            rows: n,
            cols: matrices[0].ncols(),
        });
    }
    for mat in matrices.iter().skip(1) {
        if mat.nrows() != n || mat.ncols() != n {
            return Err(EmbedError::ShapeMismatch {
                rows0: n,
                cols0: n,
                rows: mat.nrows(),
                cols: mat.ncols(),
            });
        }
    }

    let mut prepped: Vec<Mat<f64>> = matrices.iter().map(faer::Mat::to_owned).collect();
    if opts.diag_aug {
        for m in &mut prepped {
            augment_diagonal_in_place(m);
        }
    }

    let undirected = prepped
        .iter()
        .all(|a| is_almost_symmetric(a.as_ref(), 1e-15));

    // Stage 1: per-graph SVDs.
    let initial_components = match opts.n_components {
        Some(k) => k,
        None => ((n as f64).log2().ceil() as usize).max(1),
    };
    let mut per_graph_svds: Vec<SelectSvd> = Vec::with_capacity(prepped.len());
    for m in &prepped {
        per_graph_svds.push(select_svd(
            m.as_ref(),
            Some(initial_components),
            opts.n_elbows,
        )?);
    }

    let best_dim = match opts.n_components {
        Some(k) => k,
        None => {
            let mut max_elbow = 0_usize;
            for s in &per_graph_svds {
                let elbows = select_dimension(&s.s, opts.n_elbows.max(1));
                if let Some((idx, _)) = elbows.last() {
                    if *idx > max_elbow {
                        max_elbow = *idx;
                    }
                }
            }
            max_elbow.max(1)
        }
    };

    // Build stacked Us (and Vs when at least one input is asymmetric).
    let total_cols = best_dim * matrices.len();
    let mut stacked_u = Mat::<f64>::zeros(n, total_cols);
    let mut stacked_v = Mat::<f64>::zeros(n, total_cols);
    let scaled = opts.scaled;
    for (i, svd) in per_graph_svds.iter().enumerate() {
        let off = i * best_dim;
        for r in 0..n {
            for c in 0..best_dim {
                let scale = if scaled {
                    svd.s[c].max(0.0).sqrt()
                } else {
                    1.0
                };
                stacked_u[(r, off + c)] = svd.u[(r, c)] * scale;
                // V[r][c] = vt[c][r]
                stacked_v[(r, off + c)] = svd.vt[(c, r)] * scale;
            }
        }
    }

    // Stage 2: SVD of stacked Us → shared left latent.
    let svd_u = select_svd(stacked_u.as_ref(), opts.n_components, opts.n_elbows)?;
    let mut latent_left = Mat::<f64>::zeros(n, svd_u.n_components);
    for r in 0..n {
        for c in 0..svd_u.n_components {
            latent_left[(r, c)] = svd_u.u[(r, c)];
        }
    }
    let n_left = svd_u.n_components;
    let sing_left = svd_u.s.clone();

    let (latent_right_opt, sing_right_opt, n_right) = if undirected {
        (None, None, n_left)
    } else {
        let svd_v = select_svd(stacked_v.as_ref(), opts.n_components, opts.n_elbows)?;
        let mut latent_right = Mat::<f64>::zeros(n, svd_v.n_components);
        for r in 0..n {
            for c in 0..svd_v.n_components {
                latent_right[(r, c)] = svd_v.u[(r, c)];
            }
        }
        let n_right = svd_v.n_components;
        (Some(latent_right), Some(svd_v.s), n_right)
    };

    let scores: Vec<Mat<f64>> = prepped
        .iter()
        .map(|a| compute_score(&latent_left, latent_right_opt.as_ref(), a, n_left, n_right))
        .collect();

    Ok(MaseEmbedding {
        latent_left,
        latent_right: latent_right_opt,
        scores,
        singular_values_left: sing_left,
        singular_values_right: sing_right_opt,
        n_components_left: n_left,
        n_components_right: n_right,
    })
}

fn compute_score(
    u: &Mat<f64>,
    v: Option<&Mat<f64>>,
    a: &Mat<f64>,
    n_left: usize,
    n_right: usize,
) -> Mat<f64> {
    let n = a.nrows();
    let v_for = v.unwrap_or(u);
    // tmp = U^T @ A: (n_left × n)
    let mut tmp = Mat::<f64>::zeros(n_left, n);
    for r in 0..n_left {
        for c in 0..n {
            let mut acc = 0.0;
            for m in 0..n {
                acc += u[(m, r)] * a[(m, c)];
            }
            tmp[(r, c)] = acc;
        }
    }
    // out = tmp @ V: (n_left × n_right)
    let mut out = Mat::<f64>::zeros(n_left, n_right);
    for r in 0..n_left {
        for c in 0..n_right {
            let mut acc = 0.0;
            for m in 0..n {
                acc += tmp[(r, m)] * v_for[(m, c)];
            }
            out[(r, c)] = acc;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_matrix(n: usize) -> Mat<f64> {
        let mut a = Mat::<f64>::zeros(n, n);
        for i in 0..n {
            let j = (i + 1) % n;
            a[(i, j)] = 1.0;
            a[(j, i)] = 1.0;
        }
        a
    }

    #[test]
    fn mase_returns_expected_shapes() {
        let a = ring_matrix(6);
        let b = ring_matrix(6);
        let emb = multiple_adjacency_spectral_embed(
            &[a, b],
            &MaseOptions {
                n_components: Some(2),
                diag_aug: false,
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(emb.latent_left.nrows(), 6);
        assert_eq!(emb.latent_left.ncols(), 2);
        assert!(emb.latent_right.is_none());
        assert_eq!(emb.scores.len(), 2);
        for s in &emb.scores {
            assert_eq!(s.nrows(), 2);
            assert_eq!(s.ncols(), 2);
        }
    }

    #[test]
    fn mase_latent_columns_orthonormal() {
        // The latent_left is the U from stage-2 SVD — its columns are
        // orthonormal by construction.
        let a = ring_matrix(8);
        let b = ring_matrix(8);
        let emb = multiple_adjacency_spectral_embed(
            &[a, b],
            &MaseOptions {
                n_components: Some(3),
                diag_aug: false,
                ..Default::default()
            },
        )
        .expect("ok");

        let k = emb.n_components_left;
        // U^T U = I_k.
        for i in 0..k {
            for j in 0..k {
                let mut dot = 0.0;
                for r in 0..emb.latent_left.nrows() {
                    dot += emb.latent_left[(r, i)] * emb.latent_left[(r, j)];
                }
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!((dot - expected).abs() < 1e-10, "U^T U[{i}][{j}] = {dot}");
            }
        }
    }

    #[test]
    fn mase_rejects_too_few_graphs() {
        let a = ring_matrix(4);
        let result = multiple_adjacency_spectral_embed(&[a], &MaseOptions::default());
        assert!(matches!(result, Err(EmbedError::TooFewGraphs { n: 1 })));
    }

    #[test]
    fn mase_unscaled_path_runs() {
        let a = ring_matrix(5);
        let b = ring_matrix(5);
        let emb = multiple_adjacency_spectral_embed(
            &[a, b],
            &MaseOptions {
                n_components: Some(2),
                scaled: false,
                diag_aug: false,
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(emb.latent_left.ncols(), 2);
    }

    #[test]
    fn mase_scores_close_to_zero_when_inputs_are_zero() {
        let zero = Mat::<f64>::zeros(4, 4);
        // Need at least 2 non-degenerate graphs. Construct a tiny SBM-ish.
        let mut a = Mat::<f64>::zeros(4, 4);
        a[(0, 1)] = 1.0;
        a[(1, 0)] = 1.0;
        a[(2, 3)] = 1.0;
        a[(3, 2)] = 1.0;
        let emb = multiple_adjacency_spectral_embed(
            &[a, zero],
            &MaseOptions {
                n_components: Some(2),
                diag_aug: false,
                ..Default::default()
            },
        )
        .expect("ok");
        // Score of the zero matrix must itself be (very close to) zero.
        let s = &emb.scores[1];
        for r in 0..s.nrows() {
            for c in 0..s.ncols() {
                assert!(s[(r, c)].abs() < 1e-10);
            }
        }
    }
}
