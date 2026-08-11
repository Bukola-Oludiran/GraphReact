// Pedantic numeric lints common in matrix code; allow them at the
// crate level to keep the math readable across the M8/M9/M10 modules.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::similar_names
)]

//! Graspologic-equivalent algorithms for graph statistics and analysis.
//!
//! Built on [`graphreact_linalg`] for spectral methods.
//!
//! * M8 — `partition` (Leiden, modularity)
//! * M9 — `embed` (ASE, LSE, OMNI, MASE, CASE)
//! * M10 — `cluster` (GMM), `inference` (latent-position / latent-distribution),
//!   `match_` (Hungarian / FAQ), `nominate` (spectral VN)

pub mod cluster;
pub mod embed;
pub mod inference;
pub mod match_;
pub mod nominate;
pub mod partition;

pub use cluster::{
    fit_gmm, fit_gmm_with_model_selection, CovarianceType, GmmError, GmmModel, GmmOptions,
    GmmResult,
};
pub use embed::{
    adjacency_matrices, adjacency_matrix, adjacency_spectral_embed,
    adjacency_spectral_embed_matrix, augment_diagonal, average_matrices, covariate_assisted_embed,
    is_almost_symmetric, laplacian_spectral_embed, laplacian_spectral_embed_matrix,
    multiple_adjacency_spectral_embed, omnibus_embed, orthogonal_procrustes, procrustes_distance,
    select_dimension, select_svd, to_laplacian, AseOptions, CaseEmbedding, CaseOptions, EmbedError,
    Embedding, LaplacianForm, LseOptions, MaseEmbedding, MaseOptions, OmniEmbedding, OmniOptions,
    SelectSvd,
};
pub use inference::{
    distance_correlation, latent_distribution_test, latent_position_test, sample_rdpg, DcorrError,
    LatentDistributionError, LatentDistributionOptions, LatentDistributionTest,
    LatentPositionError, LatentPositionOptions, LatentPositionTest,
};
pub use match_::{
    assignment_cost, faq_graph_match, faq_graph_match_seeded, linear_sum_assignment,
    linear_sum_assignment_max, FaqError, FaqMatch, FaqOptions, LapError, LapResult,
};
pub use nominate::{
    spectral_vertex_nomination, spectral_vertex_nomination_from_adjacency,
    spectral_vertex_nomination_matrix, Nomination, NominationEmbedder, NominationError,
    NominationOptions, NominationResult,
};
pub use partition::{
    leiden, modularity, modularity_components, LeidenError, LeidenOptions, ModularityError,
    PartitionMap,
};
