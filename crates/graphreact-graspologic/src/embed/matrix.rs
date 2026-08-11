//! Matrix utilities for spectral embeddings.
//!
//! Provides:
//!
//! * [`adjacency_matrix`] — extract a dense adjacency matrix from a
//!   [`CommunicationGraph`]. Multi-edges sum, self-loops contribute once
//!   to the diagonal — matching NetworkX's `to_numpy_array` defaults.
//! * [`augment_diagonal`] / [`augment_diagonal_in_place`] — replace the
//!   diagonal with `(in_deg + out_deg) / (2 * (n - 1))`. Empirically
//!   improves ASE latent quality.
//! * [`to_laplacian`] — graph Laplacian in `I-DAD`, `DAD`, or `R-DAD`
//!   form, matching graspologic.
//! * [`is_almost_symmetric`] — symmetry check used to decide whether a
//!   graph is directed.
//! * [`average_matrices`] — uniform mean across a slice of matrices.

use faer::{Mat, MatRef};
use rustc_hash::FxHashMap;

use graphreact_core::{Attrs, CommunicationGraph};

use super::error::{EmbedError, EmbedResult};

/// Choice of Laplacian normalization. Mirrors graspologic's `LaplacianFormType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaplacianForm {
    /// Symmetric normalized Laplacian: `L = I − D^{-1/2} A D^{-1/2}`.
    IDad,
    /// Normalized adjacency: `L = D^{-1/2} A D^{-1/2}`. Default in graspologic.
    #[default]
    Dad,
    /// Regularized normalized adjacency: `L = D_r^{-1/2} A D_r^{-1/2}` where
    /// `D_r = D + r·I`. If `regularizer` is `None`, `r = mean(out_degree)`.
    RDad,
}

/// Extract the dense adjacency matrix from a [`CommunicationGraph`].
///
/// Nodes are indexed in graph insertion order (the order produced by
/// [`CommunicationGraph::nodes`]). For multi-edges, the named weight
/// attribute is summed (NetworkX `to_numpy_array` default). Self-loops
/// contribute once to the diagonal.
pub fn adjacency_matrix(graph: &CommunicationGraph, weight_attribute: &str) -> Mat<f64> {
    let n = graph.order();
    let mut idx_of: FxHashMap<String, usize> = FxHashMap::default();
    for (i, id) in graph.nodes().enumerate() {
        idx_of.insert(id.to_string(), i);
    }
    let mut a = Mat::<f64>::zeros(n, n);
    for (u, v, key) in graph.edges() {
        let attrs = graph
            .get_edge_data(&u, &v, Some(&key))
            .expect("edge from edges() must have data");
        let w = edge_weight(attrs, weight_attribute);
        let i = idx_of[&u];
        let j = idx_of[&v];
        a[(i, j)] += w;
        if i != j {
            a[(j, i)] += w;
        }
    }
    a
}

/// Read a numeric edge weight from the named attribute. Falls back to
/// `1.0` when missing or non-numeric.
pub(crate) fn edge_weight(attrs: &Attrs, attr_name: &str) -> f64 {
    #[allow(clippy::redundant_closure_for_method_calls)]
    attrs.get(attr_name).and_then(|v| v.as_f64()).unwrap_or(1.0)
}

/// Replace the matrix diagonal with the per-node mean of `(in + out)`
/// degree (sum of off-diagonal magnitudes), divided by `n - 1`. Matches
/// graspologic's `augment_diagonal` exactly: the original diagonal is
/// ignored — degrees are computed from absolute off-diagonal values.
pub fn augment_diagonal_in_place(a: &mut Mat<f64>) {
    let n = a.nrows();
    if n == 0 {
        return;
    }
    let divisor = ((n - 1) as f64).max(1.0);

    let mut in_degree = vec![0.0_f64; n];
    let mut out_degree = vec![0.0_f64; n];
    for r in 0..n {
        for c in 0..n {
            if r == c {
                continue;
            }
            let v = a[(r, c)].abs();
            out_degree[r] += v;
            in_degree[c] += v;
        }
    }
    for i in 0..n {
        a[(i, i)] = (in_degree[i] + out_degree[i]) / 2.0 / divisor;
    }
}

/// Owned variant of [`augment_diagonal_in_place`].
pub fn augment_diagonal(a: MatRef<'_, f64>) -> Mat<f64> {
    let mut out = a.to_owned();
    augment_diagonal_in_place(&mut out);
    out
}

/// Returns true when `(a − a^T).max_abs ≤ atol`.
pub fn is_almost_symmetric(a: MatRef<'_, f64>, atol: f64) -> bool {
    if a.nrows() != a.ncols() {
        return false;
    }
    for r in 0..a.nrows() {
        for c in (r + 1)..a.ncols() {
            if (a[(r, c)] - a[(c, r)]).abs() > atol {
                return false;
            }
        }
    }
    true
}

