//! Orthogonal Procrustes alignment.
//!
//! Spectral embeddings are unique only up to an orthogonal
//! transformation in the latent space. Direct comparison of two
//! embeddings is meaningless — they must first be aligned by the
//! orthogonal matrix that minimises their Frobenius distance.
//!
//! Given `X, Y ∈ R^{n × k}`, the orthogonal Procrustes problem
//!
//! ```text
//! minimise || X R − Y ||_F   over orthogonal R ∈ R^{k × k}
//! ```
//!
//! has the closed-form solution `R = U V^T` where `X^T Y = U Σ V^T`.

use faer::{Mat, MatRef};

use graphreact_linalg::svd as linalg_svd;

use super::error::{EmbedError, EmbedResult};

/// Solve `min_R || X R − Y ||_F` over orthogonal `R`.
pub fn orthogonal_procrustes(x: MatRef<'_, f64>, y: MatRef<'_, f64>) -> EmbedResult<Mat<f64>> {
    if x.nrows() != y.nrows() || x.ncols() != y.ncols() {
        return Err(EmbedError::ShapeMismatch {
            rows0: x.nrows(),
            cols0: x.ncols(),
            rows: y.nrows(),
            cols: y.ncols(),
        });
    }
    if x.nrows() == 0 || x.ncols() == 0 {
        return Err(EmbedError::Empty);
    }

    let k = x.ncols();
    // M = X^T Y, shape (k × k).
    let mut m = Mat::<f64>::zeros(k, k);
    for r in 0..k {
        for c in 0..k {
            let mut acc = 0.0;
            for n in 0..x.nrows() {
                acc += x[(n, r)] * y[(n, c)];
            }
            m[(r, c)] = acc;
        }
    }

    let svd = linalg_svd(m.as_ref(), k)?;
    // R = U V^T  (svd.vt is already V^T)
    let mut rmat = Mat::<f64>::zeros(k, k);
    for r in 0..k {
        for c in 0..k {
            let mut acc = 0.0;
            for i in 0..k {
                acc += svd.u[(r, i)] * svd.vt[(i, c)];
            }
            rmat[(r, c)] = acc;
        }
    }
    Ok(rmat)
}

/// Apply the Procrustes-optimal rotation to align `x` with `y`,
/// returning `||X R − Y||_F`.
pub fn procrustes_distance(x: MatRef<'_, f64>, y: MatRef<'_, f64>) -> EmbedResult<f64> {
    let rmat = orthogonal_procrustes(x, y)?;
    let aligned = x * rmat.as_ref();
    let mut sum_sq = 0.0;
    for r in 0..x.nrows() {
        for c in 0..x.ncols() {
            let d = aligned[(r, c)] - y[(r, c)];
            sum_sq += d * d;
        }
    }
    Ok(sum_sq.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_orthogonal_2d(theta: f64) -> Mat<f64> {
        let mut r = Mat::<f64>::zeros(2, 2);
        r[(0, 0)] = theta.cos();
        r[(0, 1)] = -theta.sin();
        r[(1, 0)] = theta.sin();
        r[(1, 1)] = theta.cos();
        r
    }

    #[test]
    fn procrustes_recovers_known_rotation() {
        let mut x = Mat::<f64>::zeros(4, 2);
        x[(0, 0)] = 1.0;
        x[(0, 1)] = 0.0;
        x[(1, 0)] = 0.0;
        x[(1, 1)] = 1.0;
        x[(2, 0)] = 1.0;
        x[(2, 1)] = 1.0;
        x[(3, 0)] = -1.0;
        x[(3, 1)] = 0.5;

        let r = make_orthogonal_2d(std::f64::consts::FRAC_PI_4);
        let y = x.as_ref() * r.as_ref();

        let dist = procrustes_distance(x.as_ref(), y.as_ref()).expect("ok");
        assert!(dist < 1e-10, "expected small distance, got {dist}");
    }

    #[test]
    fn procrustes_distance_is_zero_for_identical_inputs() {
        let mut x = Mat::<f64>::zeros(3, 2);
        x[(0, 0)] = 1.0;
        x[(1, 1)] = 1.0;
        x[(2, 0)] = 0.5;
        x[(2, 1)] = 0.5;
        let dist = procrustes_distance(x.as_ref(), x.as_ref()).expect("ok");
        assert!(dist < 1e-12);
    }
}
