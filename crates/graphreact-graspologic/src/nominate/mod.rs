//! Vertex nomination — given a graph and a "seed" subset, rank the
//! remaining vertices by relevance to the seed.

pub mod spectral_vn;

pub use spectral_vn::{
    spectral_vertex_nomination, spectral_vertex_nomination_from_adjacency,
    spectral_vertex_nomination_matrix, Nomination, NominationEmbedder, NominationError,
    NominationOptions, NominationResult,
};
