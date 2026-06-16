//! Composition layer for **M17 Go bodies** — **phase 3a skeleton**.
//!
//! Mirrors the role of [`crate::pyruntime`] (M13 CPython) and
//! [`crate::jsruntime`] (M12.3 JSC) for `lang="go"` definitions:
//! parses a `.grx` file, drives [`graphreact_codegen_go`] to emit the
//! prelude + body files + invoke `go build -buildmode=c-shared`, then
//! constructs a [`graphreact_engine_go::GoEngine`] pointed at the
//! produced artifact.
//!
//! # Phase status
//!
//! - **Phase 3a v1 (this module):** [`build_and_load`] runs the
//!   end-to-end codegen + build + load + initialize round-trip on a
//!   `.grx` file with `lang="go"` definitions, returns a
//!   [`GoRuntime`] holding the live engine + one isolate. **Does
//!   NOT yet wire into the scheduler** — the full scheduler-mount
//!   surface (`mount_with_scheduler`-equivalent) lands in M17 phase
//!   3a follow-on alongside the hook surface (3c–h).
//! - **Phase 3a follow-on:** scheduler integration. Mirror
//!   [`crate::pyruntime::mount_with_scheduler`] verbatim with Go body
//!   dispatch.
//! - **Phase 3c–h:** the full M6.6 hook surface (`UseState`,
//!   `UseEffect`, `UseEmit`, `UseMessage`, `UseNeighbors`,
//!   `UseGraphQuery` Tier 1/2/3, etc.) replaces the
//!   [`graphreact_codegen_go::prelude::render`] stubbed bodies.
//!
//! Per [ADR-0005](../../../docs/decisions/0005-third-runtime-is-go-not-jvm.md)
//! and the [later-milestone strategic plan](../../../../../.claude/plans/graphreact-rust-later-milestones.md)
//! §M17.

use std::path::{Path, PathBuf};

use graphreact_engine_go::GoEngine;
use graphreact_engine_zig::{Engine, IsolateHandle, IsolateOptions};

use crate::runtime::RuntimeError;

/// Live Go-runtime composition produced by [`build_and_load`].
/// Holds the [`GoEngine`] post-`initialize`, one isolate, and the
/// codegen-produced [`graphreact_codegen_go::BuildArtifact`] for
/// debug + hot-reload short-circuiting.
///
/// **Phase 3a v1:** intentionally minimal. The scheduler /
/// component-handle / hook-state surfaces that
/// [`crate::pyruntime::PyRuntime`] carries land at phase 3a
/// follow-on; this skeleton proves only the codegen → build → load
/// → initialize → create_isolate path.
pub struct GoRuntime {
    pub engine: GoEngine,
    pub iso: IsolateHandle,
    /// Path to the c-shared artifact produced by
    /// [`graphreact_codegen_go::Codegen::build_artifact`]. Held for
    /// debug + M17 phase 3g hot-reload short-circuiting.
    pub artifact_path: PathBuf,
    /// SHA-256 of the artifact bytes (lowercase hex). Phase 3g uses
    /// this to skip rebuilds when the input IR + toolchain produce
    /// byte-identical output.
    pub artifact_sha256_hex: String,
    /// Number of `lang="go"` definitions the codegen-emitted package
    /// contained. Used in `grx ps` / observer surfaces to flag the
    /// component count to operators.
    pub n_definitions: usize,
    /// Graph id from the parsed IR — useful for logging and the
    /// observer surfaces.
    pub graph_id: String,
}

impl GoRuntime {
    /// Build the artifact at `out_dir` and dispose the engine on drop.
    /// `out_dir` is also where intermediate `grx_prelude.go` /
    /// `body_*.go` / `go.mod` land — same directory as the produced
    /// `libgrx_go.{so,dylib,dll}`.
    ///
    /// **NOT a full mount.** The scheduler-side mount lands at M17
    /// phase 3a follow-on. This function exists so phase 3a v1
    /// orchestration is end-to-end testable today.
    pub fn build_and_load(grx_path: &Path, out_dir: &Path) -> Result<Self, RuntimeError> {
        let ir = parse_grx_to_ir(grx_path)?;

        let codegen = graphreact_codegen_go::Codegen::new();
        codegen.emit_prelude(&ir, out_dir).map_err(|e| {
            RuntimeError::Engine(graphreact_engine_zig::EngineError::Internal(format!(
                "graphreact-codegen-go::emit_prelude: {e}"
            )))
        })?;
        let artifact = codegen.build_artifact(out_dir, out_dir).map_err(|e| {
            RuntimeError::Engine(graphreact_engine_zig::EngineError::Internal(format!(
                "graphreact-codegen-go::build_artifact: {e}"
            )))
        })?;

        let mut engine = GoEngine::with_artifact(artifact.library_path.clone());
        engine
            .initialize()
            .map_err(RuntimeError::Engine)?;
        let iso = engine
            .create_isolate(IsolateOptions::default())
            .map_err(RuntimeError::Engine)?;

        let n_definitions = ir
            .definitions
            .iter()
            .filter(|d| d.lang == graphreact_grx_parser::ast::ComponentLang::Go)
            .count();

        Ok(GoRuntime {
            engine,
            iso,
            artifact_path: artifact.library_path,
            artifact_sha256_hex: artifact.library_sha256_hex,
            n_definitions,
            graph_id: ir.graph_id,
        })
    }
}

