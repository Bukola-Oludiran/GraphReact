//! Symmetric eigendecomposition.

use faer::prelude::*;
use faer::{Mat, Side};

use crate::error::{LinalgError, LinalgResult};

/// Result of a symmetric eigendecomposition.
///
/// Eigenvalues are returned in **descending** order (largest first), matching
/// the convention used by spectral embedding algorithms. Eigenvectors are
/// stored column-wise in `eigenvectors` (i.e. column `i` is the eigenvector
/// corresponding to `eigenvalues[i]`).
#[derive(Debug, Clone)]
pub struct EighDecomp {
    pub eigenvalues: Vec<f64>,
    pub eigenvectors: Mat<f64>,
    pub k: usize,
    pub n: usize,
}

/// Symmetric eigendecomposition of an `(n × n)` matrix.
///
/// Returns the `k` **largest** eigenvalues and their eigenvectors. Only the
/// lower triangle of `a` is read — caller must ensure the matrix is symmetric.
pub fn eigh(a: MatRef<'_, f64>, k: usize) -> LinalgResult<EighDecomp> {
    let n = a.nrows();
    if n == 0 || a.ncols() == 0 {
        return Err(LinalgError::Empty);
    }
    if a.nrows() != a.ncols() {
        return Err(LinalgError::DimensionMismatch {
            context: format!(
                "eigh requires a square matrix, got ({}, {})",
                a.nrows(),
                a.ncols()
            ),
        });
    }
    let k = k.min(n);
    if k == 0 {
        return Err(LinalgError::InvalidRank {
            rows: n,
            cols: n,
            k: 0,
        });
    }

    let decomp = a.selfadjoint_eigendecomposition(Side::Lower);
    // faer returns eigenvalues in ascending order; we want largest k, descending.
    let all_vals = decomp.s().column_vector();
    let eigvecs = decomp.u();

    let eigenvalues: Vec<f64> = (0..k).map(|i| all_vals[n - 1 - i]).collect();
    let mut eigenvectors = Mat::<f64>::zeros(n, k);
    for r in 0..n {
        for i in 0..k {
            eigenvectors[(r, i)] = eigvecs[(r, n - 1 - i)];
        }
    }

    Ok(EighDecomp {
        eigenvalues,
        eigenvectors,
        k,
        n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use faer::Mat;

    fn approx_eq(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn diagonal_matrix_eigenvalues() {
        // Diagonal [1, 5, 3] — eigenvalues sorted descending: [5, 3, 1].
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 0)] = 1.0;
        a[(1, 1)] = 5.0;
        a[(2, 2)] = 3.0;

        let decomp = eigh(a.as_ref(), 3).expect("eigh succeeds");
        assert_eq!(decomp.k, 3);
        assert!(approx_eq(decomp.eigenvalues[0], 5.0, 1e-10));
        assert!(approx_eq(decomp.eigenvalues[1], 3.0, 1e-10));
        assert!(approx_eq(decomp.eigenvalues[2], 1.0, 1e-10));
    }

    #[test]
    fn truncation_returns_top_k_eigenvalues() {
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 0)] = 1.0;
        a[(1, 1)] = 5.0;
        a[(2, 2)] = 3.0;

        let decomp = eigh(a.as_ref(), 2).expect("eigh succeeds");
        assert_eq!(decomp.k, 2);
        assert!(approx_eq(decomp.eigenvalues[0], 5.0, 1e-10));
        assert!(approx_eq(decomp.eigenvalues[1], 3.0, 1e-10));
    }

    #[test]
    fn eigenvectors_are_orthonormal() {
        // Symmetric matrix.
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 0)] = 4.0;
        a[(0, 1)] = 1.0;
        a[(0, 2)] = 0.0;
        a[(1, 0)] = 1.0;
        a[(1, 1)] = 3.0;
        a[(1, 2)] = 1.0;
        a[(2, 0)] = 0.0;
        a[(2, 1)] = 1.0;
        a[(2, 2)] = 2.0;

        let decomp = eigh(a.as_ref(), 3).expect("eigh succeeds");
        let v = &decomp.eigenvectors;

        // Each column should have unit L2 norm.
        for c in 0..3 {
            let mut norm_sq = 0.0;
            for r in 0..3 {
                norm_sq += v[(r, c)] * v[(r, c)];
            }
            assert!(
                approx_eq(norm_sq, 1.0, 1e-10),
                "column {c} norm² = {norm_sq}"
            );
        }

        // Columns should be pairwise orthogonal.
        for i in 0..3 {
            for j in (i + 1)..3 {
                let mut dot = 0.0;
                for r in 0..3 {
                    dot += v[(r, i)] * v[(r, j)];
                }
                assert!(approx_eq(dot, 0.0, 1e-10), "cols {i},{j} dot = {dot}");
            }
        }
    }

    #[test]
    fn non_square_errors() {
        let a = Mat::<f64>::zeros(3, 2);
        let result = eigh(a.as_ref(), 1);
        assert!(matches!(result, Err(LinalgError::DimensionMismatch { .. })));
    }
}
