//! Error types for the spectral-embedding module.

use thiserror::Error;

use graphreact_linalg::LinalgError;

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("input graph or matrix is empty")]
    Empty,

    #[error("input matrix must be square, got ({rows}, {cols})")]
    NotSquare { rows: usize, cols: usize },

    #[error(
        "n_components ({n_components}) must satisfy 1 ≤ n_components ≤ min(shape) ({min_shape})"
    )]
    InvalidNComponents {
        n_components: usize,
        min_shape: usize,
    },

    #[error("must provide at least 2 graphs for multi-graph embedding, got {n}")]
    TooFewGraphs { n: usize },

    #[error(
        "input graphs must share the same shape; first is ({rows0}, {cols0}), found ({rows}, {cols})"
    )]
    ShapeMismatch {
        rows0: usize,
        cols0: usize,
        rows: usize,
        cols: usize,
    },

    #[error("covariate matrix has {cov_rows} rows; expected one row per vertex ({n_vertices})")]
    CovariateMismatch { cov_rows: usize, n_vertices: usize },

    #[error("regularizer must be ≥ 0, got {value}")]
    InvalidRegularizer { value: f64 },

    #[error("alpha must be finite and ≥ 0, got {value}")]
    InvalidAlpha { value: f64 },

    #[error("linalg error: {0}")]
    Linalg(#[from] LinalgError),
}

pub type EmbedResult<T> = Result<T, EmbedError>;
