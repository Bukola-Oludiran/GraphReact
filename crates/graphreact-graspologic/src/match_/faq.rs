//! Fast Approximate QAP (FAQ) — graph matching by Frank–Wolfe over the
//! Birkhoff polytope.
//!
//! Given two `n × n` adjacency matrices `A` and `B`, find a permutation
//! matrix `P` that maximises (or minimises) the inner product
//! `⟨A, P B Pᵀ⟩_F`. The QAP is NP-hard; FAQ relaxes the integer
//! constraint to doubly-stochastic matrices, runs Frank–Wolfe with the
//! Hungarian algorithm choosing the descent direction, then projects
//! the relaxed solution back to a permutation.
//!
//! Reference: Vogelstein et al., "Fast Approximate Quadratic
//! Programming for Graph Matching", PLOS ONE 2015.
//!
//! ## Implementation note (feature parity, not line-by-line)
//!
//! graspologic's `_GraphMatchSolver` supports multilayer graphs,
//! similarity matrices, partial matches, Sinkhorn-transport gradients,
//! and seeded initialisation. This Rust port covers the **core
//! single-layer FAQ** — sufficient for graph-matching parity on the
//! simple case and to feed [`super::super::nominate::vn_via_sgm`] in
//! a future expansion. Multilayer / Sinkhorn / partial-match are
//! deferred until a real consumer demands them, matching the
//! "feature parity ≠ line-by-line" stance.

use faer::{Mat, MatRef};
use rand::seq::SliceRandom;
use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use thiserror::Error;

use super::lap::{linear_sum_assignment, linear_sum_assignment_max, LapError};

