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
/// **M-Prod sub-track #3 (Phase A).** `grx secrets …` dispatcher.
pub mod secrets_commands;
/// **M-Prod sub-track #1 Phase A.** `grx audit …` dispatcher.
/// Today's surface is the hash-chain verifier for the NDJSON file
/// produced by `--observer-audit-log`. Phase B adds replay /
/// inspect / report operations.
pub mod audit_commands;
/// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** `grx admin …`
/// dispatcher. Today's surface is the SIGHUP-driven
/// `reload-tokens` operator command; Group 1cd adds JWT-OIDC
/// reload + remote HTTP-driven admin endpoints.
pub mod admin_commands;
#[cfg(target_os = "macos")]
pub mod jsruntime;
#[cfg(target_os = "macos")]
pub mod pyruntime;
/// Go body runtime — **M17 phase 3a skeleton**. Mirrors `pyruntime`
/// (M13 CPython) and `jsruntime` (M12.3 JSC) for `lang="go"`
/// definitions. Currently ships `GoRuntime::build_and_load` which
/// drives the codegen + build + load + initialize round-trip; full
/// scheduler integration lands at M17 phase 3a follow-on (alongside
/// the hook surface 3c–h).
pub mod goruntime;
/// **M22 — Mixed-runtime orchestrator.** Hosts JSC + CPython + Go
/// engines in one daemon. Initialize-all-before-mount + reverse-
/// order shutdown + per-component dispatch via Phase 1's relaxed
/// per-engine mount paths. See `docs/backend/m22-mixed-runtime.md`.
#[cfg(target_os = "macos")]
pub mod mixedruntime;
pub mod runtime;
pub mod sources;
pub mod watch_mode;

pub use cli::{Cli, Command};
pub use engine_select::{EngineKind, EngineSelectError};
