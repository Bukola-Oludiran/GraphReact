//! Omnibus (OMNI) embedding — joint embedding of multiple graphs that
//! share a vertex set.
//!
//! Given `m` adjacency matrices `A_1, …, A_m` over the same `n` vertices,
//! build the `(mn × mn)` block matrix
//!
//! ```text
//! M_{ij} =        A_i              when i == j
//!          (A_i + A_j) / 2          otherwise
//! ```
//!
//! then run ASE on `M`. The latent matrix splits row-wise into one
//! `(n × k)` block per input graph.
//!
//! Reference: Levin et al., "A central limit theorem for an omnibus
//! embedding of multiple random dot product graphs", ICDMW 2017.

use faer::Mat;

use super::ase::{adjacency_spectral_embed_matrix, Embedding};
use super::error::{EmbedError, EmbedResult};
use super::matrix::{
    augment_diagonal_in_place, is_almost_symmetric, to_laplacian, LaplacianForm,
};

#[derive(Debug, Clone)]
pub struct OmniOptions {
    pub n_components: Option<usize>,
    pub n_elbows: usize,
    pub diag_aug: bool,
    /// If true, replace each input adjacency with its Laplacian before
    /// stacking — corresponds to graspologic's `lse=True`.
    pub use_lse: bool,
    pub laplacian_form: LaplacianForm,
}