#[derive(Debug, Error)]
pub enum FaqError {
    #[error("input adjacency matrices must be square and the same shape")]
    ShapeMismatch,
    #[error("matrices contain a non-finite value")]
    NonFinite,
    #[error("LAP solver failed: {0}")]
    Lap(#[from] LapError),
}

pub type FaqResult<T> = Result<T, FaqError>;

#[derive(Debug, Clone)]
pub struct FaqOptions {
    /// Maximize `⟨A, PBP^T⟩` (true) or minimize (false). Default `true`.
    pub maximize: bool,
    /// Maximum Frank–Wolfe iterations.
    pub max_iter: usize,
    /// Convergence threshold on the iterate's Frobenius change.
    pub tol: f64,
    /// PRNG seed for the doubly-stochastic perturbation around `J/n`.
    /// `None` derives from thread RNG. Determinism is recommended for
    /// reproducible matches.
    pub seed: Option<u64>,
    /// Number of random restarts; the best objective wins. Default `1`.
    pub n_init: usize,
}

impl Default for FaqOptions {
    fn default() -> Self {
        Self {
            maximize: true,
            max_iter: 30,
            tol: 0.03,
            seed: None,
            n_init: 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FaqMatch {
    /// Row → column permutation: `row i` of `A` aligns to `permutation[i]`
    /// of `B`.
    pub permutation: Vec<usize>,
    /// Final objective value `⟨A, PBP^T⟩`.
    pub objective: f64,
    /// Frank–Wolfe iterations actually executed.
    pub iterations: usize,
}

/// Run FAQ on a square `(n × n)` adjacency pair.
pub fn faq_graph_match(
    a: MatRef<'_, f64>,
    b: MatRef<'_, f64>,
    opts: &FaqOptions,
) -> FaqResult<FaqMatch> {
    let n = a.nrows();
    if n == 0 || a.ncols() != n || b.nrows() != n || b.ncols() != n {
        return Err(FaqError::ShapeMismatch);
    }
    for r in 0..n {
        for c in 0..n {
            if !a[(r, c)].is_finite() || !b[(r, c)].is_finite() {
                return Err(FaqError::NonFinite);
            }
        }
    }

    let mut best: Option<FaqMatch> = None;
    let mut rng = match opts.seed {
        Some(s) => ChaCha8Rng::seed_from_u64(s),
        None => ChaCha8Rng::from_entropy(),
    };

    for _ in 0..opts.n_init.max(1) {
        let p_init = barycentric_init(n, &mut rng, 0.1);
        let m = run_frank_wolfe(a, b, p_init.as_ref(), opts)?;
        match &best {
            None => best = Some(m),
            Some(prev) => {
                let prev_obj = prev.objective;
                let cur = if opts.maximize {
                    m.objective > prev_obj
                } else {
                    m.objective < prev_obj
                };
                if cur {
                    best = Some(m);
                }
            }
        }
    }
    Ok(best.expect("n_init ≥ 1 produces at least one candidate"))
}

/// Initialise from an explicit permutation (1-init Frank–Wolfe). Useful
/// for seeded starts and as a building block for vertex-nomination.
pub fn faq_graph_match_seeded(
    a: MatRef<'_, f64>,
    b: MatRef<'_, f64>,
    seed_permutation: &[usize],
    opts: &FaqOptions,
) -> FaqResult<FaqMatch> {
    let n = a.nrows();
    if seed_permutation.len() != n {
        return Err(FaqError::ShapeMismatch);
    }
    if a.ncols() != n || b.nrows() != n || b.ncols() != n {
        return Err(FaqError::ShapeMismatch);
    }
    let p = perm_to_doubly_stochastic(seed_permutation, n);
    run_frank_wolfe(a, b, p.as_ref(), opts)
}

fn run_frank_wolfe(
    a: MatRef<'_, f64>,
    b: MatRef<'_, f64>,
    init: MatRef<'_, f64>,
    opts: &FaqOptions,
) -> FaqResult<FaqMatch> {
    let n = a.nrows();
    let mut p = init.to_owned();
    let mut prev = p.clone();
    let mut iters = 0_usize;

    for _ in 0..opts.max_iter.max(1) {
        iters += 1;
        // Gradient of f(P) = ⟨A, P B Pᵀ⟩: ∇f = A P B^T + Aᵀ P B
        let grad = compute_gradient(a, b, p.as_ref());
        // Direction: solve LAP on (-grad) for max, grad for min.
        // Build `n × n` cost from the gradient (ascend = LAP-min on -grad).
        let direction = if opts.maximize {
            linear_sum_assignment_max(grad.as_ref())?
        } else {
            linear_sum_assignment(grad.as_ref())?
        };
        let q = perm_to_doubly_stochastic(&direction, n);
        // Step size: closed-form optimum of the quadratic in alpha
        // f(P + α(Q − P)) = f(P) + α·⟨∇f, Q−P⟩ + α²·⟨A(Q−P), (Q−P)B⟩
        let alpha = optimal_step_size(a, b, p.as_ref(), q.as_ref(), &grad, opts.maximize);
        let alpha = alpha.clamp(0.0, 1.0);
        let mut next = Mat::<f64>::zeros(n, n);
        for r in 0..n {
            for c in 0..n {
                next[(r, c)] = (1.0 - alpha) * p[(r, c)] + alpha * q[(r, c)];
            }
        }

        // Convergence: Frobenius change small.
        let mut diff_sq = 0.0;
        for r in 0..n {
            for c in 0..n {
                let d = next[(r, c)] - p[(r, c)];
                diff_sq += d * d;
            }
        }
        prev.clone_from(&p);
        p = next;
        if diff_sq.sqrt() < opts.tol {
            break;
        }
        let _ = &prev;
    }

    // Project onto a permutation: pick the permutation matrix with
    // largest inner product against the doubly-stochastic P (equivalent
    // to LAP-max regardless of objective sign — projection is the same
    // operation either way).
    let _ = opts.maximize;
    let proj = linear_sum_assignment_max(p.as_ref())?;
    let objective = qap_objective(a, b, &proj);
    Ok(FaqMatch {
        permutation: proj,
        objective,
        iterations: iters,
    })
}

fn compute_gradient(a: MatRef<'_, f64>, b: MatRef<'_, f64>, p: MatRef<'_, f64>) -> Mat<f64> {
    let n = a.nrows();
    // ∇f(P) = A P Bᵀ + Aᵀ P B
    // Compute A P
    let ap = mul(a, p);
    // (A P) Bᵀ
    let apb_t = mul_with_transposed(ap.as_ref(), b);
    // Aᵀ P
    let at_p = mul_transposed_lhs(a, p);
    // (Aᵀ P) B
    let atpb = mul(at_p.as_ref(), b);
    let mut g = Mat::<f64>::zeros(n, n);
    for r in 0..n {
        for c in 0..n {
            g[(r, c)] = apb_t[(r, c)] + atpb[(r, c)];
        }
    }
    g
}

fn optimal_step_size(
    a: MatRef<'_, f64>,
    b: MatRef<'_, f64>,
    p: MatRef<'_, f64>,
    q: MatRef<'_, f64>,
    grad: &Mat<f64>,
    maximize: bool,
) -> f64 {
    // Let D = Q − P. The objective along the line is
    //   g(α) = f(P) + α·c1 + α²·c2,
    //   c1 = ⟨∇f, D⟩,  c2 = ⟨A D Bᵀ, D⟩.
    // Critical point α* = −c1/(2c2). For maximisation, choose α ∈ [0,1]
    // that *maximises* g; for minimisation, that minimises g. Compare
    // α=0, α=1, and α* (if interior) explicitly to handle every sign
    // combination.
    let n = a.nrows();
    let mut d = Mat::<f64>::zeros(n, n);
    for r in 0..n {
        for c in 0..n {
            d[(r, c)] = q[(r, c)] - p[(r, c)];
        }
    }
    let mut c1 = 0.0;
    for r in 0..n {
        for c in 0..n {
            c1 += grad[(r, c)] * d[(r, c)];
        }
    }
    let ad = mul(a, d.as_ref());
    let adb_t = mul_with_transposed(ad.as_ref(), b);
    let mut c2 = 0.0;
    for r in 0..n {
        for c in 0..n {
            c2 += adb_t[(r, c)] * d[(r, c)];
        }
    }
    let alpha_one = 1.0_f64;
    let val_one = c1 + c2;
    let mut candidates: Vec<(f64, f64)> = vec![(0.0, 0.0), (alpha_one, val_one)];
    if c2.abs() > 1e-12 {
        let alpha_star = -c1 / (2.0 * c2);
        if alpha_star > 0.0 && alpha_star < 1.0 {
            let v = alpha_star * c1 + alpha_star * alpha_star * c2;
            candidates.push((alpha_star, v));
        }
    }
    let cmp = |x: &(f64, f64), y: &(f64, f64)| {
        if maximize {
            x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal)
        } else {
            y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal)
        }
    };
    candidates.sort_by(cmp);
    candidates.last().map(|(a, _)| *a).unwrap_or(0.0)
}

fn perm_to_doubly_stochastic(perm: &[usize], n: usize) -> Mat<f64> {
    let mut p = Mat::<f64>::zeros(n, n);
    for (r, &c) in perm.iter().enumerate() {
        p[(r, c)] = 1.0;
    }
    p
}

fn barycentric_init(n: usize, rng: &mut impl Rng, perturbation: f64) -> Mat<f64> {
    // Start at the barycenter J/n then mix in a random doubly-stochastic
    // perturbation so different `n_init` runs explore different basins.
    let mut p = Mat::<f64>::zeros(n, n);
    let base = 1.0 / n as f64;
    let mut perm: Vec<usize> = (0..n).collect();
    perm.shuffle(rng);
    let pert = perturbation.clamp(0.0, 0.5);
    for r in 0..n {
        for c in 0..n {
            p[(r, c)] = (1.0 - pert) * base;
        }
    }
    for (r, &c) in perm.iter().enumerate() {
        p[(r, c)] += pert;
    }
    p
}

fn qap_objective(a: MatRef<'_, f64>, b: MatRef<'_, f64>, perm: &[usize]) -> f64 {
    let n = a.nrows();
    let mut s = 0.0;
    for i in 0..n {
        for j in 0..n {
            s += a[(i, j)] * b[(perm[i], perm[j])];
        }
    }
    s
}

fn mul(a: MatRef<'_, f64>, b: MatRef<'_, f64>) -> Mat<f64> {
    let m = a.nrows();
    let k = a.ncols();
    let n = b.ncols();
    let mut out = Mat::<f64>::zeros(m, n);
    for r in 0..m {
        for c in 0..n {
            let mut acc = 0.0;
            for i in 0..k {
                acc += a[(r, i)] * b[(i, c)];
            }
            out[(r, c)] = acc;
        }
    }
    out
}

fn mul_with_transposed(a: MatRef<'_, f64>, b_t: MatRef<'_, f64>) -> Mat<f64> {
    // Computes A · Bᵀ where b_t is in *column-row* layout (i.e. we want
    // A row r times B row c).
    let m = a.nrows();
    let k = a.ncols();
    let n = b_t.nrows();
    let mut out = Mat::<f64>::zeros(m, n);
    for r in 0..m {
        for c in 0..n {
            let mut acc = 0.0;
            for i in 0..k {
                acc += a[(r, i)] * b_t[(c, i)];
            }
            out[(r, c)] = acc;
        }
    }
    out
}

fn mul_transposed_lhs(a_t: MatRef<'_, f64>, b: MatRef<'_, f64>) -> Mat<f64> {
    // Aᵀ · B
    let m = a_t.ncols();
    let k = a_t.nrows();
    let n = b.ncols();
    let mut out = Mat::<f64>::zeros(m, n);
    for r in 0..m {
        for c in 0..n {
            let mut acc = 0.0;
            for i in 0..k {
                acc += a_t[(i, r)] * b[(i, c)];
            }
            out[(r, c)] = acc;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(n: usize) -> Mat<f64> {
        let mut a = Mat::<f64>::zeros(n, n);
        for i in 0..n {
            let j = (i + 1) % n;
            a[(i, j)] = 1.0;
            a[(j, i)] = 1.0;
        }
        a
    }

    fn permute(a: MatRef<'_, f64>, perm: &[usize]) -> Mat<f64> {
        let n = a.nrows();
        let mut out = Mat::<f64>::zeros(n, n);
        for r in 0..n {
            for c in 0..n {
                out[(perm[r], perm[c])] = a[(r, c)];
            }
        }
        out
    }

    #[test]
    fn faq_recovers_identity_for_isomorphic_inputs() {
        // FAQ is a Frank–Wolfe heuristic on a non-convex objective
        // and can land in local maxima on highly-symmetric inputs
        // (ring graphs have multiple non-automorphism perms preserving
        // half the edges). Use multiple restarts to give it a fair chance.
        let a = ring(6);
        let m = faq_graph_match(
            a.as_ref(),
            a.as_ref(),
            &FaqOptions {
                seed: Some(0),
                n_init: 8,
                ..Default::default()
            },
        )
        .expect("ok");
        let self_obj: f64 = (0..6)
            .map(|i| (0..6).map(|j| a[(i, j)] * a[(i, j)]).sum::<f64>())
            .sum();
        assert!(
            (m.objective - self_obj).abs() < 1e-6,
            "got {} vs upper {self_obj}",
            m.objective
        );
    }

    #[test]
    fn faq_recovers_known_permutation_on_ring() {
        let a = ring(8);
        let perm_truth: Vec<usize> = vec![3, 4, 5, 6, 7, 0, 1, 2];
        let b = permute(a.as_ref(), &perm_truth);
        let m = faq_graph_match(
            a.as_ref(),
            b.as_ref(),
            &FaqOptions {
                seed: Some(7),
                n_init: 4,
                ..Default::default()
            },
        )
        .expect("ok");
        // FAQ is a heuristic — verify the recovered objective hits the
        // upper bound (full-graph self-similarity).
        let upper: f64 = (0..8)
            .map(|i| (0..8).map(|j| a[(i, j)] * a[(i, j)]).sum::<f64>())
            .sum();
        assert!((m.objective - upper).abs() < 1e-6, "{} vs {}", m.objective, upper);
    }

    #[test]
    fn faq_seeded_init_runs_to_completion() {
        let a = ring(5);
        let b = ring(5);
        let m = faq_graph_match_seeded(
            a.as_ref(),
            b.as_ref(),
            &[0, 1, 2, 3, 4],
            &FaqOptions {
                seed: Some(0),
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(m.permutation.len(), 5);
        assert!(m.iterations >= 1);
    }

    #[test]
    fn faq_rejects_shape_mismatch() {
        let a = ring(4);
        let b = ring(5);
        let result = faq_graph_match(a.as_ref(), b.as_ref(), &FaqOptions::default());
        assert!(matches!(result, Err(FaqError::ShapeMismatch)));
    }

    #[test]
    fn faq_minimization_complementary_to_max_on_complete_graphs() {
        // On a complete graph (J − I), all permutations give identical
        // objective — sanity check the minimisation path returns a
        // valid permutation without crashing.
        let n = 5;
        let mut a = Mat::<f64>::zeros(n, n);
        for r in 0..n {
            for c in 0..n {
                if r != c {
                    a[(r, c)] = 1.0;
                }
            }
        }
        let m = faq_graph_match(
            a.as_ref(),
            a.as_ref(),
            &FaqOptions {
                seed: Some(0),
                maximize: false,
                ..Default::default()
            },
        )
        .expect("ok");
        let mut sorted = m.permutation.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..n).collect::<Vec<_>>(), "must be a permutation");
    }

    #[test]
    fn faq_n_init_picks_best_objective() {
        let a = ring(6);
        let perm_truth: Vec<usize> = vec![2, 3, 4, 5, 0, 1];
        let b = permute(a.as_ref(), &perm_truth);
        let m = faq_graph_match(
            a.as_ref(),
            b.as_ref(),
            &FaqOptions {
                seed: Some(42),
                n_init: 6,
                ..Default::default()
            },
        )
        .expect("ok");
        // Should reach the upper bound on at least one of 6 restarts.
        let upper: f64 = (0..6)
            .map(|i| (0..6).map(|j| a[(i, j)] * a[(i, j)]).sum::<f64>())
            .sum();
        assert!((m.objective - upper).abs() < 1e-6);
    }

}
