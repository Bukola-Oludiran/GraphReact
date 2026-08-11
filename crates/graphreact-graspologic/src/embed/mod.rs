// Pedantic-level lints common in numerical code; allow them throughout
// the embed module to keep the math readable.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::similar_names
)]

//! Spectral embeddings (graspologic P1).
//!
//! Five embedding routines built on faer-backed dense linear algebra:
//!
//! * [`adjacency_spectral_embed`] / [`adjacency_spectral_embed_matrix`] — ASE
//! * [`laplacian_spectral_embed`] / [`laplacian_spectral_embed_matrix`] — LSE
//! * [`omnibus_embed`] — joint embedding of `m` graphs (OMNI)
//! * [`multiple_adjacency_spectral_embed`] — joint embedding via two-stage SVD (MASE)
//! * [`covariate_assisted_embed`] — graph + per-vertex covariates (CASE)
//!
//! Plus shared infrastructure:
//!
//! * [`adjacency_matrix`], [`augment_diagonal`], [`to_laplacian`] —
//!   matrix helpers reused by callers wanting to mix-and-match.
//! * [`select_svd`], [`select_dimension`] — Zhu & Ghodsi profile-likelihood
//!   elbow detection over singular spectra.
//! * [`procrustes_distance`], [`orthogonal_procrustes`] — alignment
//!   infrastructure used by tests and downstream comparison routines.

pub mod ase;
pub mod case;
pub mod error;
pub mod lse;
pub mod mase;
pub mod matrix;
pub mod omni;
pub mod procrustes;
pub mod select;

pub use ase::{adjacency_spectral_embed, adjacency_spectral_embed_matrix, AseOptions, Embedding};
pub use case::{
    covariate_assisted_embed, covariate_assisted_embed_matrix, CaseEmbedding, CaseOptions,
};
pub use error::{EmbedError, EmbedResult};
pub use lse::{laplacian_spectral_embed, laplacian_spectral_embed_matrix, LseOptions};
pub use mase::{multiple_adjacency_spectral_embed, MaseEmbedding, MaseOptions};
pub use matrix::{
    adjacency_matrices, adjacency_matrix, augment_diagonal, augment_diagonal_in_place,
    average_matrices, is_almost_symmetric, to_laplacian, LaplacianForm,
};
pub use omni::{omnibus_embed, omnibus_embed_full, OmniEmbedding, OmniOptions};
pub use procrustes::{orthogonal_procrustes, procrustes_distance};
pub use select::{select_dimension, select_svd, SelectSvd};
