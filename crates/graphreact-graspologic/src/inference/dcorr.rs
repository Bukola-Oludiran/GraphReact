//! Distance correlation (Székely & Rizzo, 2007).
//!
//! For two samples `X ∈ R^{n × p}` and `Y ∈ R^{n × q}`, distance
//! correlation `dCor(X, Y) ∈ [0, 1]` measures **any** form of
//! dependence between the marginals. `dCor = 0` ⇔ X ⫫ Y.
//!
//! Algorithm:
//! 1. Pairwise Euclidean distance matrices `A_ij = ‖x_i − x_j‖`,
//!    `B_ij = ‖y_i − y_j‖`.
//! 2. Doubly-centred matrices `Â_ij = A_ij − ā_i − ā_j + ā_..`.
//! 3. `dCov² = (1/n²) Σ Â_ij B̂_ij`, similarly for `dVar(X)` and
//!    `dVar(Y)`.
//! 4. `dCor² = dCov² / sqrt(dVar(X) · dVar(Y))`.
//!
//! Used by [`super::latent_distribution`] as the test statistic of the
//! permutation-based two-sample test.

use faer::{Mat, MatRef};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DcorrError {
    #[error("input samples are empty")]
    Empty,
    #[error("X and Y must have the same number of rows; got {n_x} and {n_y}")]
    RowMismatch { n_x: usize, n_y: usize },
}

pub type DcorrResult<T> = Result<T, DcorrError>;

/// Distance correlation `dCor(X, Y)` ∈ [0, 1].
pub fn distance_correlation(x: MatRef<'_, f64>, y: MatRef<'_, f64>) -> DcorrResult<f64> {
    let n = x.nrows();
    if n == 0 {
        return Err(DcorrError::Empty);
    }
    if y.nrows() != n {
        return Err(DcorrError::RowMismatch {
            n_x: n,
            n_y: y.nrows(),
        });
    }
    let a = pairwise_distance(x);
    let b = pairwise_distance(y);
    let a_centred = double_centre(&a);
    let b_centred = double_centre(&b);
    let dcov2 = mean_product(&a_centred, &b_centred);
    let dvar_x = mean_product(&a_centred, &a_centred);
    let dvar_y = mean_product(&b_centred, &b_centred);
    let denom = (dvar_x * dvar_y).sqrt();
    if denom <= 0.0 {
        return Ok(0.0);
    }
    Ok((dcov2 / denom).max(0.0).sqrt())
}

/// Convenience: pairwise Euclidean distance matrix `D_ij = ‖x_i − x_j‖`.
pub fn pairwise_distance(x: MatRef<'_, f64>) -> Mat<f64> {
    let n = x.nrows();
    let d = x.ncols();
    let mut out = Mat::<f64>::zeros(n, n);
    for i in 0..n {
        for j in (i + 1)..n {
            let mut s = 0.0_f64;
            for c in 0..d {
                let dif = x[(i, c)] - x[(j, c)];
                s += dif * dif;
            }
            let v = s.sqrt();
            out[(i, j)] = v;
            out[(j, i)] = v;
        }
    }
    out
}

fn double_centre(m: &Mat<f64>) -> Mat<f64> {
    let n = m.nrows();
    let mut row_means = vec![0.0_f64; n];
    let mut col_means = vec![0.0_f64; n];
    let mut grand = 0.0_f64;
    for i in 0..n {
        for j in 0..n {
            row_means[i] += m[(i, j)];
            col_means[j] += m[(i, j)];
            grand += m[(i, j)];
        }
    }
    for v in &mut row_means {
        *v /= n as f64;
    }
    for v in &mut col_means {
        *v /= n as f64;
    }
    grand /= (n as f64) * (n as f64);
    let mut out = Mat::<f64>::zeros(n, n);
    for i in 0..n {
        for j in 0..n {
            out[(i, j)] = m[(i, j)] - row_means[i] - col_means[j] + grand;
        }
    }
    out
}

fn mean_product(a: &Mat<f64>, b: &Mat<f64>) -> f64 {
    let n = a.nrows();
    let mut s = 0.0_f64;
    for i in 0..n {
        for j in 0..n {
            s += a[(i, j)] * b[(i, j)];
        }
    }
    s / ((n as f64) * (n as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dcorr_on_identical_inputs_is_one() {
        let mut x = Mat::<f64>::zeros(5, 2);
        for i in 0..5 {
            x[(i, 0)] = i as f64;
            x[(i, 1)] = 2.0 * i as f64;
        }
        let r = distance_correlation(x.as_ref(), x.as_ref()).expect("ok");
        assert!((r - 1.0).abs() < 1e-10, "expected 1.0, got {r}");
    }

    #[test]
    fn dcorr_on_constant_y_is_zero() {
        let mut x = Mat::<f64>::zeros(5, 1);
        for i in 0..5 {
            x[(i, 0)] = i as f64;
        }
        let y = Mat::<f64>::zeros(5, 1);
        let r = distance_correlation(x.as_ref(), y.as_ref()).expect("ok");
        assert!(r.abs() < 1e-12);
    }

    #[test]
    fn dcorr_detects_linear_dependence() {
        let mut x = Mat::<f64>::zeros(20, 1);
        let mut y = Mat::<f64>::zeros(20, 1);
        for i in 0..20 {
            let v = i as f64;
            x[(i, 0)] = v;
            y[(i, 0)] = 3.0 * v + 1.0;
        }
        let r = distance_correlation(x.as_ref(), y.as_ref()).expect("ok");
        assert!(r > 0.99);
    }

    #[test]
    fn dcorr_rejects_row_mismatch() {
        let x = Mat::<f64>::zeros(3, 2);
        let y = Mat::<f64>::zeros(4, 2);
        assert!(matches!(
            distance_correlation(x.as_ref(), y.as_ref()),
            Err(DcorrError::RowMismatch { .. })
        ));
    }
}
