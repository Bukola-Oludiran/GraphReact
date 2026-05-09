//! L2-normalize the rows of a matrix.

use faer::prelude::*;
use faer::Mat;

use crate::error::{LinalgError, LinalgResult};

/// Threshold below which a row's norm is considered effectively zero — those
/// rows are left as-is to avoid division by near-zero.
pub const NORM_EPSILON: f64 = 1e-12;

/// Return a new matrix where each row of `a` has been L2-normalized.
///
/// Rows whose L2 norm is below [`NORM_EPSILON`] are passed through unchanged.
pub fn normalize_rows(a: MatRef<'_, f64>) -> LinalgResult<Mat<f64>> {
    let rows = a.nrows();
    let cols = a.ncols();
    if rows == 0 || cols == 0 {
        return Err(LinalgError::Empty);
    }

    let mut out = Mat::<f64>::zeros(rows, cols);
    for r in 0..rows {
        let mut norm_sq = 0.0_f64;
        for c in 0..cols {
            let v = a[(r, c)];
            norm_sq += v * v;
        }
        let norm = norm_sq.sqrt();
        if norm > NORM_EPSILON {
            for c in 0..cols {
                out[(r, c)] = a[(r, c)] / norm;
            }
        } else {
            for c in 0..cols {
                out[(r, c)] = a[(r, c)];
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn rows_have_unit_norm_after_normalization() {
        let mut a = Mat::<f64>::zeros(3, 2);
        a[(0, 0)] = 3.0;
        a[(0, 1)] = 4.0; // norm 5
        a[(1, 0)] = 1.0;
        a[(1, 1)] = 0.0; // norm 1
        a[(2, 0)] = 2.0;
        a[(2, 1)] = 2.0; // norm 2√2

        let n = normalize_rows(a.as_ref()).expect("normalize succeeds");
        for r in 0..3 {
            let mut nsq = 0.0;
            for c in 0..2 {
                nsq += n[(r, c)] * n[(r, c)];
            }
            assert!(approx_eq(nsq, 1.0, 1e-12), "row {r} norm² = {nsq}");
        }
    }

    #[test]
    fn zero_row_is_passed_through() {
        let mut a = Mat::<f64>::zeros(2, 2);
        a[(0, 0)] = 0.0;
        a[(0, 1)] = 0.0; // zero row
        a[(1, 0)] = 1.0;
        a[(1, 1)] = 0.0;

        let n = normalize_rows(a.as_ref()).expect("normalize succeeds");
        assert_eq!(n[(0, 0)], 0.0);
        assert_eq!(n[(0, 1)], 0.0);
        assert!(approx_eq(n[(1, 0)], 1.0, 1e-12));
    }

    #[test]
    fn empty_matrix_errors() {
        let a = Mat::<f64>::zeros(0, 0);
        let result = normalize_rows(a.as_ref());
        assert!(matches!(result, Err(LinalgError::Empty)));
    }
}