impl Default for OmniOptions {
    fn default() -> Self {
        Self {
            n_components: None,
            n_elbows: 2,
            diag_aug: true,
            use_lse: false,
            laplacian_form: LaplacianForm::Dad,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OmniEmbedding {
    /// `m` matrices each of shape `(n, k)` — left latent positions per input graph.
    pub per_graph_left: Vec<Mat<f64>>,
    /// Populated only when the omnibus matrix is asymmetric.
    pub per_graph_right: Option<Vec<Mat<f64>>>,
    pub singular_values: Vec<f64>,
    pub n_components: usize,
}

/// OMNI on a slice of pre-built adjacency matrices.
pub fn omnibus_embed(
    matrices: &[Mat<f64>],
    opts: &OmniOptions,
) -> EmbedResult<OmniEmbedding> {
    if matrices.len() < 2 {
        return Err(EmbedError::TooFewGraphs {
            n: matrices.len(),
        });
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
    if opts.use_lse {
        let mut laplacians = Vec::with_capacity(prepped.len());
        for m in &prepped {
            laplacians.push(to_laplacian(m.as_ref(), opts.laplacian_form, None)?);
        }
        prepped = laplacians;
    }

    let m = prepped.len();
    let big = m * n;
    let mut omni = Mat::<f64>::zeros(big, big);
    for i in 0..m {
        for j in 0..m {
            for r in 0..n {
                for c in 0..n {
                    let v = if i == j {
                        prepped[i][(r, c)]
                    } else {
                        (prepped[i][(r, c)] + prepped[j][(r, c)]) / 2.0
                    };
                    omni[(i * n + r, j * n + c)] = v;
                }
            }
        }
    }

    let directed_hint = !is_almost_symmetric(omni.as_ref(), 1e-15);
    let emb = adjacency_spectral_embed_matrix(
        omni.as_ref(),
        opts.n_components,
        opts.n_elbows,
        Some(directed_hint),
    )?;

    let k = emb.n_components;
    let per_graph_left = split_per_graph(&emb.latent_left, m, n, k);
    let per_graph_right = emb
        .latent_right
        .as_ref()
        .map(|right| split_per_graph(right, m, n, k));

    Ok(OmniEmbedding {
        per_graph_left,
        per_graph_right,
        singular_values: emb.singular_values,
        n_components: k,
    })
}

fn split_per_graph(latent: &Mat<f64>, m: usize, n: usize, k: usize) -> Vec<Mat<f64>> {
    let mut out = Vec::with_capacity(m);
    for i in 0..m {
        let mut slc = Mat::<f64>::zeros(n, k);
        for r in 0..n {
            for c in 0..k {
                slc[(r, c)] = latent[(i * n + r, c)];
            }
        }
        out.push(slc);
    }
    out
}

/// Lower-level helper: the fitted [`Embedding`] over the raw `(mn × mn)`
/// omnibus matrix. Useful when callers want full access to the joint
/// embedding rather than the per-graph slices.
pub fn omnibus_embed_full(
    matrices: &[Mat<f64>],
    opts: &OmniOptions,
) -> EmbedResult<Embedding> {
    let omni_emb = omnibus_embed(matrices, opts)?;
    let m = omni_emb.per_graph_left.len();
    let n = omni_emb.per_graph_left[0].nrows();
    let k = omni_emb.n_components;
    let mut latent_left = Mat::<f64>::zeros(m * n, k);
    for (i, slc) in omni_emb.per_graph_left.iter().enumerate() {
        for r in 0..n {
            for c in 0..k {
                latent_left[(i * n + r, c)] = slc[(r, c)];
            }
        }
    }
    let latent_right = omni_emb.per_graph_right.as_ref().map(|right| {
        let mut lr = Mat::<f64>::zeros(m * n, k);
        for (i, slc) in right.iter().enumerate() {
            for r in 0..n {
                for c in 0..k {
                    lr[(i * n + r, c)] = slc[(r, c)];
                }
            }
        }
        lr
    });
    Ok(Embedding {
        latent_left,
        latent_right,
        singular_values: omni_emb.singular_values,
        n_components: k,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle_matrix() -> Mat<f64> {
        let mut a = Mat::<f64>::zeros(3, 3);
        for (i, j) in [(0, 1), (1, 2), (0, 2)] {
            a[(i, j)] = 1.0;
            a[(j, i)] = 1.0;
        }
        a
    }

    #[test]
    fn omni_returns_one_block_per_input_graph() {
        let a = triangle_matrix();
        let b = triangle_matrix();
        let emb = omnibus_embed(
            &[a, b],
            &OmniOptions {
                n_components: Some(2),
                diag_aug: false,
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(emb.per_graph_left.len(), 2);
        for slc in &emb.per_graph_left {
            assert_eq!(slc.nrows(), 3);
            assert_eq!(slc.ncols(), 2);
        }
    }

    #[test]
    fn omni_identical_inputs_have_identical_per_graph_blocks() {
        // When all inputs are the same, the omnibus matrix's row blocks
        // are identical, so each per-graph latent block matches.
        let a = triangle_matrix();
        let emb = omnibus_embed(
            &[a.clone(), a.clone()],
            &OmniOptions {
                n_components: Some(2),
                diag_aug: false,
                ..Default::default()
            },
        )
        .expect("ok");

        let lhs = &emb.per_graph_left[0];
        let rhs = &emb.per_graph_left[1];
        for r in 0..3 {
            for c in 0..2 {
                assert!(
                    (lhs[(r, c)] - rhs[(r, c)]).abs() < 1e-10,
                    "[{r},{c}] lhs={} rhs={}",
                    lhs[(r, c)],
                    rhs[(r, c)]
                );
            }
        }
    }

    #[test]
    fn omni_rejects_single_graph() {
        let a = triangle_matrix();
        let result = omnibus_embed(&[a], &OmniOptions::default());
        assert!(matches!(result, Err(EmbedError::TooFewGraphs { n: 1 })));
    }

    #[test]
    fn omni_rejects_shape_mismatch() {
        let a = triangle_matrix();
        let b = Mat::<f64>::zeros(4, 4);
        let result = omnibus_embed(&[a, b], &OmniOptions::default());
        assert!(matches!(result, Err(EmbedError::ShapeMismatch { .. })));
    }

    #[test]
    fn omni_with_lse_runs_on_laplacians() {
        let a = triangle_matrix();
        let b = triangle_matrix();
        let emb = omnibus_embed(
            &[a, b],
            &OmniOptions {
                n_components: Some(2),
                diag_aug: false,
                use_lse: true,
                laplacian_form: LaplacianForm::Dad,
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(emb.per_graph_left.len(), 2);
        // Triangle DAD has eigenvalues {1, -1/2, -1/2} → singular values
        // {1, 1/2, 1/2}. Top-2 of omni should pick up dominant copies.
        assert!(emb.singular_values[0] > emb.singular_values[1]);
    }
}