fn parse_grx_to_ir(path: &Path) -> Result<graphreact_grx_parser::ir::GrxIr, RuntimeError> {
    // Mirror `pyruntime::mount_python_definitions` shape: drive the
    // multi-file resolver so `<X src="./body.go" lang="go"/>` external
    // bodies (M11.5 / Phase 5-D substrate) actually populate the IR's
    // `code` field. The in-text-only `parse_to_ir` returns empty code
    // for external-body definitions because it never touches the
    // sibling .go file — silently breaking the `go-external-body.grx`
    // fixture path through codegen-go.
    let resolved = graphreact_grx_parser::multi_file::resolve_multi_file(path).map_err(|e| {
        RuntimeError::Engine(graphreact_engine_zig::EngineError::Internal(format!(
            "graphreact-cli::goruntime: multi-file resolver error in {}: {e}",
            path.display()
        )))
    })?;
    let subgraphs = resolved.collect_root_subgraphs().map_err(|e| {
        RuntimeError::Engine(graphreact_engine_zig::EngineError::Internal(format!(
            "graphreact-cli::goruntime: subgraph rollup error in {}: {e}",
            path.display()
        )))
    })?;
    graphreact_grx_parser::ir::build_ir(&graphreact_grx_parser::GrxFile {
        imports: resolved.root.imports.clone(),
        graph: resolved.root.graph.clone(),
        subgraphs,
        definitions: resolved.root.definitions.clone(),
    })
    .map_err(|e| {
        RuntimeError::Engine(graphreact_engine_zig::EngineError::Internal(format!(
            "graphreact-cli::goruntime: build_ir error in {}: {e}",
            path.display()
        )))
    })
}

#[cfg(test)]
mod tests {
    //! Phase 3a v1 unit tests — exercise the parse + dispatch path
    //! without spinning up a real Go runtime (the conformance smoke
    //! at `graphreact-engine-go/tests/conformance_smoke.rs` carries
    //! end-to-end Go-runtime coverage).

    use super::*;

    #[test]
    fn build_and_load_rejects_missing_file() {
        let nowhere = std::path::Path::new("/tmp/grx-this-file-definitely-does-not-exist.grx");
        let out_dir = std::env::temp_dir().join("grx-goruntime-test");
        let result = GoRuntime::build_and_load(nowhere, &out_dir);
        assert!(result.is_err());
    }

    /// End-to-end happy path: parse `tests/fixtures/go-smoke.grx`,
    /// run codegen-go, run `go build -buildmode=c-shared`, load via
    /// libloading, initialize, create an isolate. Skipped on systems
    /// without the `go` toolchain on PATH.
    ///
    /// Wrapped in `std::process::exit(0)` on success because the Go
    /// runtime's TSD cleanup deadlocks on `pthread_exit` on macOS —
    /// same hazard the engine-go conformance_smoke integration test
    /// works around.
    #[test]
    fn build_and_load_happy_path_smoke() {
        // Probe for go; skip if absent.
        let probe = std::process::Command::new("go").arg("version").output();
        if !matches!(&probe, Ok(out) if out.status.success()) {
            eprintln!("skipping build_and_load_happy_path_smoke: `go` not on PATH");
            return;
        }

        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go-smoke.grx");
        if !fixture.exists() {
            eprintln!("skipping: fixture not at {}", fixture.display());
            return;
        }
        let out_dir = std::env::temp_dir().join(format!(
            "grx-goruntime-smoke-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&out_dir);

        let runtime = GoRuntime::build_and_load(&fixture, &out_dir)
            .expect("build_and_load should succeed for go-smoke.grx");

        assert!(!runtime.graph_id.is_empty(), "graph_id empty");
        assert!(runtime.n_definitions >= 1, "n_definitions < 1");
        assert_eq!(
            runtime.artifact_sha256_hex.len(),
            64,
            "sha256 should be 64 hex chars"
        );
        assert!(
            runtime.artifact_path.exists(),
            "artifact missing at {}",
            runtime.artifact_path.display()
        );

        // Exit fast — see module-level docs on the Go TSD-cleanup
        // hazard. We leak the GoRuntime on purpose: the process is
        // terminating immediately.
        std::process::exit(0);
    }
}
