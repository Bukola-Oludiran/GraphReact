//! Go runtime engine for GraphReact — **M17 PLANNED, phase 1+2 substrate scaffold**.
//!
//! Per [ADR-0005](../../../docs/decisions/0005-third-runtime-is-go-not-jvm.md)
//! and the [later-milestone strategic plan](../../../../../.claude/plans/graphreact-rust-later-milestones.md)
//! §M17, the Go runtime ships as the third in-process body-language host
//! after JSC (M12.3) and CPython (M13). The embedding strategy is
//! **cgo + c-shared**: each `.grx` file's `lang="go"` bodies are compiled
//! by [`graphreact-codegen-go`](../../graphreact-codegen-go/) via
//! `go build -buildmode=c-shared` into a `.so` / `.dylib` artifact that
//! the daemon loads through `libloading` and dispatches against the
//! [`graphreact_engine_zig::Engine`] trait.
//!
//! # Phase status (M17)
//!
//! - **Phase 1+2 (Engine binding lifecycle):** scaffold only — every
//!   `Engine` method returns [`EngineError::Internal`] flagging the
//!   substrate-only status. Real `initialize` / `shutdown` /
//!   `create_isolate` / `dispose_isolate` / `eval_module` /
//!   `resolve_function` / `call_function` / `register_callback` /
//!   `drain_microtasks` wiring lands in M17 phase 1+2.
//! - **Phase 2.5 (bidirectional callbacks):** not yet — cgo trampoline
//!   for `Rust → Go → Rust` callbacks lands in M17 phase 2.5.
//! - **Phase 3a–3h (hook surface + parity gates):** not yet — see plan
//!   file §M17.
//!
//! # Why the scaffold ships now (substrate uplift)
//!
//! M17 phase 1+2 needs the workspace to know about the Go engine —
//! [`graphreact_grx_parser::ast::ComponentLang::Go`],
//! `crates/graphreact-cli/src/engine_select.rs::EngineKind::Go`,
//! `crates/graphreact-cli/src/cli.rs::RuntimeArg::Go`, and
//! `crates/graphreact-cli/src/debug/flag.rs::DebugEngine::Go` all
//! reference this crate by name. The scaffold provides a real
//! `GoEngine` type the workspace can construct, while every method
//! returns a clear "not yet implemented" error pointing back at the
//! plan.
//!
//! [`graphreact-codegen-go`]: ../../graphreact-codegen-go/

#![allow(clippy::module_name_repetitions)]

use graphreact_engine_zig::{
    BoxCallback, CallbackId, Engine, EngineError, EngineResult, FunctionHandle, HostValue,
    IsolateHandle, IsolateOptions, ModuleHandle,
};

/// Go runtime engine — substrate-only scaffold pending M17 phase 1+2.
///
/// Constructing a `GoEngine` is cheap and never fails. Every `Engine`
/// trait method returns an [`EngineError::Internal`] flagging the
/// substrate-only status so callers see a clear, actionable message
/// rather than a panic or a silent no-op.
///
/// Selecting this engine through the CLI surface is gated earlier by
/// [`graphreact-cli`'s `EngineSelectError::GoNotLinked`], so most
/// operators will encounter that error before they ever construct a
/// `GoEngine` directly. The scaffold here exists for workspace-level
/// composition (engine list, type-system substrate, future-test
/// fixtures).
#[derive(Default)]
pub struct GoEngine {
    initialized: bool,
}

impl GoEngine {
    /// Construct a fresh `GoEngine`. Cheap; never fails.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Helper to centralize the substrate-only error message so every
/// stub returns the same text and the same plan-file pointer.
fn not_yet_implemented(method: &'static str) -> EngineError {
    EngineError::Internal(format!(
        "graphreact-engine-go::{method} not yet implemented \
         (M17 phase 1+2 substrate-only scaffold). \
         See ~/.claude/plans/graphreact-rust-later-milestones.md §M17 \
         and docs/decisions/0005-third-runtime-is-go-not-jvm.md."
    ))
}

impl Engine for GoEngine {
    fn initialize(&mut self) -> EngineResult<()> {
        // M17 phase 1 will: load `libgrx_go.so` / `libgrx_go.dylib`
        // via libloading; call its `GrxInitialize` exported symbol;
        // bind the host-callback trampoline.
        self.initialized = true;
        Ok(())
    }

