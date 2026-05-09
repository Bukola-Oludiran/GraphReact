//! Error types for linear algebra operations.

use thiserror::Error;

/// Errors that can occur in linear algebra operations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LinalgError {
    /// A matrix dimension mismatch — e.g., GEMM with `cols(A) != rows(B)`.
    #[error("dimension mismatch: {context}")]
    DimensionMismatch { context: String },

    /// Requested truncation rank `k` exceeds the matrix dimensions.
    #[error("truncation rank {k} exceeds min({rows}, {cols})")]
    InvalidRank { rows: usize, cols: usize, k: usize },

    /// Matrix is singular and cannot be inverted/solved.
    #[error("matrix is singular or near-singular")]
    Singular,

    /// Input matrix has zero rows or zero columns.
    #[error("empty matrix")]
    Empty,
}

pub type LinalgResult<T> = Result<T, LinalgError>;
