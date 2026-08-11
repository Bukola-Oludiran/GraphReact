//! Gaussian Mixture Model (GMM) via EM.
//!
//! Fits `k` multivariate normal components to an `(n × d)` data matrix
//! `X` by Expectation–Maximisation. Supports four covariance shapes:
//!
//! * `Full` — each component has an arbitrary `d × d` covariance.
//! * `Tied` — all components share one `d × d` covariance.
//! * `Diag` — each component has a diagonal `d × d` covariance.
//! * `Spherical` — each component has a single variance.
//!
//! Reference: Bishop, "Pattern Recognition and Machine Learning",
//! ch. 9.2; sklearn's `GaussianMixture`.
//!
//! BIC-based model selection across `k_min..=k_max` is exposed via
//! [`fit_with_model_selection`], matching graspologic's
//! `GaussianCluster` essence (without sklearn-specific bells like
//! parameter grids — kept Rust-native per the project's
//! "feature parity ≠ line-by-line" stance).

use std::f64::consts::TAU;

use faer::{Mat, MatRef};
use rand::seq::IteratorRandom;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use thiserror::Error;

use graphreact_linalg::eigh as linalg_eigh;

#[derive(Debug, Error)]
pub enum GmmError {
    #[error("input matrix is empty")]
    Empty,
    #[error("must have at least one mixture component, got {n}")]
    InvalidK { n: usize },
    #[error("k ({k}) must not exceed n_samples ({n})")]
    KExceedsN { k: usize, n: usize },
    #[error("covariance is singular for component {component} — try increasing reg_covar")]
    SingularCovariance { component: usize },
    #[error("non-finite value in input data at ({row}, {col})")]
    NonFinite { row: usize, col: usize },
}

pub type GmmResult<T> = Result<T, GmmError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CovarianceType {
    #[default]
    Full,
    Tied,
    Diag,
    Spherical,
}

#[derive(Debug, Clone)]
pub struct GmmOptions {
    pub n_components: usize,
    pub covariance_type: CovarianceType,
    pub max_iter: usize,
    pub tol: f64,
    /// Diagonal regularisation added to component covariances to keep
    /// them invertible. Default `1e-6`.
    pub reg_covar: f64,
    pub n_init: usize,
    pub seed: Option<u64>,
}

