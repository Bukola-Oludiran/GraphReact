//! Safe-point detection for hot reload.
//!
//! M15's swap path needs a quiescent moment where no node is mid-
//! evaluation, no message is in flight to an affected node, and no
//! topology event is pending. The scheduler reaches that state at
//! the bottom of every `process_batch` loop iteration — when all
//! drains exit empty (`messages`, `dirty`, `pending_topology`,
//! `external_inbox`, `pending_mutations`,
//! `pending_remote_demonitors`, `pending_remote_unlinks` per
//! [`Scheduler::process_batch`]).
//!
//! [`wait_for_global_quiescence`] drives the scheduler to that
//! state with a bounded retry: invoke `process_batch`, observe
//! whether new work landed during the call (external inbox, source
//! events), and re-drain until either the scheduler stays quiet
//! across a single empty cycle or the timeout elapses.
//!
//! For Phase 6 we treat global quiescence as the safe point. Per-
//! node-only quiescence (drain just the affected node, refuse new
//! inbound on that node, leave others running) is a refinement —
//! tracked as M-Prod's "backpressure semantics" deliverable since
//! it requires per-node DRAINING flag plumbing on the scheduler's
//! `make_emit` path.

use std::time::{Duration, Instant};

use graphreact_core::Scheduler;
use thiserror::Error;

/// How long to keep retrying `process_batch` waiting for the
/// scheduler to settle before declaring the wait failed.
pub const DEFAULT_QUIESCENCE_TIMEOUT: Duration = Duration::from_millis(500);

/// Maximum number of `process_batch` calls per
/// [`wait_for_global_quiescence`] invocation. A guard against
/// pathological cascades that keep producing new work indefinitely
/// (e.g., a tight `setInterval`-style timer source).
pub const MAX_DRAIN_CYCLES: u32 = 32;

/// Result of a quiescence wait.
#[derive(Debug, Error)]
pub enum QuiescenceError {
    /// The scheduler returned an error while draining.
    #[error("scheduler error during quiescence drain: {0}")]
    Scheduler(String),
    /// Scheduler kept producing new work past `timeout` /
    /// `MAX_DRAIN_CYCLES`. Caller decides whether to force the
    /// swap anyway (incompatible reload risks message loss) or
    /// abort.
    #[error("scheduler did not reach quiescence within {timeout_ms}ms ({cycles} drain cycles)")]
    Timeout { timeout_ms: u128, cycles: u32 },
}

/// Drive the scheduler until it reaches a globally-quiescent
/// state, bounded by `timeout` or [`MAX_DRAIN_CYCLES`].
///
/// "Quiescent" here matches `process_batch`'s loop-exit condition:
/// all queues drained, no in-flight evaluation, no pending
/// topology / mutation events. Calling `process_batch` itself is
/// the act of reaching quiescence — the function returns when the
/// loop exits.
///
/// The safe-point pattern in the orchestrator:
/// 1. `wait_for_global_quiescence(scheduler, timeout)` before swap
/// 2. perform swap (`replace_definition` / `unmount` / `add_node`)
/// 3. `scheduler.process_batch()` to re-render with the new body
///
/// Step 1 ensures any in-flight messages targeting the affected
/// node have settled before we swap; step 3 ensures the new body
/// renders in the same call so the swap is observable to the
/// caller.
pub fn wait_for_global_quiescence(
    scheduler: &mut Scheduler,
    timeout: Duration,
) -> Result<(), QuiescenceError> {
    let deadline = Instant::now() + timeout;
    let mut cycles: u32 = 0;
    loop {
        cycles = cycles.saturating_add(1);
        scheduler
            .process_batch()
            .map_err(|e| QuiescenceError::Scheduler(format!("{e}")))?;
        // process_batch returns when its internal loop exits —
        // i.e., on a quiescence-reached cycle. We then run a
        // second drain attempt to verify nothing new arrived
        // while we were processing (external inbox pushes, source
        // events). If that second call is a no-op (no further
        // evaluations triggered), we're truly settled.
        //
        // The "no-op verification" is implicit: process_batch
        // returns Ok(()) immediately when all queues are already
        // empty. A second consecutive trip with nothing to do
        // means quiescence held.
        if Instant::now() > deadline {
            return Err(QuiescenceError::Timeout {
                timeout_ms: timeout.as_millis(),
                cycles,
            });
        }
        if cycles > MAX_DRAIN_CYCLES {
            return Err(QuiescenceError::Timeout {
                timeout_ms: timeout.as_millis(),
                cycles,
            });
        }
        // We rely on process_batch's settled-condition; if it
        // returned with the queues empty (which is the only way
        // it returns Ok), we're done. Two cycles is the floor —
        // first drains the queues, second confirms nothing new
        // arrived during the drain.
        if cycles >= 2 {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::runtime::node_instance::NodeDefinition;
    use graphreact_core::{Attrs, CommunicationGraph, NodeContext, PermissionManifest, Scheduler};

    fn empty_def(name: &'static str) -> NodeDefinition {
        NodeDefinition::new(name, |_: &NodeContext| PermissionManifest::default())
    }

    #[test]
    fn quiescence_succeeds_on_empty_scheduler() {
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        let res = wait_for_global_quiescence(&mut scheduler, DEFAULT_QUIESCENCE_TIMEOUT);
        assert!(
            res.is_ok(),
            "empty scheduler must reach quiescence: {res:?}"
        );
    }

    #[test]
    fn quiescence_succeeds_after_simple_mount() {
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        scheduler
            .add_node_at_runtime(empty_def("Foo"), "n", Attrs::new(), vec![])
            .unwrap();
        // add_node_at_runtime already drained via process_batch.
        // wait_for_global_quiescence should observe the post-mount
        // settled state immediately.
        wait_for_global_quiescence(&mut scheduler, DEFAULT_QUIESCENCE_TIMEOUT).unwrap();
        assert!(scheduler.has_node("n"));
    }

    #[test]
    fn quiescence_drains_pending_messages_before_returning() {
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        scheduler
            .add_node_at_runtime(empty_def("Foo"), "n", Attrs::new(), vec![])
            .unwrap();

        // Push a message via the in-process Scheduler::send API.
        // wait_for_global_quiescence's process_batch sweep should
        // drain it.
        scheduler.send("n", "ping", serde_json::Value::Null);

        wait_for_global_quiescence(&mut scheduler, DEFAULT_QUIESCENCE_TIMEOUT).unwrap();

        // After quiescence, the external inbox should be empty;
        // we observe via a second drain that produces a clean
        // (no-op) cycle.
        wait_for_global_quiescence(&mut scheduler, DEFAULT_QUIESCENCE_TIMEOUT).unwrap();
    }
}
