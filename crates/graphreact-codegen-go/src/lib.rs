//! Build-step codegen for Go bodies in `.grx` files — **M17 phase 3b v1**.
//!
//! Per [ADR-0005](../../../docs/decisions/0005-third-runtime-is-go-not-jvm.md)
//! locked decision #3 and the [later-milestone strategic plan](../../../../../.claude/plans/graphreact-rust-later-milestones.md)
//! §M17, Go has no runtime `eval` (the
//! [`graphreact_engine_zig::Engine::eval_module`] arm for Go cannot
//! mirror M13's HOOK_PRELUDE-at-eval-time pattern). Component bodies
//! must therefore be **pre-compiled** via a build step that:
//!
//! 1. Reads each `.grx` file's `lang="go"` definitions.
//! 2. Emits a canonical [`prelude::render`]-shaped `grx_prelude.go`
//!    carrying the FFI scaffold + cgo header + `//export Grx*` entry
//!    points the [`graphreact_engine_go::LoadedRuntime`] loader expects.
//! 3. Writes each Go body to a sibling `body_<component>.go` so the
//!    Go compiler builds the combined source set as one package.
//! 4. Drops a `go.mod` so `go build` runs without GOPATH config.
//! 5. Invokes `go build -buildmode=c-shared` and produces the
//!    `.so` / `.dylib` / `.dll` artifact + a SHA-256 of its bytes.
//!
//! Authors write plain Go in their `.grx` bodies; the `grx build`
//! pipeline composes the prelude + body and produces the c-shared
//! artifact. `init()` registration APIs, struct-tag macros, and
//! `go:generate` annotations are considered and explicitly rejected
//! per ADR-0005.
//!
//! # Status
//!
//! - **Phase 3a (parser-side):** SHIPPED — [`graphreact_grx_parser::ast::ComponentLang::Go`]
//!   recognises `lang="go" | "golang"` bodies.
//! - **Phase 3b-real (this crate):** SHIPPED — codegen emits the
//!   prelude + per-component `body_<C>.go` + a `dispatch.go` that
//!   populates a process-global `bodyRegistry` from `init()`.
//!   `GrxFunctionCall` routes by `(component, fn_name)` into the
//!   registry; user bodies authored as `func <C>Body(ctx Ctx) Manifest`
//!   actually execute end-to-end. The hook surface (`UseState` /
//!   `UseEffect` / `UseEmit` / `UseMessage` / etc.) layers on top in
//!   M17 phase 3c–h.
//!
//! # Author convention
//!
//! For each `<C lang="go">…</C>` definition, the user MUST export a
//! Go function named `<C>Body(ctx Ctx) Manifest`. The codegen-emitted
//! `dispatch.go` registers it under the component name via
//! `RegisterBody`. A component without the matching function will
//! surface a Go compile error from `go build`, naming the missing
//! identifier.
//!
//! [`graphreact-engine-go`]: ../../graphreact-engine-go/

#![allow(clippy::module_name_repetitions)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use graphreact_grx_parser::ast::ComponentLang;
use graphreact_grx_parser::ir::GrxIr;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub mod prelude;

/// Errors surfaced by the M17 build-step codegen pipeline.
#[derive(Debug, Error)]
pub enum CodegenError {
    #[error("graphreact-codegen-go: I/O error writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "graphreact-codegen-go: `go` toolchain not found on PATH \
         (or failed `go version` probe). Install Go 1.22+ or set the \
         GO environment variable. See ~/.claude/plans/graphreact-rust-later-milestones.md \
         §M17 for the build prerequisites."
    )]
    ToolchainMissing,
    #[error(
        "graphreact-codegen-go: `go build -buildmode=c-shared` failed with status {status}.\n\
         stdout:\n{stdout}\n\
         stderr:\n{stderr}"
    )]
    GoBuildFailed {
        status: i32,
        stdout: String,
        stderr: String,
    },
    #[error(
        "graphreact-codegen-go: no `lang=\"go\"` definitions found in IR. \
         The codegen pipeline only runs when at least one Go body is present."
    )]
    NoGoBodies,
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
/// minimal at the phase 3b v1 tier; future phases extend
/// [`Codegen`] with per-hook prelude variants.
#[derive(Default)]
pub struct Codegen;

