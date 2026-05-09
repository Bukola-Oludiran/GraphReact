//! Clustering routines.
//!
//! * [`gmm::fit`] — fixed-k Gaussian Mixture Model via EM.
//! * [`gmm::fit_with_model_selection`] — BIC-driven sweep over `k`.
//!
//! Mirrors the essence of graspologic's `cluster.GaussianCluster` and
//! `cluster.KMeansCluster`, kept Rust-native (no sklearn shim, no
//! pandas DataFrame outputs).

pub mod gmm;

pub use gmm::{
    fit as fit_gmm, fit_with_model_selection as fit_gmm_with_model_selection, CovarianceType,
    GmmError, GmmModel, GmmOptions, GmmResult,
};
