//! Truncated singular value decomposition.

use faer::prelude::*;
use faer::Mat;

use crate::error::{LinalgError, LinalgResult};

/// Result of a truncated SVD: `A ≈ U Σ Vᵀ`.
///
/// `u` is `(rows × k)`, `s` holds the top-`k` singular values in descending
/// order, and `vt` is `(k × cols)`.
#[derive(Debug, Clone)]
pub struct SvdDecomp {
    pub u: Mat<f64>,
    pub s: Vec<f64>,
    pub vt: Mat<f64>,
    pub k: usize,
}

/// Truncated SVD of an `(rows × cols)` matrix.
///
/// Returns the `k` largest singular triplets. `k` is clamped to
/// `min(rows, cols)`. Errors if the input is empty.
pub fn svd(a: MatRef<'_, f64>, k: usize) -> LinalgResult<SvdDecomp> {
    let rows = a.nrows();
    let cols = a.ncols();
    if rows == 0 || cols == 0 {
        return Err(LinalgError::Empty);
    }
    let k = k.min(rows).min(cols);
    if k == 0 {
        return Err(LinalgError::InvalidRank { rows, cols, k: 0 });
    }

    let decomp = a.svd();

    // Singular values, top-k descending. faer returns them sorted descending.
    let s_diag = decomp.s_diagonal();
    let s: Vec<f64> = (0..k).map(|i| s_diag[i]).collect();

    // U truncated to first k columns.
    let u_full = decomp.u();
    let mut u = Mat::<f64>::zeros(rows, k);
    for r in 0..rows {
        for c in 0..k {
            u[(r, c)] = u_full[(r, c)];
        }
    }

    // Vᵀ — faer returns V (cols × cols); take first k columns and transpose.
    let v_full = decomp.v();
    let mut vt = Mat::<f64>::zeros(k, cols);
    for r in 0..k {
        for c in 0..cols {
            vt[(r, c)] = v_full[(c, r)];
        }
    }

    Ok(SvdDecomp { u, s, vt, k })
}

#[cfg(test)]
mod tests {
    use super::*;
    use faer::Mat;

    fn approx_eq(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn diagonal_matrix_has_diagonal_singular_values() {
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 0)] = 3.0;
        a[(1, 1)] = 2.0;
        a[(2, 2)] = 1.0;

        let decomp = svd(a.as_ref(), 3).expect("svd succeeds");
        assert_eq!(decomp.k, 3);
        assert!(approx_eq(decomp.s[0], 3.0, 1e-10));
        assert!(approx_eq(decomp.s[1], 2.0, 1e-10));
        assert!(approx_eq(decomp.s[2], 1.0, 1e-10));
    }

    #[test]
    fn truncation_returns_top_k() {
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 0)] = 3.0;
        a[(1, 1)] = 2.0;
        a[(2, 2)] = 1.0;

        let decomp = svd(a.as_ref(), 2).expect("svd succeeds");
        assert_eq!(decomp.k, 2);
        assert_eq!(decomp.s.len(), 2);
        assert!(approx_eq(decomp.s[0], 3.0, 1e-10));
        assert!(approx_eq(decomp.s[1], 2.0, 1e-10));
    }

    #[test]
    fn reconstruction_is_close_to_original() {
        let data = [
            [1.0, 0.5, 0.2],
            [0.5, 1.0, 0.3],
            [0.2, 0.3, 1.0],
            [0.7, 0.1, 0.4],
        ];
        let mut a = Mat::<f64>::zeros(4, 3);
        for (r, row) in data.iter().enumerate() {
            for (c, &v) in row.iter().enumerate() {
                a[(r, c)] = v;
            }
        }

        let decomp = svd(a.as_ref(), 3).expect("svd succeeds");
        let mut sigma = Mat::<f64>::zeros(3, 3);
        for i in 0..3 {
            sigma[(i, i)] = decomp.s[i];
        }
        let recon = &decomp.u * &sigma * &decomp.vt;

        for r in 0..4 {
            for c in 0..3 {
                assert!(
                    approx_eq(a[(r, c)], recon[(r, c)], 1e-10),
                    "mismatch at ({r}, {c}): {} vs {}",
                    a[(r, c)],
                    recon[(r, c)]
                );
            }
        }
    }

    #[test]
    fn empty_matrix_errors() {
        let a = Mat::<f64>::zeros(0, 0);
        let result = svd(a.as_ref(), 1);
        assert!(matches!(result, Err(LinalgError::Empty)));
    }
}
