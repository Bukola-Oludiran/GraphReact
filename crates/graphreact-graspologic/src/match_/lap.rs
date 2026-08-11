//! Linear Assignment Problem (LAP) — Hungarian algorithm.
//!
//! Given an `n × n` cost matrix `C`, finds the permutation `π` that
//! minimises `Σ_i C[i, π(i)]`. Used by [`super::faq`] to project the
//! gradient of the relaxed QAP onto the Birkhoff polytope; equivalent
//! to scipy's `linear_sum_assignment`.
//!
//! The implementation is a textbook O(n³) Hungarian (Kuhn–Munkres):
//! reduce rows and columns, find a maximum matching of zeros via
//! augmenting paths, repeat with extra slack updates until a perfect
//! matching exists.

use faer::MatRef;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LapError {
    #[error("cost matrix must be square, got ({rows}, {cols})")]
    NotSquare { rows: usize, cols: usize },
    #[error("cost matrix is empty")]
    Empty,
    #[error("cost contains a non-finite value at ({row}, {col})")]
    NonFinite { row: usize, col: usize },
}

pub type LapResult<T> = Result<T, LapError>;

/// Solve `min Σ_i C[i, π(i)]` over permutations `π`. Returns the
/// row-to-column assignment as a `Vec<usize>` of length `n`. To
/// **maximize**, pass `-C` (or use [`linear_sum_assignment_max`]).
pub fn linear_sum_assignment(cost: MatRef<'_, f64>) -> LapResult<Vec<usize>> {
    let n = cost.nrows();
    if n == 0 || cost.ncols() == 0 {
        return Err(LapError::Empty);
    }
    if cost.ncols() != n {
        return Err(LapError::NotSquare {
            rows: n,
            cols: cost.ncols(),
        });
    }
    let mut c = vec![vec![0.0_f64; n]; n];
    for r in 0..n {
        for col in 0..n {
            let v = cost[(r, col)];
            if !v.is_finite() {
                return Err(LapError::NonFinite { row: r, col });
            }
            c[r][col] = v;
        }
    }

    // Jonker–Volgenant-style O(n³) using the "potential" formulation.
    // u[i] / v[j] are dual variables; p[j] is the column→row assignment.
    let inf = f64::INFINITY;
    let mut u = vec![0.0_f64; n + 1];
    let mut v = vec![0.0_f64; n + 1];
    let mut p = vec![0_usize; n + 1]; // p[j] = which row claims column j (0 = none)
    let mut way = vec![0_usize; n + 1];

    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0_usize;
        let mut min_v = vec![inf; n + 1];
        let mut used = vec![false; n + 1];
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = inf;
            let mut j1 = 0_usize;
            for j in 1..=n {
                if !used[j] {
                    let cur = c[i0 - 1][j - 1] - u[i0] - v[j];
                    if cur < min_v[j] {
                        min_v[j] = cur;
                        way[j] = j0;
                    }
                    if min_v[j] < delta {
                        delta = min_v[j];
                        j1 = j;
                    }
                }
            }
            for j in 0..=n {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    min_v[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }

    let mut assignment = vec![0_usize; n];
    for j in 1..=n {
        if p[j] != 0 {
            assignment[p[j] - 1] = j - 1;
        }
    }
    Ok(assignment)
}

/// `linear_sum_assignment` for the maximisation objective.
pub fn linear_sum_assignment_max(cost: MatRef<'_, f64>) -> LapResult<Vec<usize>> {
    let n = cost.nrows();
    if n == 0 || cost.ncols() == 0 {
        return Err(LapError::Empty);
    }
    if cost.ncols() != n {
        return Err(LapError::NotSquare {
            rows: n,
            cols: cost.ncols(),
        });
    }
    // Negate.
    let mut neg = faer::Mat::<f64>::zeros(n, n);
    for r in 0..n {
        for col in 0..n {
            neg[(r, col)] = -cost[(r, col)];
        }
    }
    linear_sum_assignment(neg.as_ref())
}

/// Total cost of the assignment `π` against `cost`.
pub fn assignment_cost(cost: MatRef<'_, f64>, assignment: &[usize]) -> f64 {
    let mut total = 0.0;
    for (r, &c) in assignment.iter().enumerate() {
        total += cost[(r, c)];
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use faer::Mat;

    fn build(values: &[&[f64]]) -> Mat<f64> {
        let n = values.len();
        let m = values[0].len();
        let mut c = Mat::<f64>::zeros(n, m);
        for r in 0..n {
            for col in 0..m {
                c[(r, col)] = values[r][col];
            }
        }
        c
    }

    #[test]
    fn identity_assignment_is_optimal_when_diagonal_is_minimal() {
        let c = build(&[&[1.0, 9.0, 9.0], &[9.0, 1.0, 9.0], &[9.0, 9.0, 1.0]]);
        let a = linear_sum_assignment(c.as_ref()).expect("ok");
        assert_eq!(a, vec![0, 1, 2]);
        assert!((assignment_cost(c.as_ref(), &a) - 3.0).abs() < 1e-12);
    }

    #[test]
    fn permutation_assignment_recovered() {
        let c = build(&[&[9.0, 1.0, 9.0], &[9.0, 9.0, 1.0], &[1.0, 9.0, 9.0]]);
        let a = linear_sum_assignment(c.as_ref()).expect("ok");
        assert_eq!(a, vec![1, 2, 0]);
    }

    #[test]
    fn maximisation_negates_cost() {
        let c = build(&[&[1.0, 9.0], &[9.0, 1.0]]);
        let a_min = linear_sum_assignment(c.as_ref()).expect("ok");
        let a_max = linear_sum_assignment_max(c.as_ref()).expect("ok");
        assert_eq!(a_min, vec![0, 1]);
        assert_eq!(a_max, vec![1, 0]);
    }

    #[test]
    fn rejects_non_square() {
        let c = Mat::<f64>::zeros(2, 3);
        assert!(matches!(
            linear_sum_assignment(c.as_ref()),
            Err(LapError::NotSquare { .. })
        ));
    }

    #[test]
    fn rejects_non_finite() {
        let mut c = Mat::<f64>::zeros(2, 2);
        c[(0, 0)] = f64::NAN;
        assert!(matches!(
            linear_sum_assignment(c.as_ref()),
            Err(LapError::NonFinite { .. })
        ));
    }

    #[test]
    fn random_4x4_lower_bound_holds() {
        // Hand-computed example: costs chosen so brute-force minimum is 8.
        let c = build(&[
            &[5.0, 9.0, 1.0, 4.0],
            &[10.0, 3.0, 9.0, 7.0],
            &[2.0, 7.0, 4.0, 8.0],
            &[6.0, 9.0, 3.0, 7.0],
        ]);
        let a = linear_sum_assignment(c.as_ref()).expect("ok");
        let cost = assignment_cost(c.as_ref(), &a);
        // Brute-force check: try all 24 permutations.
        let mut best = f64::INFINITY;
        let perms = [
            [0, 1, 2, 3],
            [0, 1, 3, 2],
            [0, 2, 1, 3],
            [0, 2, 3, 1],
            [0, 3, 1, 2],
            [0, 3, 2, 1],
            [1, 0, 2, 3],
            [1, 0, 3, 2],
            [1, 2, 0, 3],
            [1, 2, 3, 0],
            [1, 3, 0, 2],
            [1, 3, 2, 0],
            [2, 0, 1, 3],
            [2, 0, 3, 1],
            [2, 1, 0, 3],
            [2, 1, 3, 0],
            [2, 3, 0, 1],
            [2, 3, 1, 0],
            [3, 0, 1, 2],
            [3, 0, 2, 1],
            [3, 1, 0, 2],
            [3, 1, 2, 0],
            [3, 2, 0, 1],
            [3, 2, 1, 0],
        ];
        for p in perms {
            let mut s = 0.0;
            for i in 0..4 {
                s += c[(i, p[i])];
            }
            if s < best {
                best = s;
            }
        }
        assert!((cost - best).abs() < 1e-12, "got {cost}, expected {best}");
    }
}
