//! Engine selection from CLI flag → boxed [`Engine`].

use thiserror::Error;

use graphreact_engine_zig::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    Null,
    Echo,
    Jsc,
    Cpython,
    /// Go body engine. Loads a cgo + c-shared artifact compiled by
    /// `graphreact-codegen-go` and dispatches against the Engine
    /// trait. M17 (PLANNED) — phase 1 ships the stub; phase 2 wires
    /// real lifecycle. Until phase 1 closes, select() returns
    /// `GoNotLinked`.
    Go,
}

#[derive(Debug, Error)]
pub enum EngineSelectError {
    /// Reserved for cross-platform builds where JSC isn't yet linked
    /// (Linux/Windows pre-M12.3b-phase4). Currently unused on macOS,
    /// where the system framework provides JSC.
    #[error(
        "engine `jsc` is not linked into this build of `grx`. \n\
         macOS is supported via the system JavaScriptCore.framework; \n\
         Linux/Windows via vendored WebKit lands in M12.3b-phase4."
    )]
    JscNotLinked,
    /// Reserved for builds where CPython isn't yet linked.
    /// All platforms now resolve via pkg-config (`python-3.X-embed`,
    /// 3.12+ floor); see `crates/graphreact-engine-cpython/build.rs`.
    #[error(
        "engine `cpython` is not linked into this build of `grx`. \n\
         Resolved via `pkg-config python-3.X-embed` (3.12+ required). \n\
         Install python@3.12 / 3.13 / 3.14 and ensure pkg-config sees it."
    )]
    CpythonNotLinked,
    /// Reserved for cross-platform builds where the Go engine isn't
    /// wired (currently unused — the engine is platform-agnostic
    /// since cgo + c-shared works on Linux/macOS/Windows). Kept as a
    /// variant so the error surface is stable if a future build
    /// gates the Go engine off (e.g., a future `--no-go` feature
    /// flag).
    #[allow(dead_code)]
    #[error(
        "engine `go` is not linked into this build of `grx`. \n\
         The `graphreact-engine-go` crate is built by default — see \n\
         ~/.claude/plans/graphreact-rust-later-milestones.md §M17 for details."
    )]
    GoNotLinked,
}

/// Construct a boxed engine from a CLI selection.
pub fn select(kind: EngineKind) -> Result<Box<dyn Engine>, EngineSelectError> {
    match kind {
        EngineKind::Null => {
            let engine = graphreact_engine_null::NullEngine::new();
            Ok(Box::new(engine))
        }
        EngineKind::Echo => {
            let engine = graphreact_engine_echo::EchoEngine::new();
            Ok(Box::new(engine))
        }
        EngineKind::Jsc => {
            #[cfg(target_os = "macos")]
            {
                let engine = graphreact_engine_jsc::JscEngine::new();
                Ok(Box::new(engine))
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(EngineSelectError::JscNotLinked)
            }
        }
        EngineKind::Cpython => {
            #[cfg(target_os = "macos")]
            {
                let engine = graphreact_engine_cpython::CPythonEngine::new();
                Ok(Box::new(engine))
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(EngineSelectError::CpythonNotLinked)
            }
        }
        EngineKind::Go => {
            // M17 phase 3a: construct a fresh GoEngine. The caller
            // is responsible for calling `set_artifact` (or using
            // [`crate::goruntime::GoRuntime::build_and_load`]) before
            // `initialize`. The bare `select()` path is mostly
            // useful for substrate composition; production callers
            // route through `goruntime`.
            let engine = graphreact_engine_go::GoEngine::new();
            Ok(Box::new(engine))
        }
    }
}

impl From<crate::cli::RuntimeArg> for EngineKind {
    fn from(arg: crate::cli::RuntimeArg) -> Self {
        match arg {
            crate::cli::RuntimeArg::Null => Self::Null,
            crate::cli::RuntimeArg::Echo => Self::Echo,
            crate::cli::RuntimeArg::Jsc => Self::Jsc,
            crate::cli::RuntimeArg::Cpython => Self::Cpython,
            crate::cli::RuntimeArg::Go => Self::Go,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_engine_is_constructible() {
        let e = select(EngineKind::Null).expect("ok");
        assert_eq!(e.engine_name(), "null");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn jsc_constructs_on_macos() {
        let e = select(EngineKind::Jsc).expect("jsc should build on macOS via system framework");
        assert_eq!(e.engine_name(), "jsc");
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn jsc_returns_clear_error_on_non_macos() {
        let result = select(EngineKind::Jsc);
        let Err(err) = result else {
            panic!("expected error on non-macOS");
        };
        let msg = format!("{err}");
        assert!(msg.contains("M12.3"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn cpython_constructs_on_macos() {
        let e = select(EngineKind::Cpython)
            .expect("cpython should build on macOS via system Python framework");
        assert_eq!(e.engine_name(), "cpython");
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn cpython_returns_clear_error_on_non_macos() {
        let result = select(EngineKind::Cpython);
        let Err(err) = result else {
            panic!("expected error on non-macOS");
        };
        let msg = format!("{err}");
        assert!(msg.contains("M13"));
    }

    #[test]
    fn go_engine_is_constructible() {
        let e = select(EngineKind::Go).expect("ok");
        assert_eq!(e.engine_name(), "go");
    }
}
