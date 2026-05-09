//! SVD selection helpers shared across the embedding routines.
//!
//! [`select_dimension`] implements Zhu & Ghodsi's profile-likelihood
//! elbow detection on a sorted-descending series of singular values
//! (Zhu & Ghodsi, 2006).
//!
//! [`select_svd`] runs full SVD on the input matrix (delegating to
//! [`graphreact_linalg::svd`]) and truncates to either an explicit
//! `n_components` or the elbow chosen by [`select_dimension`].

use faer::{Mat, MatRef};

use graphreact_linalg::{eigh as linalg_eigh, svd as linalg_svd};

use super::error::{EmbedError, EmbedResult};
use super::matrix::is_almost_symmetric;

const LOG_2PI: f64 = 1.837_877_066_409_345_5_f64;

/// Truncated SVD result. `u` has shape `(rows, n_components)`, `s` has
/// length `n_components` (descending), and `vt` has shape
/// `(n_components, cols)`.
#[derive(Debug, Clone)]
pub struct SelectSvd {
    pub u: Mat<f64>,
    pub s: Vec<f64>,
    pub vt: Mat<f64>,
    pub n_components: usize,
}

/// Zhu & Ghodsi profile-likelihood elbow detection.
///
/// `values` must be sorted in descending order. Returns up to
/// `n_elbows` `(elbow_idx, value_at_elbow)` pairs, where `elbow_idx`
/// is the 1-indexed dimension count to keep at that elbow. Stops
/// early when the remaining tail is too short to score.
pub fn select_dimension(values: &[f64], n_elbows: usize) -> Vec<(usize, f64)> {
    if n_elbows == 0 || values.len() < 2 {
        return Vec::new();
    }
    let mut elbows = Vec::with_capacity(n_elbows);
    let mut idx = 0_usize;
    for _ in 0..n_elbows {
        let arr = &values[idx..];
        if arr.len() <= 1 {
            break;
        }
        let likelihoods = compute_likelihoods(arr);
        let (best, best_val) = likelihoods.iter().enumerate().fold(
            (0_usize, f64::NEG_INFINITY),
            |(bi, bv), (i, &v)| if v > bv { (i, v) } else { (bi, bv) },
        );
        if best_val.is_infinite() && best_val < 0.0 {
            break;
        }
        idx += best + 1;
        elbows.push((idx, values[idx - 1]));
    }
    elbows
}

fn log_pdf(x: f64, mu: f64, sigma: f64) -> f64 {
    let z = (x - mu) / sigma;
    -0.5 * LOG_2PI - sigma.ln() - 0.5 * z * z
}

fn compute_likelihoods(arr: &[f64]) -> Vec<f64> {
    let n = arr.len();
    let mut out = vec![f64::NEG_INFINITY; n];
    for split in 1..=n {
        let s1 = &arr[..split];
        let s2 = &arr[split..];
        if s1.len() == 1 && s2.len() == 1 {
            // Matches the Python guard: variance computed on two
            // single-element halves is degenerate.
            out[split - 1] = f64::NEG_INFINITY;
            continue;
        }
        let mu1 = s1.iter().sum::<f64>() / s1.len() as f64;
        let mu2 = if s2.is_empty() {
            0.0
        } else {
            s2.iter().sum::<f64>() / s2.len() as f64
        };
        let denom = (n as f64) - 1.0 - if split < n { 1.0 } else { 0.0 };
        if denom <= 0.0 {
            continue;
        }
        let var1: f64 = s1.iter().map(|&x| (x - mu1).powi(2)).sum();
        let var2: f64 = if s2.is_empty() {
            0.0
        } else {
            s2.iter().map(|&x| (x - mu2).powi(2)).sum()
        };
        let variance = (var1 + var2) / denom;
        if variance <= 0.0 {
            continue;
        }
        let sigma = variance.sqrt();
        let mut ll: f64 = s1.iter().map(|&x| log_pdf(x, mu1, sigma)).sum();
        if !s2.is_empty() {
            ll += s2.iter().map(|&x| log_pdf(x, mu2, sigma)).sum::<f64>();
        }
        out[split - 1] = ll;
    }
    out
}

