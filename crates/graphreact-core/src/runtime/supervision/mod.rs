//! OTP-inspired supervision layer.
//!
//! Provides three composable observation/control primitives that map onto
//! the OTP patterns from `lib/stdlib/src/supervisor.erl`,
//! `lib/kernel/src/pg.erl`, and `lib/stdlib/src/gen_statem.erl`:
//!
//! * **[`monitor`]** — unidirectional observation. `monitor(target)` returns
//!   a [`MonitorRef`]; when `target` unmounts, the observer receives a
//!   `__DOWN__` system message bearing the ref and termination reason.
//! * **[`link`]** — bidirectional ownership. Linked nodes propagate
//!   abnormal exits to one another via `__EXIT__` messages. Use it for
//!   parent-child supervision, monitor for everything else.
//! * **[`Supervisor`]** — restart-strategy-driven recovery. Wraps a set of
//!   children with policy: `OneForOne`, `OneForAll`, `RestForOne`,
//!   `SimpleOneForOne`. Bounds restart attempts via intensity/period.
//! * **[`GroupRegistry`]** — pg-style process groups. Nodes join named
//!   groups; broadcasts route to all current members.
//! * **[`postpone`]** — gen_statem-style selective receive. A handler can
//!   defer a message; the scheduler replays postponed messages on the
//!   next state transition the node observes.
//!
//! These primitives layer onto the M4 scheduler — they don't replace it.
//! User-facing hooks (`use_monitor`, `use_link`, `use_group`) keep the
//! same call-order contract as React-style hooks.

pub mod groups;
pub mod monitor;
pub mod postpone;
pub mod supervisor;

pub use groups::{GroupName, GroupRegistry};
pub use monitor::{ExitReason, LinkRegistry, MonitorRef, MonitorRegistry};
pub use postpone::{postpone, PostponeQueue};
pub use supervisor::{
    ChildId, ChildSpec, RestartPlan, RestartPolicy, RestartStrategy, RestartTracker, Supervisor,
    SupervisorError,
};

/// System-message type emitted to a monitor when its target node unmounts.
///
/// Payload shape: `{ "ref": <u64>, "target": <node_id>, "reason": <reason_string> }`
pub const SYSTEM_DOWN: &str = "__DOWN__";

/// System-message type emitted to a linked peer when the linked node
/// unmounts abnormally (or always, if `trap_exit` semantics are enabled).
///
/// Payload shape: `{ "from": <node_id>, "reason": <reason_string> }`
pub const SYSTEM_EXIT: &str = "__EXIT__";
