//! GraphReact M15 — Hot reload (second BEAM-class property).
//!
//! Live code swap on a running graph. Compatible-change reload preserves
//! `useState` values + pending message queue; incompatible-change drains
//! gracefully (no message loss for at-least-once channels); rollback
//! fires on first-tick failure of new code.
//!
//! Pipeline:
//!
//! ```text
//! .grx file changes  →  FileWatcher (notify)
//!         ↓
//! IR re-parse (graphreact-grx-parser)
//!         ↓
//! Diff classifier  →  ReloadDelta { Compatible | Incompatible | TopologyOnly | Mixed }
//!         ↓
//! Apply orchestrator
//!     │
//!     ├─ Compatible      → Scheduler::replace_definition (preserves HookSlot)
//!     ├─ Incompatible    → wait-for-quiescence → unmount → re-mount
//!     └─ Topology-only   → emit TopologyEvent::EdgeAdded / EdgeRemoved
//!         ↓
//! Rollback on first-tick failure (RollbackGuard)
//! ```
//!
//! See [docs/backend/m15-hot-reload.md](GraphReact/graphreactRust/docs/backend/m15-hot-reload.md)
//! for the full milestone narrative.

pub mod diff;
pub mod hook_signature;
pub mod orchestrator;
pub mod rollback;
pub mod safe_point;
pub mod watcher;

pub use diff::{classify, DiffCache, EdgeDelta, NodeDelta, NodeDeltaKind, ReloadDelta};
pub use hook_signature::{
    extract as extract_hook_signature, extract_js as extract_js_hook_signature,
    extract_python as extract_python_hook_signature, HookCall, HookKind, HookSignature,
    HookSignatureError,
};
pub use orchestrator::{
    ApplyError, EdgeApplyOutcome, EngineAdapter, NodeApplyOutcome, ReloadOrchestrator,
    ReloadOutcome, RemountInfo,
};
pub use rollback::{RollbackGuard, RollbackOutcome};
pub use safe_point::{
    wait_for_global_quiescence, QuiescenceError, DEFAULT_QUIESCENCE_TIMEOUT, MAX_DRAIN_CYCLES,
};
pub use watcher::{FileWatcher, WatchEvent, WatcherError};