/// Run SVD on `a` and truncate to `n_components` (or the
/// `n_elbows`-th elbow when `n_components` is `None`).
///
/// The underlying SVD is the full faer-backed decomposition from
/// [`graphreact_linalg`]. For "feature parity ≠ line-by-line": Python
/// branches between scipy.linalg / scipy.sparse.linalg / sklearn's
/// randomized SVD by `algorithm`; the Rust port runs the same SVD
/// kernel for every algorithm flag (graspologic's algorithm choice is
/// a numerical-method preference, not an output-shape divergence —
/// truncated singular triplets are unique up to sign).
pub fn select_svd(
    a: MatRef<'_, f64>,
    n_components: Option<usize>,
    n_elbows: usize,
) -> EmbedResult<SelectSvd> {
    let rows = a.nrows();
    let cols = a.ncols();
    let min_shape = rows.min(cols);
    if min_shape == 0 {
        return Err(EmbedError::Empty);
    }

    // Symmetric matrices route through eigendecomposition. faer 0.20's
    // dense SVD has correctness regressions on certain rank-deficient
    // symmetric inputs (observed during M9 development on the 6×6 OMNI
    // matrix of two triangle graphs — reconstruction error ~0.7); the
    // symmetric eigensolver is unaffected. For symmetric A,
    // M = Q Λ Qᵀ trivially yields a valid SVD with σ = |λ| and a
    // sign-corrected V.
    if rows == cols && is_almost_symmetric(a, 1e-12) {
        return select_svd_symmetric(a, n_components, n_elbows);
    }

    let full = linalg_svd(a, min_shape)?;
    let k = match n_components {
        Some(k) => k,
        None => {
            let elbows = select_dimension(&full.s, n_elbows.max(1));
            elbows.last().map(|(idx, _)| *idx).unwrap_or(min_shape).max(1)
        }
    };

    if k == 0 || k > min_shape {
        return Err(EmbedError::InvalidNComponents {
            n_components: k,
            min_shape,
        });
    }

    let mut u = Mat::<f64>::zeros(rows, k);
    for r in 0..rows {
        for c in 0..k {
            u[(r, c)] = full.u[(r, c)];
        }
    }
    let s: Vec<f64> = full.s[..k].to_vec();
    let mut vt = Mat::<f64>::zeros(k, cols);
    for r in 0..k {
        for c in 0..cols {
            vt[(r, c)] = full.vt[(r, c)];
        }
    }
    Ok(SelectSvd {
        u,
        s,
        vt,
        n_components: k,
    })
}

fn select_svd_symmetric(
    a: MatRef<'_, f64>,
    n_components: Option<usize>,
    n_elbows: usize,
) -> EmbedResult<SelectSvd> {
    let n = a.nrows();
    let decomp = linalg_eigh(a, n)?;

    // Sort indices by |λ| descending.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&i, &j| {
        decomp.eigenvalues[j]
            .abs()
            .partial_cmp(&decomp.eigenvalues[i].abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let s_full: Vec<f64> = order
        .iter()
        .map(|&i| decomp.eigenvalues[i].abs())
        .collect();

    let k = match n_components {
        Some(k) => k,
        None => {
            let elbows = select_dimension(&s_full, n_elbows.max(1));
            elbows.last().map(|(idx, _)| *idx).unwrap_or(n).max(1)
        }
    };
    if k == 0 || k > n {
        return Err(EmbedError::InvalidNComponents {
            n_components: k,
            min_shape: n,
        });
    }

    let mut u = Mat::<f64>::zeros(n, k);
    let mut vt = Mat::<f64>::zeros(k, n);
    for (out_col, &src) in order.iter().take(k).enumerate() {
        let lambda = decomp.eigenvalues[src];
        let sign = if lambda >= 0.0 { 1.0 } else { -1.0 };
        for r in 0..n {
            let q = decomp.eigenvectors[(r, src)];
            u[(r, out_col)] = q;
            // M = U Σ Vᵀ with σ = |λ|, V = Q · sign(Λ).
            vt[(out_col, r)] = sign * q;
        }
    }

    Ok(SelectSvd {
        u,
        s: s_full[..k].to_vec(),
        vt,
        n_components: k,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_dimension_picks_first_drop_in_two_population_series() {
        // Big values then small values — elbow should land at idx=3.
        let values = vec![10.0, 9.5, 9.0, 1.0, 0.9, 0.8, 0.7];
        let elbows = select_dimension(&values, 1);
        assert_eq!(elbows.len(), 1);
        assert_eq!(elbows[0].0, 3);
    }

    #[test]
    fn select_dimension_returns_empty_for_short_input() {
        assert!(select_dimension(&[], 2).is_empty());
        assert!(select_dimension(&[1.0], 2).is_empty());
    }

    #[test]
    fn select_svd_truncates_to_requested_n() {
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 0)] = 5.0;
        a[(1, 1)] = 2.0;
        a[(2, 2)] = 1.0;
        let svd = select_svd(a.as_ref(), Some(2), 1).expect("ok");
        assert_eq!(svd.n_components, 2);
        assert!((svd.s[0] - 5.0).abs() < 1e-12);
        assert!((svd.s[1] - 2.0).abs() < 1e-12);
        assert_eq!(svd.u.nrows(), 3);
        assert_eq!(svd.u.ncols(), 2);
        assert_eq!(svd.vt.nrows(), 2);
        assert_eq!(svd.vt.ncols(), 3);
    }
}