impl Default for GmmOptions {
    fn default() -> Self {
        Self {
            n_components: 2,
            covariance_type: CovarianceType::Full,
            max_iter: 100,
            tol: 1e-3,
            reg_covar: 1e-6,
            n_init: 1,
            seed: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GmmModel {
    /// Mixing weights `π_k`, sums to 1.
    pub weights: Vec<f64>,
    /// Component means, one row per component, `(k × d)`.
    pub means: Mat<f64>,
    /// Component covariances. `Full`/`Diag`/`Spherical` → length `k`;
    /// `Tied` → length `1`. `Diag` rows are the diagonal as a `(1 × d)`
    /// row matrix; `Spherical` rows are a single `(1 × 1)` value.
    pub covariances: Vec<Mat<f64>>,
    pub covariance_type: CovarianceType,
    /// Final log-likelihood of the data under the fitted model.
    pub log_likelihood: f64,
    /// Number of EM iterations actually executed.
    pub iterations: usize,
    /// `true` if the inner change fell below `tol` before `max_iter`.
    pub converged: bool,
}

impl GmmModel {
    /// Per-sample posterior probabilities `γ[i,k]`. Shape `(n × k)`.
    pub fn predict_proba(&self, x: MatRef<'_, f64>) -> GmmResult<Mat<f64>> {
        let log_resp = compute_log_responsibilities(
            x,
            &self.weights,
            self.means.as_ref(),
            &self.covariances,
            self.covariance_type,
        )?;
        let n = x.nrows();
        let k = self.weights.len();
        let mut out = Mat::<f64>::zeros(n, k);
        for r in 0..n {
            for c in 0..k {
                out[(r, c)] = log_resp[(r, c)].exp();
            }
        }
        Ok(out)
    }

    /// Hard cluster assignments by argmax responsibility.
    pub fn predict(&self, x: MatRef<'_, f64>) -> GmmResult<Vec<usize>> {
        let proba = self.predict_proba(x)?;
        let n = proba.nrows();
        let k = proba.ncols();
        let mut labels = vec![0_usize; n];
        for r in 0..n {
            let mut best = 0_usize;
            let mut best_v = f64::NEG_INFINITY;
            for c in 0..k {
                if proba[(r, c)] > best_v {
                    best_v = proba[(r, c)];
                    best = c;
                }
            }
            labels[r] = best;
        }
        Ok(labels)
    }

    /// Bayesian Information Criterion. Lower is better.
    pub fn bic(&self, n_samples: usize) -> f64 {
        let n_params = self.n_parameters() as f64;
        let n = n_samples as f64;
        -2.0 * self.log_likelihood + n_params * n.ln()
    }

    pub fn n_parameters(&self) -> usize {
        let k = self.weights.len();
        let d = self.means.ncols();
        let cov_params = match self.covariance_type {
            CovarianceType::Full => k * d * (d + 1) / 2,
            CovarianceType::Tied => d * (d + 1) / 2,
            CovarianceType::Diag => k * d,
            CovarianceType::Spherical => k,
        };
        let mean_params = k * d;
        let weight_params = k - 1;
        cov_params + mean_params + weight_params
    }
}

/// Fit a GMM with fixed `k` (taken from `opts.n_components`).
pub fn fit(x: MatRef<'_, f64>, opts: &GmmOptions) -> GmmResult<GmmModel> {
    let n = x.nrows();
    let d = x.ncols();
    if n == 0 || d == 0 {
        return Err(GmmError::Empty);
    }
    let k = opts.n_components;
    if k == 0 {
        return Err(GmmError::InvalidK { n: 0 });
    }
    if k > n {
        return Err(GmmError::KExceedsN { k, n });
    }
    for r in 0..n {
        for c in 0..d {
            if !x[(r, c)].is_finite() {
                return Err(GmmError::NonFinite { row: r, col: c });
            }
        }
    }

    let mut rng = match opts.seed {
        Some(s) => ChaCha8Rng::seed_from_u64(s),
        None => ChaCha8Rng::from_entropy(),
    };
    let mut best: Option<GmmModel> = None;

    for _ in 0..opts.n_init.max(1) {
        let model = run_em(x, opts, &mut rng)?;
        match &best {
            None => best = Some(model),
            Some(prev) => {
                if model.log_likelihood > prev.log_likelihood {
                    best = Some(model);
                }
            }
        }
    }
    Ok(best.expect("n_init ≥ 1 produces at least one candidate"))
}

/// Fit GMMs across `k_min..=k_max` and return the BIC-best.
pub fn fit_with_model_selection(
    x: MatRef<'_, f64>,
    k_min: usize,
    k_max: usize,
    base_opts: &GmmOptions,
) -> GmmResult<GmmModel> {
    if k_min == 0 || k_max < k_min {
        return Err(GmmError::InvalidK { n: k_min });
    }
    let n = x.nrows();
    let mut best: Option<(f64, GmmModel)> = None;
    for k in k_min..=k_max {
        if k > n {
            break;
        }
        let opts = GmmOptions {
            n_components: k,
            ..base_opts.clone()
        };
        let model = fit(x, &opts)?;
        let bic = model.bic(n);
        match &best {
            None => best = Some((bic, model)),
            Some((prev_bic, _)) => {
                if bic < *prev_bic {
                    best = Some((bic, model));
                }
            }
        }
    }
    best.map(|(_, m)| m).ok_or(GmmError::Empty)
}

fn run_em(x: MatRef<'_, f64>, opts: &GmmOptions, rng: &mut impl Rng) -> GmmResult<GmmModel> {
    let n = x.nrows();
    let d = x.ncols();
    let k = opts.n_components;

    // Init means by random sample selection.
    let chosen: Vec<usize> = (0..n).choose_multiple(rng, k);
    let mut means = Mat::<f64>::zeros(k, d);
    for (kc, &row) in chosen.iter().enumerate() {
        for c in 0..d {
            means[(kc, c)] = x[(row, c)];
        }
    }
    let mut weights = vec![1.0 / k as f64; k];
    let mut covariances = init_covariances(x, opts.covariance_type, k, d, opts.reg_covar);

    let mut prev_ll = f64::NEG_INFINITY;
    let mut converged = false;
    let mut iterations = 0_usize;
    let mut log_likelihood = f64::NEG_INFINITY;

    for _ in 0..opts.max_iter.max(1) {
        iterations += 1;
        // E-step: log-responsibilities.
        let log_resp = compute_log_responsibilities(
            x,
            &weights,
            means.as_ref(),
            &covariances,
            opts.covariance_type,
        )?;

        // Aggregate log-likelihood by re-using the un-normalised log-prob
        // from a parallel pass (the responsibilities are already
        // normalised, so the LL is summed inside that helper).
        log_likelihood = compute_log_likelihood(
            x,
            &weights,
            means.as_ref(),
            &covariances,
            opts.covariance_type,
        )?;

        // M-step: re-estimate means, covariances, weights.
        let resp = log_resp_to_resp(&log_resp);
        let nk = column_sums(&resp);
        for kc in 0..k {
            weights[kc] = nk[kc].max(1e-300) / n as f64;
        }
        means = compute_means(x, &resp, &nk);
        covariances = compute_covariances(
            x,
            &resp,
            &nk,
            means.as_ref(),
            opts.covariance_type,
            opts.reg_covar,
        );

        let delta = log_likelihood - prev_ll;
        prev_ll = log_likelihood;
        if delta.abs() < opts.tol {
            converged = true;
            break;
        }
    }

    Ok(GmmModel {
        weights,
        means,
        covariances,
        covariance_type: opts.covariance_type,
        log_likelihood,
        iterations,
        converged,
    })
}

fn init_covariances(
    x: MatRef<'_, f64>,
    cov: CovarianceType,
    k: usize,
    d: usize,
    reg: f64,
) -> Vec<Mat<f64>> {
    // Initialize from the global sample covariance.
    let n = x.nrows();
    let mut mean = vec![0.0_f64; d];
    for r in 0..n {
        for c in 0..d {
            mean[c] += x[(r, c)];
        }
    }
    for v in &mut mean {
        *v /= n as f64;
    }
    let mut full_cov = Mat::<f64>::zeros(d, d);
    for r in 0..n {
        for i in 0..d {
            for j in 0..d {
                full_cov[(i, j)] += (x[(r, i)] - mean[i]) * (x[(r, j)] - mean[j]);
            }
        }
    }
    for i in 0..d {
        for j in 0..d {
            full_cov[(i, j)] /= n as f64;
        }
        full_cov[(i, i)] += reg;
    }

    match cov {
        CovarianceType::Full => (0..k).map(|_| full_cov.clone()).collect(),
        CovarianceType::Tied => vec![full_cov],
        CovarianceType::Diag => {
            let mut diag = Mat::<f64>::zeros(1, d);
            for c in 0..d {
                diag[(0, c)] = full_cov[(c, c)];
            }
            (0..k).map(|_| diag.clone()).collect()
        }
        CovarianceType::Spherical => {
            let mut trace = 0.0;
            for c in 0..d {
                trace += full_cov[(c, c)];
            }
            let var = trace / d as f64;
            let mut s = Mat::<f64>::zeros(1, 1);
            s[(0, 0)] = var;
            (0..k).map(|_| s.clone()).collect()
        }
    }
}

fn compute_log_responsibilities(
    x: MatRef<'_, f64>,
    weights: &[f64],
    means: MatRef<'_, f64>,
    covariances: &[Mat<f64>],
    cov: CovarianceType,
) -> GmmResult<Mat<f64>> {
    let n = x.nrows();
    let k = weights.len();
    let mut log_w = vec![0.0; k];
    for c in 0..k {
        log_w[c] = weights[c].max(1e-300).ln();
    }

    let mut log_prob = Mat::<f64>::zeros(n, k);
    for c in 0..k {
        let log_pdf = log_multivariate_normal(x, means, c, covariances, cov)?;
        for r in 0..n {
            log_prob[(r, c)] = log_pdf[r] + log_w[c];
        }
    }
    let mut log_resp = Mat::<f64>::zeros(n, k);
    for r in 0..n {
        let mut max_lp = f64::NEG_INFINITY;
        for c in 0..k {
            if log_prob[(r, c)] > max_lp {
                max_lp = log_prob[(r, c)];
            }
        }
        let mut sum_exp = 0.0;
        for c in 0..k {
            sum_exp += (log_prob[(r, c)] - max_lp).exp();
        }
        let log_norm = max_lp + sum_exp.ln();
        for c in 0..k {
            log_resp[(r, c)] = log_prob[(r, c)] - log_norm;
        }
    }
    Ok(log_resp)
}

fn compute_log_likelihood(
    x: MatRef<'_, f64>,
    weights: &[f64],
    means: MatRef<'_, f64>,
    covariances: &[Mat<f64>],
    cov: CovarianceType,
) -> GmmResult<f64> {
    let n = x.nrows();
    let k = weights.len();
    let mut log_w = vec![0.0; k];
    for c in 0..k {
        log_w[c] = weights[c].max(1e-300).ln();
    }
    let mut total = 0.0_f64;
    // Row-wise logsumexp of log p(x | k) + log π_k.
    let mut log_prob = Mat::<f64>::zeros(n, k);
    for c in 0..k {
        let log_pdf = log_multivariate_normal(x, means, c, covariances, cov)?;
        for r in 0..n {
            log_prob[(r, c)] = log_pdf[r] + log_w[c];
        }
    }
    for r in 0..n {
        let mut max_lp = f64::NEG_INFINITY;
        for c in 0..k {
            if log_prob[(r, c)] > max_lp {
                max_lp = log_prob[(r, c)];
            }
        }
        let mut sum_exp = 0.0;
        for c in 0..k {
            sum_exp += (log_prob[(r, c)] - max_lp).exp();
        }
        total += max_lp + sum_exp.ln();
    }
    Ok(total)
}

fn log_multivariate_normal(
    x: MatRef<'_, f64>,
    means: MatRef<'_, f64>,
    component: usize,
    covariances: &[Mat<f64>],
    cov: CovarianceType,
) -> GmmResult<Vec<f64>> {
    let n = x.nrows();
    let d = x.ncols();
    let mu: Vec<f64> = (0..d).map(|c| means[(component, c)]).collect();
    let log_2pi = TAU.ln();

    match cov {
        CovarianceType::Full => {
            let sigma = &covariances[component];
            let (inv, log_det) = invert_symmetric_positive(sigma.as_ref(), component)?;
            let mut out = vec![0.0; n];
            for r in 0..n {
                let diff: Vec<f64> = (0..d).map(|c| x[(r, c)] - mu[c]).collect();
                let mut quad = 0.0;
                for i in 0..d {
                    for j in 0..d {
                        quad += diff[i] * inv[(i, j)] * diff[j];
                    }
                }
                out[r] = -0.5 * (d as f64 * log_2pi + log_det + quad);
            }
            Ok(out)
        }
        CovarianceType::Tied => {
            let sigma = &covariances[0];
            let (inv, log_det) = invert_symmetric_positive(sigma.as_ref(), 0)?;
            let mut out = vec![0.0; n];
            for r in 0..n {
                let diff: Vec<f64> = (0..d).map(|c| x[(r, c)] - mu[c]).collect();
                let mut quad = 0.0;
                for i in 0..d {
                    for j in 0..d {
                        quad += diff[i] * inv[(i, j)] * diff[j];
                    }
                }
                out[r] = -0.5 * (d as f64 * log_2pi + log_det + quad);
            }
            Ok(out)
        }
        CovarianceType::Diag => {
            let diag = &covariances[component];
            let mut log_det = 0.0;
            for c in 0..d {
                let v = diag[(0, c)];
                if v <= 0.0 {
                    return Err(GmmError::SingularCovariance { component });
                }
                log_det += v.ln();
            }
            let mut out = vec![0.0; n];
            for r in 0..n {
                let mut quad = 0.0;
                for c in 0..d {
                    let diff = x[(r, c)] - mu[c];
                    quad += diff * diff / diag[(0, c)];
                }
                out[r] = -0.5 * (d as f64 * log_2pi + log_det + quad);
            }
            Ok(out)
        }
        CovarianceType::Spherical => {
            let var = covariances[component][(0, 0)];
            if var <= 0.0 {
                return Err(GmmError::SingularCovariance { component });
            }
            let log_det = d as f64 * var.ln();
            let mut out = vec![0.0; n];
            for r in 0..n {
                let mut quad = 0.0;
                for c in 0..d {
                    let diff = x[(r, c)] - mu[c];
                    quad += diff * diff;
                }
                quad /= var;
                out[r] = -0.5 * (d as f64 * log_2pi + log_det + quad);
            }
            Ok(out)
        }
    }
}

fn invert_symmetric_positive(s: MatRef<'_, f64>, component: usize) -> GmmResult<(Mat<f64>, f64)> {
    // s = Q Λ Q^T → s^{-1} = Q Λ^{-1} Q^T, log|s| = Σ ln λ_i
    let d = s.nrows();
    let decomp = linalg_eigh(s, d).map_err(|_| GmmError::SingularCovariance { component })?;
    let mut log_det = 0.0_f64;
    for &lam in &decomp.eigenvalues {
        if lam <= 0.0 {
            return Err(GmmError::SingularCovariance { component });
        }
        log_det += lam.ln();
    }
    let mut inv = Mat::<f64>::zeros(d, d);
    for i in 0..d {
        for j in 0..d {
            let mut acc = 0.0;
            for kc in 0..d {
                acc += decomp.eigenvectors[(i, kc)] * decomp.eigenvectors[(j, kc)]
                    / decomp.eigenvalues[kc];
            }
            inv[(i, j)] = acc;
        }
    }
    Ok((inv, log_det))
}

fn log_resp_to_resp(log_resp: &Mat<f64>) -> Mat<f64> {
    let n = log_resp.nrows();
    let k = log_resp.ncols();
    let mut out = Mat::<f64>::zeros(n, k);
    for r in 0..n {
        for c in 0..k {
            out[(r, c)] = log_resp[(r, c)].exp();
        }
    }
    out
}

fn column_sums(m: &Mat<f64>) -> Vec<f64> {
    let n = m.nrows();
    let k = m.ncols();
    let mut out = vec![0.0; k];
    for c in 0..k {
        for r in 0..n {
            out[c] += m[(r, c)];
        }
    }
    out
}

fn compute_means(x: MatRef<'_, f64>, resp: &Mat<f64>, nk: &[f64]) -> Mat<f64> {
    let n = x.nrows();
    let d = x.ncols();
    let k = resp.ncols();
    let mut means = Mat::<f64>::zeros(k, d);
    for c in 0..k {
        let denom = nk[c].max(1e-300);
        for col in 0..d {
            let mut acc = 0.0;
            for r in 0..n {
                acc += resp[(r, c)] * x[(r, col)];
            }
            means[(c, col)] = acc / denom;
        }
    }
    means
}

fn compute_covariances(
    x: MatRef<'_, f64>,
    resp: &Mat<f64>,
    nk: &[f64],
    means: MatRef<'_, f64>,
    cov: CovarianceType,
    reg: f64,
) -> Vec<Mat<f64>> {
    let n = x.nrows();
    let d = x.ncols();
    let k = resp.ncols();

    match cov {
        CovarianceType::Full => {
            let mut out = Vec::with_capacity(k);
            for c in 0..k {
                let denom = nk[c].max(1e-300);
                let mut sigma = Mat::<f64>::zeros(d, d);
                for r in 0..n {
                    let w = resp[(r, c)];
                    for i in 0..d {
                        for j in 0..d {
                            sigma[(i, j)] +=
                                w * (x[(r, i)] - means[(c, i)]) * (x[(r, j)] - means[(c, j)]);
                        }
                    }
                }
                for i in 0..d {
                    for j in 0..d {
                        sigma[(i, j)] /= denom;
                    }
                    sigma[(i, i)] += reg;
                }
                out.push(sigma);
            }
            out
        }
        CovarianceType::Tied => {
            let mut sigma = Mat::<f64>::zeros(d, d);
            for c in 0..k {
                for r in 0..n {
                    let w = resp[(r, c)];
                    for i in 0..d {
                        for j in 0..d {
                            sigma[(i, j)] +=
                                w * (x[(r, i)] - means[(c, i)]) * (x[(r, j)] - means[(c, j)]);
                        }
                    }
                }
            }
            for i in 0..d {
                for j in 0..d {
                    sigma[(i, j)] /= n as f64;
                }
                sigma[(i, i)] += reg;
            }
            vec![sigma]
        }
        CovarianceType::Diag => {
            let mut out = Vec::with_capacity(k);
            for c in 0..k {
                let denom = nk[c].max(1e-300);
                let mut diag = Mat::<f64>::zeros(1, d);
                for r in 0..n {
                    let w = resp[(r, c)];
                    for i in 0..d {
                        let dif = x[(r, i)] - means[(c, i)];
                        diag[(0, i)] += w * dif * dif;
                    }
                }
                for i in 0..d {
                    diag[(0, i)] = diag[(0, i)] / denom + reg;
                }
                out.push(diag);
            }
            out
        }
        CovarianceType::Spherical => {
            let mut out = Vec::with_capacity(k);
            for c in 0..k {
                let denom = nk[c].max(1e-300);
                let mut acc = 0.0;
                for r in 0..n {
                    let w = resp[(r, c)];
                    for i in 0..d {
                        let dif = x[(r, i)] - means[(c, i)];
                        acc += w * dif * dif;
                    }
                }
                let var = acc / (denom * d as f64) + reg;
                let mut s = Mat::<f64>::zeros(1, 1);
                s[(0, 0)] = var;
                out.push(s);
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_blob_data(n_per_cluster: usize, sep: f64) -> (Mat<f64>, Vec<usize>) {
        // Two well-separated isotropic clusters in 2D.
        let n = n_per_cluster * 2;
        let mut x = Mat::<f64>::zeros(n, 2);
        let mut labels = vec![0_usize; n];
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        for i in 0..n_per_cluster {
            let nx: f64 = rng.gen_range(-0.4..0.4);
            let ny: f64 = rng.gen_range(-0.4..0.4);
            x[(i, 0)] = -sep + nx;
            x[(i, 1)] = nx + ny;
            labels[i] = 0;
        }
        for i in 0..n_per_cluster {
            let nx: f64 = rng.gen_range(-0.4..0.4);
            let ny: f64 = rng.gen_range(-0.4..0.4);
            x[(n_per_cluster + i, 0)] = sep + nx;
            x[(n_per_cluster + i, 1)] = nx + ny;
            labels[n_per_cluster + i] = 1;
        }
        (x, labels)
    }

    fn cluster_accuracy(pred: &[usize], truth: &[usize], k: usize) -> f64 {
        // Accuracy under best label permutation. Brute force over k!
        // permutations works for tiny k (k<=4 here).
        let n = pred.len();
        let perms = permutations(k);
        let mut best = 0.0;
        for perm in &perms {
            let mut correct = 0_usize;
            for i in 0..n {
                if perm[pred[i]] == truth[i] {
                    correct += 1;
                }
            }
            let a = correct as f64 / n as f64;
            if a > best {
                best = a;
            }
        }
        best
    }

    fn permutations(k: usize) -> Vec<Vec<usize>> {
        if k == 0 {
            return vec![vec![]];
        }
        let mut out = vec![];
        let smaller = permutations(k - 1);
        for sub in &smaller {
            for pos in 0..=sub.len() {
                let mut new = sub.clone();
                new.insert(pos, k - 1);
                out.push(new);
            }
        }
        out
    }

    #[test]
    fn gmm_recovers_two_separated_blobs() {
        let (x, truth) = two_blob_data(40, 5.0);
        let model = fit(
            x.as_ref(),
            &GmmOptions {
                n_components: 2,
                seed: Some(0),
                n_init: 4,
                ..Default::default()
            },
        )
        .expect("ok");
        assert!(model.converged || model.iterations >= 5);
        let pred = model.predict(x.as_ref()).expect("ok");
        let acc = cluster_accuracy(&pred, &truth, 2);
        assert!(acc >= 0.95, "accuracy {acc}");
    }

    #[test]
    fn gmm_proba_rows_sum_to_one() {
        let (x, _) = two_blob_data(20, 4.0);
        let model = fit(
            x.as_ref(),
            &GmmOptions {
                n_components: 2,
                seed: Some(1),
                ..Default::default()
            },
        )
        .expect("ok");
        let proba = model.predict_proba(x.as_ref()).expect("ok");
        for r in 0..proba.nrows() {
            let s: f64 = (0..proba.ncols()).map(|c| proba[(r, c)]).sum();
            assert!((s - 1.0).abs() < 1e-10, "row {r} sum = {s}");
        }
    }

    #[test]
    fn gmm_diag_covariance_path_runs() {
        let (x, truth) = two_blob_data(30, 4.0);
        let model = fit(
            x.as_ref(),
            &GmmOptions {
                n_components: 2,
                covariance_type: CovarianceType::Diag,
                seed: Some(2),
                ..Default::default()
            },
        )
        .expect("ok");
        let pred = model.predict(x.as_ref()).expect("ok");
        let acc = cluster_accuracy(&pred, &truth, 2);
        assert!(acc >= 0.9, "diag accuracy {acc}");
    }

    #[test]
    fn gmm_spherical_covariance_path_runs() {
        let (x, _) = two_blob_data(20, 5.0);
        let model = fit(
            x.as_ref(),
            &GmmOptions {
                n_components: 2,
                covariance_type: CovarianceType::Spherical,
                seed: Some(3),
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(model.covariances.len(), 2);
        assert_eq!(model.covariances[0].nrows(), 1);
    }

    #[test]
    fn gmm_tied_covariance_path_runs() {
        let (x, _) = two_blob_data(20, 5.0);
        let model = fit(
            x.as_ref(),
            &GmmOptions {
                n_components: 2,
                covariance_type: CovarianceType::Tied,
                seed: Some(4),
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(model.covariances.len(), 1);
    }

    #[test]
    fn gmm_bic_prefers_correct_k_on_two_blob_data() {
        let (x, _) = two_blob_data(30, 5.0);
        let model = fit_with_model_selection(
            x.as_ref(),
            1,
            4,
            &GmmOptions {
                seed: Some(5),
                n_init: 2,
                ..Default::default()
            },
        )
        .expect("ok");
        // BIC should pick k=2 (it's the truth).
        assert_eq!(model.weights.len(), 2);
    }

    #[test]
    fn gmm_rejects_invalid_k() {
        let mut x = Mat::<f64>::zeros(3, 2);
        x[(0, 0)] = 1.0;
        let result = fit(
            x.as_ref(),
            &GmmOptions {
                n_components: 0,
                ..Default::default()
            },
        );
        assert!(matches!(result, Err(GmmError::InvalidK { .. })));
    }
}