/// Compute the graph Laplacian of a square (possibly weighted, possibly
/// directed) adjacency matrix.
pub fn to_laplacian(
    a: MatRef<'_, f64>,
    form: LaplacianForm,
    regularizer: Option<f64>,
) -> EmbedResult<Mat<f64>> {
    let n = a.nrows();
    if n == 0 {
        return Err(EmbedError::Empty);
    }
    if a.ncols() != n {
        return Err(EmbedError::NotSquare {
            rows: n,
            cols: a.ncols(),
        });
    }

    let mut in_degree = vec![0.0_f64; n];
    let mut out_degree = vec![0.0_f64; n];
    for r in 0..n {
        for c in 0..n {
            in_degree[c] += a[(r, c)];
            out_degree[r] += a[(r, c)];
        }
    }

    if matches!(form, LaplacianForm::RDad) {
        let r = match regularizer {
            Some(v) if v < 0.0 => return Err(EmbedError::InvalidRegularizer { value: v }),
            Some(v) => v,
            None => out_degree.iter().sum::<f64>() / n as f64,
        };
        for v in &mut in_degree {
            *v += r;
        }
        for v in &mut out_degree {
            *v += r;
        }
    }

    let in_root: Vec<f64> = in_degree
        .iter()
        .map(|&d| if d > 0.0 { 1.0 / d.sqrt() } else { 0.0 })
        .collect();
    let out_root: Vec<f64> = out_degree
        .iter()
        .map(|&d| if d > 0.0 { 1.0 / d.sqrt() } else { 0.0 })
        .collect();

    let mut l = Mat::<f64>::zeros(n, n);
    match form {
        LaplacianForm::IDad => {
            for r in 0..n {
                for c in 0..n {
                    let pre = if r == c {
                        in_degree[r] - a[(r, c)]
                    } else {
                        -a[(r, c)]
                    };
                    l[(r, c)] = in_root[r] * pre * in_root[c];
                }
            }
        }
        LaplacianForm::Dad | LaplacianForm::RDad => {
            for r in 0..n {
                for c in 0..n {
                    l[(r, c)] = out_root[r] * a[(r, c)] * in_root[c];
                }
            }
        }
    }

    if is_almost_symmetric(a, 1e-15) {
        for r in 0..n {
            for c in (r + 1)..n {
                let avg = (l[(r, c)] + l[(c, r)]) / 2.0;
                l[(r, c)] = avg;
                l[(c, r)] = avg;
            }
        }
    }

    Ok(l)
}

/// Element-wise mean across a non-empty slice of matrices that share the
/// same shape.
pub fn average_matrices(matrices: &[Mat<f64>]) -> EmbedResult<Mat<f64>> {
    if matrices.is_empty() {
        return Err(EmbedError::TooFewGraphs { n: 0 });
    }
    let rows = matrices[0].nrows();
    let cols = matrices[0].ncols();
    for m in matrices.iter().skip(1) {
        if m.nrows() != rows || m.ncols() != cols {
            return Err(EmbedError::ShapeMismatch {
                rows0: rows,
                cols0: cols,
                rows: m.nrows(),
                cols: m.ncols(),
            });
        }
    }
    let denom = matrices.len() as f64;
    let mut out = Mat::<f64>::zeros(rows, cols);
    for m in matrices {
        for r in 0..rows {
            for c in 0..cols {
                out[(r, c)] += m[(r, c)];
            }
        }
    }
    for r in 0..rows {
        for c in 0..cols {
            out[(r, c)] /= denom;
        }
    }
    Ok(out)
}

