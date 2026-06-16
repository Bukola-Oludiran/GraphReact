//! Build-step codegen for Go bodies in `.grx` files —
//! **M17 phase 3b PLANNED, substrate-only scaffold**.
//!
//! Per [ADR-0005](../../../docs/decisions/0005-third-runtime-is-go-not-jvm.md)
//! locked decision #3 and the [later-milestone strategic plan](../../../../../.claude/plans/graphreact-rust-later-milestones.md)
//! §M17, Go has no runtime `eval` (the
//! [`graphreact_engine_zig::Engine::eval_module`] arm for Go cannot
//! mirror M13's HOOK_PRELUDE-at-eval-time pattern). Component bodies
//! must therefore be **pre-compiled** via a build step that:
//!
//! 1. Reads each `.grx` file's `lang="go"` definitions.
//! 2. Emits a sibling `_grx_prelude.go` carrying typed `UseState` /
//!    `UseEffect` / `UseEmit` / `UseMessage` / `UseNeighbors` /
//!    `UseGraphQuery` Tier 1/2/3 / `UseSpawnChild` / `UseUnmountSelf` /
//!    `UseCompose` / `UseAcceptEdge` / `UseEdgeAcceptHandler` Go funcs
//!    that delegate into cgo trampolines.
//! 3. Invokes `go build -buildmode=c-shared` against the combined
//!    source set, producing the `.so` / `.dylib` artifact
//!    [`graphreact-engine-go`](../../graphreact-engine-go/) loads at
//!    daemon mount time.
//!
//! Authors write plain Go in their `.grx` bodies; the `grx build`
//! pipeline composes the prelude + body and produces the c-shared
//! artifact. `init()` registration APIs, struct-tag macros, and
//! `go:generate` annotations are considered and explicitly rejected
//! per ADR-0005.
//!
//! # Phase status (M17)
//!
//! - **Phase 3a (parser-side):** shipped in this commit —
//!   [`graphreact_grx_parser::ast::ComponentLang::Go`] variant
//!   recognises `lang="go" | "golang"` bodies and routes them through
//!   the parser path.
//! - **Phase 3b (codegen-side):** this crate. **Scaffold only.** The
//!   [`Codegen::emit_prelude`] and [`Codegen::build_artifact`] methods
//!   return [`CodegenError::NotYetImplemented`] until the M17 phase 3b
//!   implementation session opens.
//!
//! [`graphreact-engine-go`]: ../../graphreact-engine-go/

#![allow(clippy::module_name_repetitions)]

use std::path::{Path, PathBuf};

use graphreact_grx_parser::ir::GrxIr;
#[cfg(test)]
use graphreact_grx_parser::SourceLoc;
use thiserror::Error;

/// Errors surfaced by the M17 build-step codegen pipeline.
#[derive(Debug, Error)]
pub enum CodegenError {
    /// Returned by every entry point until M17 phase 3b ships the
    /// real codegen + `go build -buildmode=c-shared` orchestration.
    #[error(
        "graphreact-codegen-go::{0} not yet implemented \
         (M17 phase 3b substrate-only scaffold). \
         See ~/.claude/plans/graphreact-rust-later-milestones.md §M17 \
         and docs/decisions/0005-third-runtime-is-go-not-jvm.md."
    )]
    NotYetImplemented(&'static str),
}

/// Output of a successful M17 phase 3b build — a c-shared artifact
/// plus its companion C header, ready for the daemon's
/// [`libloading`]-based loader (M17 phase 1).
///
/// [`libloading`]: https://crates.io/crates/libloading
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildArtifact {
    /// Path to the produced `.so` / `.dylib` / `.dll`.
    pub library_path: PathBuf,
    /// Path to the produced C header (`.h`). Optional in some Go
    /// toolchain configurations; populated whenever `go build`
    /// emits one alongside the library.
    pub header_path: Option<PathBuf>,
    /// SHA-256 of the library bytes, encoded as lowercase hex. Used
    /// by M17 phase 3g hot-reload to short-circuit re-load when the
    /// artifact is unchanged.
    pub library_sha256_hex: String,
}

/// Codegen orchestrator. The public API surface is intentionally
/// minimal at the scaffold tier; M17 phase 3b will expand
/// [`emit_prelude`] + [`build_artifact`] into real implementations.
///
/// [`emit_prelude`]: Codegen::emit_prelude
/// [`build_artifact`]: Codegen::build_artifact
#[derive(Default)]
pub struct Codegen;

impl Codegen {
    /// Construct a fresh `Codegen`. Cheap; never fails.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Read the Go-tagged definitions from `ir` and write a
    /// `_grx_prelude.go` carrying the typed `UseState` / `UseEffect`
    /// / etc. Go funcs alongside the body sources at `out_dir`.
    ///
    /// # Errors
    ///
    /// Always returns [`CodegenError::NotYetImplemented`] until the
    /// M17 phase 3b implementation session lands. The signature is
    /// stable so downstream callers can compose against it today.
    pub fn emit_prelude(&self, _ir: &GrxIr, _out_dir: &Path) -> Result<(), CodegenError> {
        Err(CodegenError::NotYetImplemented("emit_prelude"))
    }

    /// Invoke `go build -buildmode=c-shared` against the source set
    /// at `src_dir` and return the produced [`BuildArtifact`].
    ///
    /// # Errors
    ///
    /// Always returns [`CodegenError::NotYetImplemented`] until the
    /// M17 phase 3b implementation session lands. The signature is
    /// stable so downstream callers can compose against it today.
    pub fn build_artifact(&self, _src_dir: &Path) -> Result<BuildArtifact, CodegenError> {
        Err(CodegenError::NotYetImplemented("build_artifact"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn empty_ir() -> GrxIr {
        GrxIr {
            graph_id: String::new(),
            graph_loc: SourceLoc::new(1, 1),
            nodes: Vec::new(),
            edges: Vec::new(),
            definitions: Vec::new(),
            imports: Vec::new(),
        }
    }

    #[test]
    fn emit_prelude_returns_substrate_only_error() {
        let cg = Codegen::new();
        let ir = empty_ir();
        let out_dir = PathBuf::from("/tmp/grx-codegen-go-test");
        let result = cg.emit_prelude(&ir, &out_dir);
        let Err(CodegenError::NotYetImplemented(method)) = result else {
            panic!("expected NotYetImplemented");
        };
        assert_eq!(method, "emit_prelude");
    }

    #[test]
    fn build_artifact_returns_substrate_only_error() {
        let cg = Codegen::new();
        let src_dir = PathBuf::from("/tmp/grx-codegen-go-test-src");
        let result = cg.build_artifact(&src_dir);
        let Err(CodegenError::NotYetImplemented(method)) = result else {
            panic!("expected NotYetImplemented");
        };
        assert_eq!(method, "build_artifact");
    }

    #[test]
    fn error_message_cites_plan_and_adr() {
        let err = CodegenError::NotYetImplemented("emit_prelude");
        let msg = format!("{err}");
        assert!(msg.contains("M17"), "error must cite M17: {msg}");
        assert!(msg.contains("ADR") || msg.contains("0005"), "error must cite ADR-0005: {msg}");
    }
}
