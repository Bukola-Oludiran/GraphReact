//! Engine-agnostic Rust trait surface for embedding host languages
//! into GraphReact's standalone runtime.
//!
//! Each concrete engine (JSC, CPython, V8, JVM, …) ships its own
//! `graphreact-engine-<name>` crate that implements [`Engine`].
//!
//! The runtime calls into [`Engine`] via the trait — it never knows
//! which engine is loaded. The CLI shim selects the engine at startup
//! by `--runtime=jsc|python|...`.
//!
//! ## Lifecycle
//!
//! ```text
//! Engine::initialize() → engine handle
//!   ↓
//! Engine::create_isolate(opts) → IsolateHandle
//!   ↓ for each component definition:
//! Engine::eval_module(iso, src) → ModuleHandle
//! Engine::register_callback(iso, "host_send", cb)  // bridge to scheduler
//!   ↓ on each scheduler tick:
//! Engine::call_function(iso, fn_handle, args)
//! Engine::drain_microtasks(iso)
//!   ↓
//! Engine::dispose(iso)
//! Engine::shutdown()
//! ```
//!
//! Concrete engines vary in their implementation language (JSC in Zig
//! per M12; CPython via Zig+pyo3 patterns per M13; V8 in Zig per M17).
//! The trait pins the contract so `graphreact-cli` can swap them at
//! startup.

#![allow(clippy::module_name_repetitions)]

pub mod error;
pub mod handles;
pub mod options;
pub mod traits;
pub mod value;

pub use error::{EngineError, EngineResult};
pub use handles::{IsolateHandle, ModuleHandle, FunctionHandle, CallbackId};
pub use options::{IsolateOptions, MicrotaskPolicy};
pub use traits::{BoxCallback, Engine, EngineCallback, HostCall};
pub use value::{HostObject, HostValue, HostValueKind};