/// Build a list of adjacency matrices from a list of graphs that share
/// the same node set in the same insertion order.
pub fn adjacency_matrices(
    graphs: &[CommunicationGraph],
    weight_attribute: &str,
) -> EmbedResult<Vec<Mat<f64>>> {
    if graphs.len() < 2 {
        return Err(EmbedError::TooFewGraphs { n: graphs.len() });
    }
    let n0 = graphs[0].order();
    let nodes0: Vec<&str> = graphs[0].nodes().collect();
    for g in graphs.iter().skip(1) {
        if g.order() != n0 {
            return Err(EmbedError::ShapeMismatch {
                rows0: n0,
                cols0: n0,
                rows: g.order(),
                cols: g.order(),
            });
        }
        for (lhs, rhs) in nodes0.iter().zip(g.nodes()) {
            if *lhs != rhs {
                return Err(EmbedError::ShapeMismatch {
                    rows0: n0,
                    cols0: n0,
                    rows: g.order(),
                    cols: g.order(),
                });
            }
        }
    }
    Ok(graphs
        .iter()
        .map(|g| adjacency_matrix(g, weight_attribute))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::Attrs;

    fn weighted_attrs(w: f64) -> Attrs {
        let mut a = Attrs::new();
        a.insert("weight".to_string(), serde_json::json!(w));
        a
    }

    #[test]
    fn adjacency_matrix_symmetric_and_unweighted() {
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        g.add_node("b", Attrs::new());
        g.add_node("c", Attrs::new());
        g.add_edge("a", "b", None, Attrs::new());
        g.add_edge("b", "c", None, Attrs::new());

        let a = adjacency_matrix(&g, "weight");
        assert_eq!(a.nrows(), 3);
        assert_eq!(a[(0, 1)], 1.0);
        assert_eq!(a[(1, 0)], 1.0);
        assert_eq!(a[(1, 2)], 1.0);
        assert_eq!(a[(2, 1)], 1.0);
        assert_eq!(a[(0, 0)], 0.0);
        assert_eq!(a[(0, 2)], 0.0);
    }

    #[test]
    fn adjacency_matrix_sums_multi_edges() {
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        g.add_node("b", Attrs::new());
        g.add_edge("a", "b", None, weighted_attrs(2.0));
        g.add_edge("a", "b", None, weighted_attrs(3.0));

        let a = adjacency_matrix(&g, "weight");
        assert_eq!(a[(0, 1)], 5.0);
        assert_eq!(a[(1, 0)], 5.0);
    }

    #[test]
    fn adjacency_matrix_self_loop_on_diagonal_only_once() {
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        g.add_edge("a", "a", None, weighted_attrs(2.5));

        let a = adjacency_matrix(&g, "weight");
        assert_eq!(a.nrows(), 1);
        assert_eq!(a[(0, 0)], 2.5);
    }

    #[test]
    fn augment_diagonal_replaces_self_loops_with_degree_average() {
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 1)] = 1.0;
        a[(1, 0)] = 1.0;
        a[(0, 2)] = 1.0;
        a[(2, 0)] = 1.0;
        // out_deg = [2, 1, 1], in_deg = [2, 1, 1]; (in+out)/2 = [2, 1, 1]; /(n-1)=2 → [1.0, 0.5, 0.5]
        augment_diagonal_in_place(&mut a);
        assert!((a[(0, 0)] - 1.0).abs() < 1e-12);
        assert!((a[(1, 1)] - 0.5).abs() < 1e-12);
        assert!((a[(2, 2)] - 0.5).abs() < 1e-12);
    }

    #[test]
    fn to_laplacian_dad_sums_to_known_values_on_path_graph() {
        // Path: a - b - c, all weight 1.
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 1)] = 1.0;
        a[(1, 0)] = 1.0;
        a[(1, 2)] = 1.0;
        a[(2, 1)] = 1.0;
        // d = [1, 2, 1]; D^{-1/2} = [1, 1/√2, 1]
        // (D^{-1/2} A D^{-1/2})[0][1] = 1 * 1 * (1/√2) = 1/√2
        let l = to_laplacian(a.as_ref(), LaplacianForm::Dad, None).expect("ok");
        let inv_sqrt2 = 1.0_f64 / 2.0_f64.sqrt();
        assert!((l[(0, 1)] - inv_sqrt2).abs() < 1e-12);
        assert!((l[(1, 0)] - inv_sqrt2).abs() < 1e-12);
        assert!((l[(1, 2)] - inv_sqrt2).abs() < 1e-12);
        assert!((l[(0, 0)]).abs() < 1e-12);
    }

    #[test]
    fn to_laplacian_idad_diagonal_is_one_for_connected_node() {
        // Triangle with all weights 1; degrees = [2, 2, 2].
        let mut a = Mat::<f64>::zeros(3, 3);
        for (i, j) in [(0, 1), (1, 2), (0, 2)] {
            a[(i, j)] = 1.0;
            a[(j, i)] = 1.0;
        }
        let l = to_laplacian(a.as_ref(), LaplacianForm::IDad, None).expect("ok");
        for i in 0..3 {
            assert!((l[(i, i)] - 1.0).abs() < 1e-12);
        }
        // Off-diagonal: -A[i][j] / sqrt(2*2) = -0.5
        assert!((l[(0, 1)] - (-0.5)).abs() < 1e-12);
    }

    #[test]
    fn to_laplacian_rdad_uses_mean_degree_when_regularizer_none() {
        let mut a = Mat::<f64>::zeros(3, 3);
        a[(0, 1)] = 1.0;
        a[(1, 0)] = 1.0;
        a[(1, 2)] = 1.0;
        a[(2, 1)] = 1.0;
        // d = [1, 2, 1]; mean = 4/3; d_r = [1+4/3, 2+4/3, 1+4/3]
        let l = to_laplacian(a.as_ref(), LaplacianForm::RDad, None).expect("ok");
        let mean = 4.0_f64 / 3.0;
        let expected_01 = (1.0_f64 / (1.0 + mean).sqrt()) * 1.0 * (1.0_f64 / (2.0 + mean).sqrt());
        assert!((l[(0, 1)] - expected_01).abs() < 1e-12);
    }

    #[test]
    fn average_matrices_takes_uniform_mean() {
        let mut a = Mat::<f64>::zeros(2, 2);
        a[(0, 0)] = 1.0;
        let mut b = Mat::<f64>::zeros(2, 2);
        b[(0, 0)] = 3.0;
        let avg = average_matrices(&[a, b]).expect("ok");
        assert_eq!(avg[(0, 0)], 2.0);
    }

    #[test]
    fn is_almost_symmetric_detects_asymmetry() {
        let mut a = Mat::<f64>::zeros(2, 2);
        a[(0, 1)] = 1.0;
        a[(1, 0)] = 1.0 + 1e-13;
        assert!(is_almost_symmetric(a.as_ref(), 1e-12));
        assert!(!is_almost_symmetric(a.as_ref(), 1e-15));
    }
}
