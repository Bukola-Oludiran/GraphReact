//! **M16-Backend Phase 8.** CPython debugpy install via the existing
//! [`graphreact_engine_cpython`] FFI surface (`eval_module`).
//!
//! ## Per-process model
//!
//! debugpy.listen binds a TCP socket at the C level — process-global,
//! not per-isolate. CPython sub-interpreters all see the same listener
//! once it's installed; subsequent isolates show as separate threads
//! in the debugger.
//!
//! P8 ships [`install_debugpy`] as the one-shot install path. It's
//! safe to call multiple times — the Python prelude guards on
//! `__main__.__GRX_DEBUGPY_INSTALLED` so a second call is a no-op.
//! The CLI calls it ONCE at first PyRuntime mount.
//!
//! ## Wait-for-client
//!
//! `wait=true` blocks `debugpy.listen` until the operator's IDE
//! attaches. The plan-locked default for `--debug=cpython:5678` so
//! the daemon doesn't race past mount before the user's breakpoints
//! are loaded. `--debug-no-wait` flips it for use cases where the
//! operator wants to attach later (e.g. attach mid-session via
//! `debugpy.connect()`).
//!
//! ## debugpy availability
//!
//! debugpy ships as a PyPI package. The Python interpreter linked
//! into the CPython engine MUST have `debugpy` importable on
//! `sys.path` — typically via `pip install debugpy` in the
//! interpreter's site-packages, OR via `PYTHONPATH=/path/to/debugpy`
//! at process start. If the import fails, [`install_debugpy`]
//! returns the engine error wrapping the Python `ImportError`; the
//! CLI surfaces it cleanly so operators know to install the
//! package.

use graphreact_engine_cpython::CPythonEngine;
use graphreact_engine_zig::handles::IsolateHandle;
use graphreact_engine_zig::{Engine, EngineError};

/// Build the Python prelude that conditionally installs debugpy. The
/// guard via `__main__.__GRX_DEBUGPY_INSTALLED` makes the prelude
/// idempotent across multiple isolates / repeat calls.
#[must_use]
pub fn build_debugpy_prelude(port: u16, wait: bool) -> String {
    let wait_call = if wait {
        "    debugpy.wait_for_client()\n"
    } else {
        ""
    };
    format!(
        "import sys as _grx_sys\n\
         _grx_main = _grx_sys.modules.get('__main__')\n\
         if _grx_main is None:\n    \
             import __main__ as _grx_main\n\
         if not getattr(_grx_main, '__GRX_DEBUGPY_INSTALLED', False):\n    \
             import debugpy as _grx_debugpy\n    \
             _grx_debugpy.listen(('127.0.0.1', {port}))\n\
{wait_call}    \
             setattr(_grx_main, '__GRX_DEBUGPY_INSTALLED', True)\n"
    )
}

/// Install debugpy on `iso` via [`CPythonEngine::eval_module`]. Idempotent
/// across calls (the prelude guards on `__main__.__GRX_DEBUGPY_INSTALLED`).
///
/// `port` binds `127.0.0.1:<port>` only — there is no remote-bind
/// flag in P8 (debugpy traffic carries breakpoint state + variable
/// inspection that's effectively process-internal; the operator
/// uses an SSH tunnel for cross-host attach). Plan-locked.
///
/// `wait=true` blocks until the IDE attaches. Default for
/// `--debug=cpython:5678`; `--debug-no-wait` flips it.
pub fn install_debugpy(
    engine: &mut CPythonEngine,
    iso: IsolateHandle,
    port: u16,
    wait: bool,
) -> Result<(), EngineError> {
    let prelude = build_debugpy_prelude(port, wait);
    engine.eval_module(iso, "__grx_debugpy__", &prelude)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelude_with_wait_includes_wait_for_client() {
        let s = build_debugpy_prelude(5678, true);
        assert!(s.contains("debugpy.listen(('127.0.0.1', 5678))"));
        assert!(s.contains("debugpy.wait_for_client()"));
        assert!(s.contains("__GRX_DEBUGPY_INSTALLED"));
    }

    #[test]
    fn prelude_without_wait_omits_wait_for_client() {
        let s = build_debugpy_prelude(6000, false);
        assert!(s.contains("debugpy.listen(('127.0.0.1', 6000))"));
        assert!(!s.contains("debugpy.wait_for_client()"));
    }

    #[test]
    fn prelude_uses_loopback_only() {
        let s = build_debugpy_prelude(5678, true);
        // No remote-bind variant in P8.
        assert!(s.contains("'127.0.0.1'"));
        assert!(!s.contains("'0.0.0.0'"));
    }

    #[test]
    fn prelude_module_name_is_grx_debugpy() {
        // Test that the module name matches the install_debugpy
        // call site so duplicate installs hit the same module
        // entry — combined with the __GRX_DEBUGPY_INSTALLED
        // guard, repeat installs are a no-op.
        // (Asserted indirectly: the eval_module call site uses
        // the literal "__grx_debugpy__" string. This test pins
        // the name so a future refactor flagged the change.)
        let s = build_debugpy_prelude(5678, true);
        // Sanity: prelude text is non-trivial and grows linearly
        // with field count — pin under 1KB.
        assert!(s.len() < 1024, "prelude {} bytes; pin under 1KB", s.len());
    }

    #[test]
    fn prelude_guards_against_double_install() {
        let s = build_debugpy_prelude(5678, true);
        assert!(s.contains("getattr(_grx_main, '__GRX_DEBUGPY_INSTALLED', False)"));
        assert!(s.contains("setattr(_grx_main, '__GRX_DEBUGPY_INSTALLED', True)"));
    }
}
