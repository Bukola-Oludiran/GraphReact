//! Hypothesis testing on random dot product graphs.
//!
//! * [`distance_correlation`] — Székely & Rizzo (2007) dCor statistic.
//! * [`latent_position_test`] — semi-paired ASE-based two-sample test
//!   (Tang et al., JCGS 2017).
//! * [`latent_distribution_test`] — permutation-based two-sample test
//!   on the latent distributions, using DCorr / energy distance as
//!   the statistic.

pub mod dcorr;
pub mod latent_distribution;
pub mod latent_position;

pub use dcorr::{distance_correlation, pairwise_distance, DcorrError, DcorrResult};
pub use latent_distribution::{
    latent_distribution_test, LatentDistributionError, LatentDistributionOptions,
    LatentDistributionResult, LatentDistributionTest,
};
pub use latent_position::{
    latent_position_test, sample_rdpg, LatentPositionError, LatentPositionOptions,
    LatentPositionResult, LatentPositionTest,
};
