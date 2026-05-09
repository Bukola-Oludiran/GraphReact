//! `grx` standalone CLI — library half.
//!
//! The binary entry point lives at `src/bin/grx.rs` and dispatches
//! into the subcommand modules here. Splitting the CLI into a library
//! lets tests import subcommand handlers directly without spawning
//! processes.
//!
//! ## Engine selection (M12.1 scope)
//!
//! `--runtime=null` (default for now): runs through the
//! [`graphreact_engine_null::NullEngine`] — parses + builds IR + dry-
//! mounts the topology without executing component bodies. Useful for
//! smoke-testing the wiring.
//!
//! `--runtime=jsc`: errors with a clear "not yet linked; see M12.3"
//! message. The plumbing is in place; the engine just isn't wired
//! up. (Was `--runtime=v8` pre-2026-05-01; swapped to JSC because
//! Bun is the worked example for embedding JSC via Zig.)

#![allow(clippy::module_name_repetitions)]

pub mod cli;
pub mod commands;
pub mod debug;
pub mod dist_reload_bridge;
pub mod engine_select;
pub mod fmt;
pub mod hotreload_adapter;
pub mod observer_client;
pub mod observer_commands;
pub mod observer_tui;
#[cfg(target_os = "macos")]
pub mod jsruntime;
#[cfg(target_os = "macos")]
pub mod pyruntime;
pub mod runtime;
pub mod sources;
pub mod watch_mode;

pub use cli::{Cli, Command};
pub use engine_select::{EngineKind, EngineSelectError};
