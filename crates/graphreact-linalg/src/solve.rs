//! Solve linear systems: `A x = b`.

use faer::prelude::*;
use faer::Mat;

use crate::error::{LinalgError, LinalgResult};

/// Solve the linear system `A x = b` where `A` is `(n × n)` and `b` is `(n × 1)`.
///
/// Uses partial-pivot LU factorization. Returns `x` as a column vector.
pub fn solve(a: MatRef<'_, f64>, b: MatRef<'_, f64>) -> LinalgResult<Mat<f64>> {
    let n = a.nrows();
    if n == 0 || a.ncols() == 0 {
        return Err(LinalgError::Empty);
    }
    if a.nrows() != a.ncols() {
        return Err(LinalgError::DimensionMismatch {
            context: format!(
                "solve requires a square matrix, got ({}, {})",
                a.nrows(),
                a.ncols()
            ),
        });
    }
    if b.nrows() != n {
        return Err(LinalgError::DimensionMismatch {
            context: format!("solve: rows(b)={} must equal n={}", b.nrows(), n),
        });
    }
    if b.ncols() == 0 {
        return Err(LinalgError::Empty);
    }

    let lu = a.partial_piv_lu();
    Ok(lu.solve(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use faer::Mat;

    fn approx_eq(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn identity_solve() {
        // I · x = b ⇒ x = b
        let mut id = Mat::<f64>::zeros(3, 3);
        for i in 0..3 {
            id[(i, i)] = 1.0;
        }
        let mut b = Mat::<f64>::zeros(3, 1);
        b[(0, 0)] = 7.0;
        b[(1, 0)] = -2.0;
        b[(2, 0)] = 5.5;

        let x = solve(id.as_ref(), b.as_ref()).expect("solve succeeds");
        for i in 0..3 {
            assert!(approx_eq(x[(i, 0)], b[(i, 0)], 1e-12));
        }
    }

    #[test]
    fn diagonal_solve() {
        // diag(2, 4, 8) · x = [2, 8, 16] ⇒ x = [1, 2, 2]
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 0)] = 2.0;
        a[(1, 1)] = 4.0;
        a[(2, 2)] = 8.0;
        let mut b = Mat::<f64>::zeros(3, 1);
        b[(0, 0)] = 2.0;
        b[(1, 0)] = 8.0;
        b[(2, 0)] = 16.0;

        let x = solve(a.as_ref(), b.as_ref()).expect("solve succeeds");
        assert!(approx_eq(x[(0, 0)], 1.0, 1e-12));
        assert!(approx_eq(x[(1, 0)], 2.0, 1e-12));
        assert!(approx_eq(x[(2, 0)], 2.0, 1e-12));
    }

    #[test]
    fn round_trip() {
        // Solve A x = b, then verify A x ≈ b.
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 0)] = 2.0;
        a[(0, 1)] = 1.0;
        a[(0, 2)] = 0.0;
        a[(1, 0)] = 1.0;
        a[(1, 1)] = 3.0;
        a[(1, 2)] = 1.0;
        a[(2, 0)] = 0.0;
        a[(2, 1)] = 1.0;
        a[(2, 2)] = 2.0;

        let mut b = Mat::<f64>::zeros(3, 1);
        b[(0, 0)] = 3.0;
        b[(1, 0)] = 5.0;
        b[(2, 0)] = 3.0;

        let x = solve(a.as_ref(), b.as_ref()).expect("solve succeeds");
        let recon = &a * &x;
        for i in 0..3 {
            assert!(approx_eq(recon[(i, 0)], b[(i, 0)], 1e-10));
        }
    }

    #[test]
    fn non_square_errors() {
        let a = Mat::<f64>::zeros(2, 3);
        let b = Mat::<f64>::zeros(2, 1);
        let result = solve(a.as_ref(), b.as_ref());
        assert!(matches!(result, Err(LinalgError::DimensionMismatch { .. })));
    }
}
