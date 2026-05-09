//! Linear algebra kernel for GraphReact.
//!
//! Pure Rust, built on [`faer`]. No WASM boundary, no flat-array marshalling —
//! callers pass `faer::MatRef`/`Mat` and get strongly-typed results back.
//!
//! Five operations:
//!
//! * [`svd`] — truncated singular value decomposition
//! * [`eigh`] — symmetric eigendecomposition (largest-k)
//! * [`gemm`] — general dense matrix multiply
//! * [`solve`] — solve Ax = b
//! * [`normalize_rows`] — L2 row normalization

pub mod eigh;
pub mod error;
pub mod gemm;
pub mod normalize;
pub mod solve;
pub mod svd;

pub use eigh::{eigh, EighDecomp};
pub use error::LinalgError;
pub use gemm::gemm;
pub use normalize::normalize_rows;
pub use solve::solve;
pub use svd::{svd, SvdDecomp};
