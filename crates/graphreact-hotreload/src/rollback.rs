//! Rollback support for hot reload.
//!
//! Architecturally derived from Bun's `hot_reloader.zig`
//! module-replacement-rollback semantics
//! (<https://github.com/oven-sh/bun>, MIT-licensed). Bun rolls
//! back a failed module reload by restoring the prior export
//! object; GraphReact rolls back by restoring the prior
//! [`NodeDefinition`] via [`Scheduler::replace_definition`].
//!
//! # Pattern
//!
//! 1. Before each compatible swap, snapshot the prior
//!    `NodeDefinition` via [`Scheduler::get_definition`] and
//!    record a [`RollbackGuard`].
//! 2. Apply the swap (`Scheduler::replace_definition` for
//!    Compatible; `unmount + add_node_at_runtime` for
//!    Incompatible).
//! 3. Drive `Scheduler::process_batch` to render the new body.
//! 4. If `process_batch` returns `Err`, OR if a per-orchestrator
//!    health check fires (Phase 9 wires this from CLI), restore
//!    every recorded guard via `replace_definition(prior)`.
//! 5. On commit (success), drop guards.
//!
//! # What rollback covers (and doesn't)
//!
//! Covers: a new body that panics during `evaluate_node`, returns
//! an error code that bubbles up through `process_batch`, or
//! crashes the engine FFI in a way the engine adapter surfaces.
//!
//! Does NOT cover: incompatible-reload remounts. Once a node is
//! `unmount_with_reason`'d, its `HookSlot` storage + monitors +
//! links are gone — there's no way to restore "the prior node"
//! from a `NodeDefinition` alone. M15 acceptance scopes rollback
//! to compatible reloads (the most common path); incompatible
//! reload's "rollback" is "operator notices the error, fixes the
//! source, saves again — orchestrator runs another reload."
//!
//! Per-orchestrator policy: rollback is opt-in via
//! [`ReloadOrchestrator::with_rollback`] (default off) — a stable
//! production deployment may want failures to surface explicitly
//! rather than silently revert. Development with `--watch` should
//! enable rollback so a typo doesn't take down the running graph.

use graphreact_core::runtime::node_instance::NodeDefinition;

/// Snapshot of a node's pre-swap definition. Held by the
/// orchestrator across an `apply` invocation; restored if the
/// post-swap `process_batch` fails.
#[derive(Debug, Clone)]
pub struct RollbackGuard {
    /// Node id whose definition we snapshotted.
    pub node_id: String,
    /// The component-name + closure pair that was live before the
    /// swap. `NodeDefinition::component` is `Rc<dyn Fn>` so the
    /// clone is cheap.
    pub prior_def: NodeDefinition,
}

impl RollbackGuard {
    pub fn new(node_id: impl Into<String>, prior_def: NodeDefinition) -> Self {
        Self {
            node_id: node_id.into(),
            prior_def,
        }
    }
}

/// Outcome of a rollback attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackOutcome {
    /// Number of nodes successfully restored to their prior
    /// definition.
    pub restored: usize,
    /// Per-node ids that failed to restore (e.g., the node was
    /// removed during the failed apply and `replace_definition`
    /// errors with `NodeNotRegistered`).
    pub failed: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::runtime::node_instance::NodeDefinition;
    use graphreact_core::{NodeContext, PermissionManifest};

    #[test]
    fn rollback_guard_clones_definition_cheaply() {
        let def = NodeDefinition::new("Foo", |_: &NodeContext| {
            PermissionManifest::default()
        });
        let g = RollbackGuard::new("n", def);
        assert_eq!(g.node_id, "n");
        // Cloning the guard duplicates the Rc inside the
        // definition — no panic, no recursive copy.
        let g2 = g.clone();
        assert_eq!(g2.node_id, "n");
    }
}