impl Codegen {
    /// Construct a fresh `Codegen`. Cheap; never fails.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Emit the prelude + per-body source files + dispatch.go into
    /// `out_dir`.
    ///
    /// Files written:
    /// - `<out_dir>/grx_prelude.go` — canonical FFI scaffold from
    ///   [`prelude::render`].
    /// - `<out_dir>/body_<safe_name>.go` for each Go body — wraps the
    ///   `<X lang="go">...</X>` source in a `package main`
    ///   declaration. Bodies share the prelude's package namespace.
    /// - `<out_dir>/dispatch.go` — codegen-emitted `init()` that
    ///   registers each component's `<C>Body` function in the
    ///   prelude's `bodyRegistry`. See [`prelude::render_dispatch`].
    /// - `<out_dir>/go.mod` — minimal module file so `go build` runs
    ///   without `GOPATH` configuration.
    ///
    /// # Errors
    ///
    /// - [`CodegenError::NoGoBodies`] if `ir.definitions` contains no
    ///   `ComponentLang::Go` entries — the pipeline is a no-op in
    ///   that case.
    /// - [`CodegenError::Io`] for any file-write failure (typically a
    ///   permissions or missing-directory issue).
    pub fn emit_prelude(&self, ir: &GrxIr, out_dir: &Path) -> Result<(), CodegenError> {
        let go_defs: Vec<_> = ir
            .definitions
            .iter()
            .filter(|d| d.lang == ComponentLang::Go)
            .collect();
        if go_defs.is_empty() {
            return Err(CodegenError::NoGoBodies);
        }

        fs::create_dir_all(out_dir).map_err(|source| CodegenError::Io {
            path: out_dir.to_path_buf(),
            source,
        })?;

        write_file(&out_dir.join("grx_prelude.go"), &prelude::render())?;
        write_file(&out_dir.join("go.mod"), &prelude::render_go_mod())?;

        for def in &go_defs {
            let safe = sanitize_filename(&def.component);
            let body_path = out_dir.join(format!("body_{safe}.go"));
            write_file(&body_path, &wrap_body(&def.component, &def.code))?;
        }

        let component_names: Vec<String> =
            go_defs.iter().map(|d| d.component.clone()).collect();
        write_file(
            &out_dir.join("dispatch.go"),
            &prelude::render_dispatch(&component_names),
        )?;

        Ok(())
    }

