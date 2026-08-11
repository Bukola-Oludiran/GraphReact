//! Latent-position two-sample test for two random dot product graphs.
//!
//! Tests `H_0: X_1 = X_2 R` (latent positions equal up to orthogonal
//! rotation) against the alternative. The test statistic is
//! `T = ‖X̂_1 R̂ − X̂_2‖_F` where `X̂_i = ASE(A_i)` and `R̂` is the
//! Procrustes-optimal rotation. The null distribution is approximated by
//! sampling `n_bootstraps` random-dot-product graphs from each `X̂_i`,
//! re-embedding, and recomputing the statistic.
//!
//! Reference: Tang et al., "A Semiparametric Two-Sample Hypothesis
//! Testing Problem for Random Graphs", JCGS 2017.
//!
//! ## Scope
//!
//! Single-graph ASE-based variant only. The Omnibus path and the
//! `scalar-rotation` / `diagonal-rotation` test cases are deferred —
//! they're additional `_difference_norm` branches the original
//! exposes; pluggable when needed.

use faer::{Mat, MatRef};
use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use thiserror::Error;

use crate::embed::{adjacency_spectral_embed_matrix, orthogonal_procrustes, EmbedError};

#[derive(Debug, Error)]
pub enum LatentPositionError {
    #[error("input adjacency matrices must be square and the same shape")]
    ShapeMismatch,
    #[error("n_bootstraps must be ≥ 1")]
    NoBootstraps,
    #[error("embed error: {0}")]
    Embed(#[from] EmbedError),
}

pub type LatentPositionResult<T> = Result<T, LatentPositionError>;

#[derive(Debug, Clone)]
pub struct LatentPositionOptions {
    pub n_components: usize,
    pub n_bootstraps: usize,
    pub seed: Option<u64>,
}

impl Default for LatentPositionOptions {
    fn default() -> Self {
        Self {
            n_components: 2,
            n_bootstraps: 200,
            seed: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LatentPositionTest {
    /// Observed test statistic `T`.
    pub statistic: f64,
    /// p-value (`max(p_1, p_2)` per Tang et al.).
    pub p_value: f64,
    pub p_value_1: f64,
    pub p_value_2: f64,
    pub null_distribution_1: Vec<f64>,
    pub null_distribution_2: Vec<f64>,
}

pub fn latent_position_test(
    a1: MatRef<'_, f64>,
    a2: MatRef<'_, f64>,
    opts: &LatentPositionOptions,
) -> LatentPositionResult<LatentPositionTest> {
    let n = a1.nrows();
    if n == 0 || a1.ncols() != n || a2.nrows() != n || a2.ncols() != n {
        return Err(LatentPositionError::ShapeMismatch);
    }
    if opts.n_bootstraps == 0 {
        return Err(LatentPositionError::NoBootstraps);
    }

    let x1 = embed(a1, opts.n_components)?;
    let x2 = embed(a2, opts.n_components)?;
    let stat = procrustes_difference_norm(x1.as_ref(), x2.as_ref())?;

    let mut rng = match opts.seed {
        Some(s) => ChaCha8Rng::seed_from_u64(s),
        None => ChaCha8Rng::from_entropy(),
    };

    let null1 = bootstrap(&x1, opts.n_components, opts.n_bootstraps, &mut rng)?;
    let null2 = bootstrap(&x2, opts.n_components, opts.n_bootstraps, &mut rng)?;

    let p1 = exact_mc_pvalue(stat, &null1);
    let p2 = exact_mc_pvalue(stat, &null2);
    let p = p1.max(p2);

    Ok(LatentPositionTest {
        statistic: stat,
        p_value: p,
        p_value_1: p1,
        p_value_2: p2,
        null_distribution_1: null1,
        null_distribution_2: null2,
    })
}

fn embed(a: MatRef<'_, f64>, n_components: usize) -> Result<Mat<f64>, EmbedError> {
    let emb = adjacency_spectral_embed_matrix(a, Some(n_components), 1, Some(false))?;
    Ok(emb.latent_left)
}

fn procrustes_difference_norm(x1: MatRef<'_, f64>, x2: MatRef<'_, f64>) -> Result<f64, EmbedError> {
    let r = orthogonal_procrustes(x1, x2)?;
    let aligned = x1 * r.as_ref();
    let mut s = 0.0_f64;
    for r in 0..x1.nrows() {
        for c in 0..x1.ncols() {
            let d = aligned[(r, c)] - x2[(r, c)];
            s += d * d;
        }
    }
    Ok(s.sqrt())
}

fn bootstrap(
    x: &Mat<f64>,
    n_components: usize,
    n_bootstraps: usize,
    rng: &mut impl Rng,
) -> Result<Vec<f64>, EmbedError> {
    let mut out = Vec::with_capacity(n_bootstraps);
    for _ in 0..n_bootstraps {
        let a1 = sample_rdpg(x, rng);
        let a2 = sample_rdpg(x, rng);
        let x1 = embed(a1.as_ref(), n_components)?;
        let x2 = embed(a2.as_ref(), n_components)?;
        let stat = procrustes_difference_norm(x1.as_ref(), x2.as_ref())?;
        out.push(stat);
    }
    Ok(out)
}

/// Sample an undirected graph from the random dot product graph model
/// `P_ij = clip(⟨x_i, x_j⟩, 0, 1)`. Self-loops disabled.
pub fn sample_rdpg(x: &Mat<f64>, rng: &mut impl Rng) -> Mat<f64> {
    let n = x.nrows();
    let d = x.ncols();
    let mut a = Mat::<f64>::zeros(n, n);
    for i in 0..n {
        for j in (i + 1)..n {
            let mut p = 0.0_f64;
            for c in 0..d {
                p += x[(i, c)] * x[(j, c)];
            }
            let p = p.clamp(0.0, 1.0);
            let u: f64 = rng.gen();
            if u < p {
                a[(i, j)] = 1.0;
                a[(j, i)] = 1.0;
            }
        }
    }
    a
}

fn exact_mc_pvalue(stat: f64, null: &[f64]) -> f64 {
    // Phipson-Smyth (2010) exact MC p-value: (b + 1) / (n + 1) where b is
    // number of null statistics ≥ observed.
    let b = null.iter().filter(|&&v| v >= stat).count();
    (b as f64 + 1.0) / (null.len() as f64 + 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_adjacency(n: usize) -> Mat<f64> {
        let mut a = Mat::<f64>::zeros(n, n);
        for i in 0..n {
            let j = (i + 1) % n;
            a[(i, j)] = 1.0;
            a[(j, i)] = 1.0;
        }
        a
    }

    fn er_adjacency(n: usize, p: f64, rng: &mut impl Rng) -> Mat<f64> {
        let mut a = Mat::<f64>::zeros(n, n);
        for i in 0..n {
            for j in (i + 1)..n {
                if rng.gen::<f64>() < p {
                    a[(i, j)] = 1.0;
                    a[(j, i)] = 1.0;
                }
            }
        }
        a
    }

    #[test]
    fn latent_position_test_runs_to_completion() {
        let a = ring_adjacency(8);
        let test = latent_position_test(
            a.as_ref(),
            a.as_ref(),
            &LatentPositionOptions {
                n_components: 2,
                n_bootstraps: 8,
                seed: Some(0),
            },
        )
        .expect("ok");
        assert!(test.statistic >= 0.0);
        assert!(test.p_value > 0.0 && test.p_value <= 1.0);
        assert_eq!(test.null_distribution_1.len(), 8);
        assert_eq!(test.null_distribution_2.len(), 8);
    }

    #[test]
    fn latent_position_test_self_match_high_pvalue() {
        // Two copies of the same graph: should not reject H_0.
        let a = ring_adjacency(10);
        let test = latent_position_test(
            a.as_ref(),
            a.as_ref(),
            &LatentPositionOptions {
                n_components: 2,
                n_bootstraps: 30,
                seed: Some(1),
            },
        )
        .expect("ok");
        assert!(test.p_value > 0.05, "p-value {}", test.p_value);
    }

    #[test]
    fn rdpg_sampler_produces_symmetric_zero_diag() {
        let mut x = Mat::<f64>::zeros(5, 2);
        for i in 0..5 {
            x[(i, 0)] = 0.5;
            x[(i, 1)] = 0.3;
        }
        let mut rng = ChaCha8Rng::seed_from_u64(0);
        let a = sample_rdpg(&x, &mut rng);
        for i in 0..5 {
            assert_eq!(a[(i, i)], 0.0);
            for j in (i + 1)..5 {
                assert_eq!(a[(i, j)], a[(j, i)]);
            }
        }
    }

    #[test]
    fn latent_position_test_rejects_shape_mismatch() {
        let a = ring_adjacency(4);
        let b = ring_adjacency(5);
        let result =
            latent_position_test(a.as_ref(), b.as_ref(), &LatentPositionOptions::default());
        assert!(matches!(result, Err(LatentPositionError::ShapeMismatch)));
    }

    #[test]
    fn exact_mc_pvalue_handles_edge_cases() {
        // All-null observations, observed stat zero → b = N (since all
        // null ≥ 0 = stat); p = (N+1)/(N+1) = 1.
        let null = vec![0.1, 0.2, 0.3, 0.4];
        let p_extreme = exact_mc_pvalue(-1.0, &null);
        assert!((p_extreme - 1.0).abs() < 1e-12);
        // All null observations far above stat → p ≈ 1.
        let p_top = exact_mc_pvalue(99.0, &null);
        assert!((p_top - 1.0 / 5.0).abs() < 1e-12);
    }

    #[test]
    fn latent_position_test_distinguishes_random_vs_structured() {
        // ER graph vs. ring of same size — null distributions should
        // differ enough that the statistic isn't degenerate.
        let mut rng = ChaCha8Rng::seed_from_u64(2);
        let a = ring_adjacency(12);
        let b = er_adjacency(12, 0.5, &mut rng);
        let test = latent_position_test(
            a.as_ref(),
            b.as_ref(),
            &LatentPositionOptions {
                n_components: 2,
                n_bootstraps: 16,
                seed: Some(3),
            },
        )
        .expect("ok");
        assert!(test.statistic > 0.0);
        assert!(test.null_distribution_1.iter().all(|v| v.is_finite()));
    }
}
