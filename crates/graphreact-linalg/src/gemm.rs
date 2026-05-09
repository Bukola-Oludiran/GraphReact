//! General matrix multiply: `C = A × B`.

use faer::prelude::*;
use faer::Mat;

use crate::error::{LinalgError, LinalgResult};

/// Compute `C = A × B`.
///
/// `a` must be `(m × k)` and `b` must be `(k × n)`. Result is `(m × n)`.
pub fn gemm(a: MatRef<'_, f64>, b: MatRef<'_, f64>) -> LinalgResult<Mat<f64>> {
    if a.ncols() != b.nrows() {
        return Err(LinalgError::DimensionMismatch {
            context: format!(
                "GEMM: cols(A)={} must equal rows(B)={}",
                a.ncols(),
                b.nrows()
            ),
        });
    }
    if a.nrows() == 0 || a.ncols() == 0 || b.ncols() == 0 {
        return Err(LinalgError::Empty);
    }
    Ok(a * b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use faer::Mat;

    fn approx_eq(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn identity_times_matrix_is_matrix() {
        let mut id = Mat::<f64>::zeros(3, 3);
        for i in 0..3 {
            id[(i, i)] = 1.0;
        }
        let mut b = Mat::<f64>::zeros(3, 2);
        b[(0, 0)] = 1.0;
        b[(0, 1)] = 2.0;
        b[(1, 0)] = 3.0;
        b[(1, 1)] = 4.0;
        b[(2, 0)] = 5.0;
        b[(2, 1)] = 6.0;

        let c = gemm(id.as_ref(), b.as_ref()).expect("gemm succeeds");
        for r in 0..3 {
            for col in 0..2 {
                assert!(approx_eq(c[(r, col)], b[(r, col)], 1e-12));
            }
        }
    }

    #[test]
    fn known_product() {
        // A = [[1,2],[3,4]], B = [[5,6],[7,8]]
        // A·B = [[19,22],[43,50]]
        let mut a = Mat::<f64>::zeros(2, 2);
        a[(0, 0)] = 1.0;
        a[(0, 1)] = 2.0;
        a[(1, 0)] = 3.0;
        a[(1, 1)] = 4.0;
        let mut b = Mat::<f64>::zeros(2, 2);
        b[(0, 0)] = 5.0;
        b[(0, 1)] = 6.0;
        b[(1, 0)] = 7.0;
        b[(1, 1)] = 8.0;

        let c = gemm(a.as_ref(), b.as_ref()).expect("gemm succeeds");
        assert!(approx_eq(c[(0, 0)], 19.0, 1e-12));
        assert!(approx_eq(c[(0, 1)], 22.0, 1e-12));
        assert!(approx_eq(c[(1, 0)], 43.0, 1e-12));
        assert!(approx_eq(c[(1, 1)], 50.0, 1e-12));
    }

    #[test]
    fn dimension_mismatch_errors() {
        let a = Mat::<f64>::zeros(2, 3);
        let b = Mat::<f64>::zeros(2, 4);
        let result = gemm(a.as_ref(), b.as_ref());
        assert!(matches!(result, Err(LinalgError::DimensionMismatch { .. })));
    }
}