    fn shutdown(&mut self) -> EngineResult<()> {
        // M17 phase 1 will: call `GrxShutdown` on the loaded artifact
        // and release the libloading handle.
        self.initialized = false;
        Ok(())
    }

    fn create_isolate(&mut self, _opts: IsolateOptions) -> EngineResult<IsolateHandle> {
        // M17 phase 1 will: allocate a per-component goroutine pool
        // entry wrapped in `sync.Mutex` serialization, returning a
        // handle that maps to a Go-side isolate registry.
        Err(not_yet_implemented("create_isolate"))
    }

    fn dispose_isolate(&mut self, _iso: IsolateHandle) -> EngineResult<()> {
        Err(not_yet_implemented("dispose_isolate"))
    }

    fn eval_module(
        &mut self,
        _iso: IsolateHandle,
        _module_name: &str,
        _source: &str,
    ) -> EngineResult<ModuleHandle> {
        // M17 phase 1+2: Go has no runtime eval — `eval_module`
        // arrives with `source` already representing a compiled
        // c-shared symbol address, not source text. The codegen +
        // build step happens out-of-band in `graphreact-codegen-go`.
        Err(not_yet_implemented("eval_module"))
    }

    fn resolve_function(
        &mut self,
        _iso: IsolateHandle,
        _module: ModuleHandle,
        _name: &str,
    ) -> EngineResult<FunctionHandle> {
        // M17 phase 1+2: resolve a Go-side function pointer by
        // exported name (e.g. `Counter_Body`).
        Err(not_yet_implemented("resolve_function"))
    }

    fn register_callback(
        &mut self,
        _iso: IsolateHandle,
        _name: &str,
        _callback: BoxCallback,
    ) -> EngineResult<CallbackId> {
        // M17 phase 2.5: bidirectional callback registration via the
        // cgo trampoline.
        Err(not_yet_implemented("register_callback"))
    }

    fn call_function(
        &mut self,
        _iso: IsolateHandle,
        _function: FunctionHandle,
        _args: &[HostValue],
    ) -> EngineResult<HostValue> {
        // M17 phase 1+2: marshal `args` into the cgo ABI, lock the
        // per-isolate `sync.Mutex`, invoke the Go function, unmarshal
        // the return.
        Err(not_yet_implemented("call_function"))
    }

    fn drain_microtasks(&mut self, _iso: IsolateHandle) -> EngineResult<()> {
        // M17 phase 3g: drive the Go runtime's `context.Context`
        // cancellation for body-spawned goroutines bound to the
        // component lifetime.
        Err(not_yet_implemented("drain_microtasks"))
    }

    fn engine_name(&self) -> &'static str {
        "go"
    }

    fn engine_version(&self) -> String {
        format!("go-{}-scaffold", env!("CARGO_PKG_VERSION"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_name_is_go() {
        let e = GoEngine::new();
        assert_eq!(e.engine_name(), "go");
    }

    #[test]
    fn engine_version_flags_scaffold() {
        let e = GoEngine::new();
        let v = e.engine_version();
        assert!(v.starts_with("go-"), "got {v}");
        assert!(v.ends_with("-scaffold"), "got {v}");
    }

    #[test]
    fn initialize_and_shutdown_are_idempotent() {
        let mut e = GoEngine::new();
        assert!(e.initialize().is_ok());
        assert!(e.initialize().is_ok());
        assert!(e.shutdown().is_ok());
        assert!(e.shutdown().is_ok());
    }

    #[test]
    fn create_isolate_returns_substrate_only_error() {
        let mut e = GoEngine::new();
        e.initialize().unwrap();
        let result = e.create_isolate(IsolateOptions::default());
        let Err(EngineError::Internal(msg)) = result else {
            panic!("expected EngineError::Internal");
        };
        assert!(msg.contains("M17"), "error should cite M17: {msg}");
        assert!(
            msg.contains("substrate-only"),
            "error should flag substrate-only: {msg}"
        );
    }

    #[test]
    fn eval_module_returns_substrate_only_error() {
        let mut e = GoEngine::new();
        e.initialize().unwrap();
        let result = e.eval_module(IsolateHandle(0), "counter", "");
        assert!(matches!(result, Err(EngineError::Internal(_))));
    }

    #[test]
    fn call_function_returns_substrate_only_error() {
        let mut e = GoEngine::new();
        e.initialize().unwrap();
        let result = e.call_function(IsolateHandle(0), FunctionHandle(0), &[]);
        assert!(matches!(result, Err(EngineError::Internal(_))));
    }
}