    /// Invoke `go build -buildmode=c-shared` against the source set
    /// at `src_dir` and return the produced [`BuildArtifact`].
    ///
    /// `out_dir` is the directory the library + header land in. The
    /// library filename is derived from
    /// [`std::env::consts::DLL_PREFIX`] + `"grx_go"` +
    /// [`std::env::consts::DLL_SUFFIX`].
    ///
    /// # Errors
    ///
    /// - [`CodegenError::ToolchainMissing`] if `go` is absent or
    ///   `go version` fails to execute.
    /// - [`CodegenError::GoBuildFailed`] if `go build` exits non-zero.
    /// - [`CodegenError::Io`] for any file-read failure when hashing
    ///   the produced artifact.
    pub fn build_artifact(
        &self,
        src_dir: &Path,
        out_dir: &Path,
    ) -> Result<BuildArtifact, CodegenError> {
        let go_bin = std::env::var("GO").unwrap_or_else(|_| "go".to_string());
        let probe = Command::new(&go_bin).arg("version").output();
        if !matches!(&probe, Ok(out) if out.status.success()) {
            return Err(CodegenError::ToolchainMissing);
        }

        fs::create_dir_all(out_dir).map_err(|source| CodegenError::Io {
            path: out_dir.to_path_buf(),
            source,
        })?;

        let lib_name = format!(
            "{prefix}grx_go{suffix}",
            prefix = std::env::consts::DLL_PREFIX,
            suffix = std::env::consts::DLL_SUFFIX,
        );
        let library_path = out_dir.join(&lib_name);

        let output = Command::new(&go_bin)
            .current_dir(src_dir)
            .arg("build")
            .arg("-buildmode=c-shared")
            .arg("-o")
            .arg(&library_path)
            .arg(".")
            .output()
            .map_err(|source| CodegenError::Io {
                path: src_dir.to_path_buf(),
                source,
            })?;

        if !output.status.success() {
            return Err(CodegenError::GoBuildFailed {
                status: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        // Best-effort macOS codesign — the dlopen-from-arbitrary-path
        // path is rejected on Apple Silicon without an ad-hoc
        // signature.
        if cfg!(target_os = "macos") {
            let _ = Command::new("codesign")
                .arg("--sign")
                .arg("-")
                .arg(&library_path)
                .status();
        }

        let bytes = fs::read(&library_path).map_err(|source| CodegenError::Io {
            path: library_path.clone(),
            source,
        })?;
        let library_sha256_hex = format!("{:x}", Sha256::digest(&bytes));

        // The Go toolchain emits a sibling `.h` file by default.
        let header_path_candidate = derive_header_path(&library_path);
        let header_path = if header_path_candidate.exists() {
            Some(header_path_candidate)
        } else {
            None
        };

        Ok(BuildArtifact {
            library_path,
            header_path,
            library_sha256_hex,
        })
    }
}

fn write_file(path: &Path, contents: &str) -> Result<(), CodegenError> {
    let mut f = fs::File::create(path).map_err(|source| CodegenError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    f.write_all(contents.as_bytes())
        .map_err(|source| CodegenError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(())
}

/// Replace any non-`[A-Za-z0-9_]` character with `_` so the produced
/// filename is safe to drop on disk + safe to import in Go.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// Wrap a Go body source in a `package main` declaration if it
/// doesn't already have one.
fn wrap_body(component: &str, source: &str) -> String {
    let trimmed = source.trim_start();
    if trimmed.starts_with("package ") {
        // Author already declared a package; emit as-is. The build
        // step will fail if the package doesn't match `main`.
        return source.to_string();
    }
    format!(
        "// Code generated by graphreact-codegen-go. DO NOT EDIT.\n\
         // Source: {component} body extracted from .grx definitions.\n\
         \n\
         package main\n\
         \n\
         {source}\n",
    )
}

/// Derive the sibling `.h` header path the Go toolchain emits next to
/// a `-buildmode=c-shared` library. The convention is to swap the DLL
/// suffix for `.h`.
fn derive_header_path(library_path: &Path) -> PathBuf {
    let mut p = library_path.to_path_buf();
    p.set_extension("h");
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_grx_parser::ast::ComponentLang;
    use graphreact_grx_parser::ir::{GrxIr, IrDefinition};
    use graphreact_grx_parser::SourceLoc;

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

    fn ir_with_go_body(component: &str, code: &str) -> GrxIr {
        let mut ir = empty_ir();
        ir.definitions.push(IrDefinition {
            component: component.to_string(),
            code: code.to_string(),
            lang: ComponentLang::Go,
            source_loc: SourceLoc::new(1, 1),
        });
        ir
    }

    #[test]
    fn no_go_bodies_returns_clear_error() {
        let cg = Codegen::new();
        let ir = empty_ir();
        let out = tempdir();
        let result = cg.emit_prelude(&ir, out.path());
        assert!(matches!(result, Err(CodegenError::NoGoBodies)));
    }

    #[test]
    fn emit_writes_prelude_and_body_and_dispatch_and_go_mod() {
        let cg = Codegen::new();
        let body = "// Counter body\nfunc CounterBody(ctx Ctx) Manifest { return Manifest{} }\n";
        let ir = ir_with_go_body("Counter", body);
        let out = tempdir();
        cg.emit_prelude(&ir, out.path()).expect("emit_prelude");

        let prelude_path = out.path().join("grx_prelude.go");
        let body_path = out.path().join("body_Counter.go");
        let dispatch_path = out.path().join("dispatch.go");
        let mod_path = out.path().join("go.mod");

        assert!(prelude_path.exists(), "prelude missing");
        assert!(body_path.exists(), "body missing");
        assert!(dispatch_path.exists(), "dispatch.go missing");
        assert!(mod_path.exists(), "go.mod missing");

        let prelude_src = fs::read_to_string(&prelude_path).unwrap();
        // Every Grx* symbol the engine-go LoadedRuntime resolves
        // must be present in the emitted prelude. Drives a hard
        // contract between codegen and loader so a regression that
        // drops one symbol is caught here, not at dlopen time.
        for sym in [
            "GrxAbiVersion",
            "GrxEngineInitialize",
            "GrxEngineShutdown",
            "GrxIsolateCreate",
            "GrxIsolateDispose",
            "GrxModuleRegister",
            "GrxFunctionResolve",
            "GrxFunctionCall",
            "GrxCallbackRegister",
            "GrxMicrotasksDrain",
        ] {
            assert!(
                prelude_src.contains(sym),
                "prelude missing required symbol: {sym}"
            );
        }
        // The substrate types + body-dispatch surface must be present:
        // a regression that strips RegisterBody or bodyRegistry would
        // silently break user-authored body dispatch.
        for sym in ["type Ctx", "type Manifest", "RegisterBody", "bodyRegistry"] {
            assert!(
                prelude_src.contains(sym),
                "prelude missing required body-dispatch surface: {sym}"
            );
        }
        // M17 phase 3c surface: UseEmit / UseMessage hooks +
        // GrxInvokeHandler ABI symbol + host_emit / host_message
        // slot intercept. A regression that strips any of these
        // breaks the emit/message round-trip.
        for sym in [
            "func UseEmit()",
            "func UseMessage(",
            "GrxInvokeHandler",
            "hostEmitSlot",
            "hostMessageSlot",
            "handlerRegistry",
            "registerMessageHandler",
            "invokeHostCallback",
        ] {
            assert!(
                prelude_src.contains(sym),
                "prelude missing required M17 phase 3c hook surface: {sym}"
            );
        }
        // M17 phase 3b-complete surface: UseState / UseEffect +
        // callbackRegistry + GrxInvokeCallback / GrxReleaseCallback
        // ABI symbols. A regression that strips any of these breaks
        // the state survives-a-message acceptance gate.
        for sym in [
            "func UseState(",
            "func UseEffect(",
            "type StateResult",
            "callbackRegistry",
            "GrxInvokeCallback",
            "GrxReleaseCallback",
        ] {
            assert!(
                prelude_src.contains(sym),
                "prelude missing required M17 phase 3b-complete hook surface: {sym}"
            );
        }
        // M17 phase 3d-α surface: split-cache memo helpers +
        // identity-stable callback + pure-userland reducer.
        for sym in [
            "func UseMemo(",
            "func UseCallback(",
            "func UseReducer(",
            "type MemoCheckResult",
        ] {
            assert!(
                prelude_src.contains(sym),
                "prelude missing required M17 phase 3d-α hook surface: {sym}"
            );
        }
        // M17 phase 3d-β surface: neighbors + topology +
        // accept_edge hooks. Stripping any of these breaks the
        // structural-graph navigation acceptance.
        for sym in [
            "func UseNeighbors(",
            "func UseTopology(",
            "func UseAcceptEdge()",
            "type TopologyEvent",
            "type TopologyFilter",
        ] {
            assert!(
                prelude_src.contains(sym),
                "prelude missing required M17 phase 3d-β hook surface: {sym}"
            );
        }
        // M17 phase 3d-γ surface: directory finders + graph_query
        // Tier 1. A regression that strips any of these breaks the
        // ctx.directory + UseGraphQuery acceptance.
        for sym in [
            "type DirectoryEntry",
            "type Directory ",
            "func UseDirectory()",
            "FindByID",
            "FindByRole",
            "FindByDepth",
            "FindInSubtree",
            "IsAncestor",
            "type GraphQuery",
            "func UseGraphQuery()",
            "Neighbors",
            "HasNode",
            "HasEdge",
            "NodeCount",
        ] {
            assert!(
                prelude_src.contains(sym),
                "prelude missing required M17 phase 3d-γ hook surface: {sym}"
            );
        }
        assert!(prelude_src.contains("package main"));

        let body_src = fs::read_to_string(&body_path).unwrap();
        assert!(body_src.contains("CounterBody"));
        assert!(body_src.contains("package main"));

        let dispatch_src = fs::read_to_string(&dispatch_path).unwrap();
        assert!(dispatch_src.contains("RegisterBody(\"Counter\", CounterBody)"));
        assert!(dispatch_src.contains("func init()"));

        let mod_src = fs::read_to_string(&mod_path).unwrap();
        assert!(mod_src.contains("module grxbody"));
    }

    #[test]
    fn build_artifact_toolchain_missing_returns_clear_error() {
        // Force the toolchain probe to fail by pointing `GO` at a
        // path that doesn't exist. Save+restore the env var so
        // parallel tests aren't disturbed (Rust runs tests in
        // parallel by default; `serial_test` isn't a dep here, so
        // we restore in a `Drop` guard).
        struct GoEnvGuard {
            original: Option<String>,
        }
        impl Drop for GoEnvGuard {
            fn drop(&mut self) {
                match &self.original {
                    Some(v) => std::env::set_var("GO", v),
                    None => std::env::remove_var("GO"),
                }
            }
        }
        let _guard = GoEnvGuard {
            original: std::env::var("GO").ok(),
        };
        std::env::set_var("GO", "/nonexistent/grx-codegen-go-not-on-disk");

        let cg = Codegen::new();
        let src = tempdir();
        let out = tempdir();
        // Create a minimal src dir so build_artifact actually reaches
        // the probe before the no-Go-files check would catch it.
        std::fs::write(src.path().join("dummy.go"), "package main\nfunc main(){}\n").unwrap();

        let result = cg.build_artifact(src.path(), out.path());
        assert!(matches!(result, Err(CodegenError::ToolchainMissing)));
    }

    #[test]
    fn build_artifact_bad_go_source_returns_failed_with_stderr() {
        // Probe for go; skip if absent.
        let probe = Command::new("go").arg("version").output();
        if !matches!(&probe, Ok(out) if out.status.success()) {
            eprintln!("skipping bad-source test: `go` not on PATH");
            return;
        }

        let cg = Codegen::new();
        // Deliberately broken Go body: missing func keyword.
        let body = "this is not valid go syntax\n";
        let ir = ir_with_go_body("Broken", body);
        let src = tempdir();
        let out = tempdir();
        cg.emit_prelude(&ir, src.path()).expect("emit_prelude");

        let result = cg.build_artifact(src.path(), out.path());
        let Err(CodegenError::GoBuildFailed { stderr, status, .. }) = result else {
            panic!("expected GoBuildFailed, got {result:?}");
        };
        assert_ne!(status, 0, "expected non-zero go build exit code");
        assert!(
            !stderr.is_empty(),
            "expected non-empty stderr from go build, got empty"
        );
    }

    #[test]
    fn sanitize_filename_strips_unsafe_chars() {
        assert_eq!(sanitize_filename("Counter"), "Counter");
        assert_eq!(sanitize_filename("foo-bar.baz"), "foo_bar_baz");
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
    }

    #[test]
    fn wrap_body_skips_double_wrap_when_package_already_present() {
        let already = "package main\n\nfunc Body() {}\n";
        assert_eq!(wrap_body("Foo", already), already);

        let raw = "func Body() {}\n";
        let wrapped = wrap_body("Foo", raw);
        assert!(wrapped.contains("package main"));
        assert!(wrapped.contains("func Body()"));
    }

    /// Smoke test for the build_artifact path. Skipped silently when
    /// the `go` toolchain isn't on PATH (CI environments without Go).
    #[test]
    fn build_artifact_produces_loadable_library() {
        // Probe for go; skip the test on absence.
        let probe = Command::new("go").arg("version").output();
        if !matches!(&probe, Ok(out) if out.status.success()) {
            eprintln!("skipping build_artifact test: `go` not on PATH");
            return;
        }

        let cg = Codegen::new();
        // Phase 3b-real convention: the codegen-emitted dispatch.go
        // calls `RegisterBody("Smoke", SmokeBody)`, so the body MUST
        // define `SmokeBody(ctx Ctx) Manifest` or `go build` rejects
        // the undefined identifier. Keeps the build-smoke aligned
        // with the production author convention.
        let body = "func SmokeBody(ctx Ctx) Manifest { return Manifest{ID: ctx.ID} }\n";
        let ir = ir_with_go_body("Smoke", body);

        let src = tempdir();
        let out = tempdir();
        cg.emit_prelude(&ir, src.path()).expect("emit_prelude");

        let artifact = cg
            .build_artifact(src.path(), out.path())
            .expect("build_artifact");
        assert!(artifact.library_path.exists());
        assert_eq!(artifact.library_sha256_hex.len(), 64);
        // Header path is optional but should exist for the canonical
        // c-shared build.
        if let Some(h) = &artifact.header_path {
            assert!(h.exists());
        }

    }

    // ---- minimal tempdir helper to avoid pulling in the tempfile dep ----

    struct TempDir(PathBuf);

    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("grx-codegen-go-test-{}-{}", std::process::id(), n));
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
