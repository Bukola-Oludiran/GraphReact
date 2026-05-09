//! **M16-Backend Phase 8/9.** Per-body source-debug protocol install
//! helpers (Axis B per [decision 0002]). Engine-native debug protocol
//! consumed by the engine-native IDE — never a browser; see
//! [feedback_axis_b_no_browser.md] for the symmetric framing.
//!
//! - [`flag::DebugFlag`] — shared `--debug=<engine>:<port>` parser.
//! - [`cpython_debugpy::install_debugpy`] — P8 install path for
//!   CPython bodies via debugpy's `listen()` API. Consumer: VS Code
//!   Python extension / PyCharm.
//! - [`jsc_inspector::install_jsc_inspector`] — P9 install path for
//!   JSC bodies via WebKit's RemoteInspector C SPI. Consumer: Safari
//!   Web Inspector via Apple's lockdownd discovery
//!   (`Safari → Develop → [Hostname] → JSContext`). CDP-over-TCP
//!   attach (VS Code's JS debugger / Chrome DevTools / WebStorm)
//!   defers with V8 per [decision 0001].
//!
//! Engine-vs-`--debug=<engine>` mismatch is validated in the CLI
//! dispatch arm via [`crate::commands::validate_debug_against_runtime`].
//!
//! [decision 0001]: ../../../../docs/decisions/0001-v8-engine-question.md
//! [decision 0002]: ../../../../docs/decisions/0002-graphreact-is-backend-only.md

#[cfg(target_os = "macos")]
pub mod cpython_debugpy;
pub mod flag;
#[cfg(target_os = "macos")]
pub mod jsc_inspector;

pub use flag::{DebugEngine, DebugFlag, DebugFlagError};
