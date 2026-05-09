//! Laplacian Spectral Embedding.
//!
//! Same SVD-on-a-matrix shape as ASE, but operates on a graph
//! Laplacian (DAD by default) rather than the raw adjacency. Reference:
//! Rohe, Chatterjee, Yu — "Spectral clustering and the high-dimensional
//! stochastic blockmodel", Annals of Statistics, 2011.

use faer::MatRef;

use graphreact_core::CommunicationGraph;

use super::ase::{embedding_from_svd, Embedding};
use super::error::{EmbedError, EmbedResult};
use super::matrix::{adjacency_matrix, is_almost_symmetric, to_laplacian, LaplacianForm};
use super::select::select_svd;

#[derive(Debug, Clone)]
pub struct LseOptions {
    pub n_components: Option<usize>,
    pub n_elbows: usize,
    pub form: LaplacianForm,
    pub regularizer: Option<f64>,
    pub weight_attribute: String,
}

impl Default for LseOptions {
    fn default() -> Self {
        Self {
            n_components: None,
            n_elbows: 2,
            form: LaplacianForm::Dad,
            regularizer: None,
            weight_attribute: "weight".to_string(),
        }
    }
}

/// LSE on a [`CommunicationGraph`].
pub fn laplacian_spectral_embed(
    graph: &CommunicationGraph,
    opts: &LseOptions,
) -> EmbedResult<Embedding> {
    if graph.order() == 0 {
        return Err(EmbedError::Empty);
    }
    let a = adjacency_matrix(graph, &opts.weight_attribute);
    let l = to_laplacian(a.as_ref(), opts.form, opts.regularizer)?;
    laplacian_spectral_embed_matrix(l.as_ref(), opts.n_components, opts.n_elbows, None)
}

/// LSE on a pre-built Laplacian matrix.
pub fn laplacian_spectral_embed_matrix(
    l: MatRef<'_, f64>,
    n_components: Option<usize>,
    n_elbows: usize,
    directed: Option<bool>,
) -> EmbedResult<Embedding> {
    let svd = select_svd(l, n_components, n_elbows)?;
    let directed = directed.unwrap_or_else(|| !is_almost_symmetric(l, 1e-15));
    Ok(embedding_from_svd(svd, directed))
}

#[cfg(test)]
mod tests {
    use super::super::matrix::adjacency_matrix;
    use super::*;
    use graphreact_core::Attrs;

    #[test]
    fn lse_two_clique_graph_separates_blocks() {
        let mut g = CommunicationGraph::new();
        for i in 0..6 {
            g.add_node(format!("n{}", i), Attrs::new());
        }
        for &(u, v) in &[
            ("n0", "n1"),
            ("n0", "n2"),
            ("n1", "n2"),
            ("n3", "n4"),
            ("n3", "n5"),
            ("n4", "n5"),
        ] {
            g.add_edge(u, v, None, Attrs::new());
        }
        let emb = laplacian_spectral_embed(
            &g,
            &LseOptions {
                n_components: Some(2),
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(emb.latent_left.nrows(), 6);
        assert_eq!(emb.latent_left.ncols(), 2);

        // Within-clique rows must be identical (each node in a triangle
        // has the same Laplacian row); between cliques must differ.
        for i in 1..3 {
            for c in 0..2 {
                assert!((emb.latent_left[(0, c)] - emb.latent_left[(i, c)]).abs() < 1e-10);
            }
        }
        let mut between = 0.0;
        for c in 0..2 {
            between += (emb.latent_left[(0, c)] - emb.latent_left[(3, c)]).abs();
        }
        assert!(between > 0.0);
    }

    #[test]
    fn lse_dad_known_singular_value_on_path_graph() {
        // Path: a-b-c (n=3). DAD eigenvalues are {1, 0, -1}, so singular
        // values are {1, 1, 0} sorted desc.
        let mut g = CommunicationGraph::new();
        g.add_node("a", Attrs::new());
        g.add_node("b", Attrs::new());
        g.add_node("c", Attrs::new());
        g.add_edge("a", "b", None, Attrs::new());
        g.add_edge("b", "c", None, Attrs::new());

        let emb = laplacian_spectral_embed(
            &g,
            &LseOptions {
                n_components: Some(3),
                form: LaplacianForm::Dad,
                ..Default::default()
            },
        )
        .expect("ok");
        let mut sv = emb.singular_values.clone();
        sv.sort_by(|a, b| b.partial_cmp(a).unwrap());
        assert!((sv[0] - 1.0).abs() < 1e-10);
        assert!((sv[1] - 1.0).abs() < 1e-10);
        assert!((sv[2]).abs() < 1e-10);
    }

    #[test]
    fn lse_rdad_does_not_panic_on_isolated_nodes() {
        let mut g = CommunicationGraph::new();
        for i in 0..4 {
            g.add_node(format!("n{}", i), Attrs::new());
        }
        // Only 2 of 4 are connected.
        g.add_edge("n0", "n1", None, Attrs::new());

        let emb = laplacian_spectral_embed(
            &g,
            &LseOptions {
                n_components: Some(2),
                form: LaplacianForm::RDad,
                regularizer: None,
                ..Default::default()
            },
        )
        .expect("ok");
        assert_eq!(emb.latent_left.nrows(), 4);
    }

    #[test]
    fn lse_matrix_input_matches_graph_input() {
        let mut g = CommunicationGraph::new();
        for i in 0..3 {
            g.add_node(format!("n{}", i), Attrs::new());
        }
        g.add_edge("n0", "n1", None, Attrs::new());
        g.add_edge("n1", "n2", None, Attrs::new());

        let from_graph = laplacian_spectral_embed(
            &g,
            &LseOptions {
                n_components: Some(2),
                form: LaplacianForm::Dad,
                ..Default::default()
            },
        )
        .expect("ok");

        let a = adjacency_matrix(&g, "weight");
        let l = to_laplacian(a.as_ref(), LaplacianForm::Dad, None).expect("ok");
        let from_matrix =
            laplacian_spectral_embed_matrix(l.as_ref(), Some(2), 1, Some(false))
                .expect("ok");
        for (a, b) in from_graph
            .singular_values
            .iter()
            .zip(from_matrix.singular_values.iter())
        {
            assert!((a - b).abs() < 1e-12);
        }
    }
}
