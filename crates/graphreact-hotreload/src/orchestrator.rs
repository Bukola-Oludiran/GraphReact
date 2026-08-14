//! Apply orchestrator — drives a [`ReloadDelta`] against a live
//! [`Scheduler`].
//!
//! Architecturally derived from Bun's `hot_reloader.zig`
//! (<https://github.com/oven-sh/bun>, MIT-licensed). Bun is licensed
//! under the MIT License; we port the architectural patterns
//! (rollback semantics, module-replacement-with-state-preservation)
//! into Rust over GraphReact's per-node lifecycle.
//!
//! # Apply pipeline
//!
//! For a [`ReloadDelta::Compatible`] entry:
//!   1. Snapshot the current `NodeDefinition` for rollback (Phase 7).
//!   2. Build a new `NodeDefinition` via the [`EngineAdapter`].
//!   3. Call `Scheduler::replace_definition(id, new_def)`.
//!   4. The next `process_batch` re-evaluates the node against
//!      existing `HookSlot` storage — `useState` / `useEffect`
//!      committed-deps / `useMemo` cache survive the swap.
//!
//! For a [`ReloadDelta::Incompatible`] entry:
//!   1. Wait for quiescence on the node (Phase 6 — handled by
//!      [`crate::safe_point`]). For now, the orchestrator just
//!      proceeds; the integration with safe-point detection lands
//!      in Phase 6.
//!   2. `unmount_with_reason(key, ExitReason::Shutdown)` — runs
//!      effect cleanups, fires `__DOWN__` to monitors / `__EXIT__`
//!      to links, removes from registries.
//!   3. `add_node_at_runtime(new_def, id, props, connect_to)` —
//!      fresh mount.
//!
//! For each [`EdgeDelta`]:
//!   1. Mutate the underlying `CommunicationGraph` (add / remove
//!      edge); the existing topology dispatcher fans out
//!      `TopologyEvent`s to subscribers.
//!
//! Phase 5 ships the apply mechanics; Phase 6 layers safe-point
//! detection on top; Phase 7 layers rollback. Each phase tightens
//! the orchestrator without changing its public surface.

use std::collections::HashMap;
use std::time::Duration;

use graphreact_core::runtime::node_instance::NodeDefinition;
use graphreact_core::runtime::supervision::ExitReason;
use graphreact_core::Attrs;
use graphreact_core::Scheduler;
use graphreact_grx_parser::ast::ComponentLang;
use thiserror::Error;

use crate::diff::{EdgeDelta, NodeDelta, NodeDeltaKind, ReloadDelta};
use crate::rollback::{RollbackGuard, RollbackOutcome};
use crate::safe_point::{wait_for_global_quiescence, DEFAULT_QUIESCENCE_TIMEOUT};

/// Engine adapter trait — bridges from "definition body source +
/// language tag" to a runtime [`NodeDefinition`].
///
/// Implementations:
/// - **Tests / pure-Rust**: a closure-based adapter that returns
///   a hard-coded `NodeDefinition` for the given component name.
/// - **Phase 8**: real adapters in `graphreact-cli` that compile
///   the body via JSC's `eval_component` (JS) or CPython's
///   equivalent (Python), preserving the same isolate handle so
///   engine-side state (callback registry, sub-interpreter
///   memory) survives the swap.
pub trait EngineAdapter {
    /// Build a [`NodeDefinition`] for `component` from `code`. The
    /// adapter owns the engine handle (isolate) and reuses it
    /// across calls so engine-side state is preserved.
    fn build_definition(
        &self,
        component: &str,
        code: &str,
        lang: ComponentLang,
    ) -> Result<NodeDefinition, ApplyError>;
}

/// Errors that can arise during apply.
#[derive(Debug, Clone, Error)]
pub enum ApplyError {
    /// Engine adapter failed to compile / register the new body.
    #[error("engine adapter failed for component `{component}`: {message}")]
    EngineFailure { component: String, message: String },
    /// Scheduler returned a runtime error during `replace_definition`
    /// or `add_node_at_runtime`.
    #[error("scheduler runtime error: {0}")]
    Scheduler(String),
    /// Required props for a remount were missing — e.g. the
    /// `Incompatible::Added` path was hit but the orchestrator
    /// has no way to derive props for the new node.
    #[error("missing remount metadata for node `{node_id}`: {detail}")]
    MissingRemountInfo { node_id: String, detail: String },
}

/// Per-node apply result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeApplyOutcome {
    /// Body swapped in place via `replace_definition`.
    Replaced,
    /// Drained, unmounted, and re-mounted with new spec.
    Remounted,
    /// Recorded for rollback if the next tick fails (Phase 7).
    Rolled,
    /// Skipped — orchestrator declined to apply (e.g., adapter
    /// reported a parse failure that should defer to the next
    /// reload, NOT crash the running system).
    Skipped { reason: String },
}

/// Per-edge apply result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeApplyOutcome {
    Added,
    Removed,
    AttrsChanged,
}

/// Top-level apply outcome — what got applied, what failed.
#[derive(Debug, Clone)]
pub struct ReloadOutcome {
    pub nodes: Vec<(String, NodeApplyOutcome)>,
    pub edges: Vec<(String, String, EdgeApplyOutcome)>,
    pub errors: Vec<ApplyError>,
    /// Phase 7: if rollback was enabled and the post-swap
    /// `process_batch` failed, this records what the orchestrator
    /// did about it. `None` if rollback was disabled OR the
    /// post-swap render was clean.
    pub rollback: Option<RollbackOutcome>,
}

impl ReloadOutcome {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            errors: Vec::new(),
            rollback: None,
        }
    }

    /// Returns `true` if no errors fired during apply.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.errors.is_empty()
    }

    /// Returns `true` if rollback fired during apply.
    #[must_use]
    pub fn was_rolled_back(&self) -> bool {
        self.rollback
            .as_ref()
            .is_some_and(|r| r.restored > 0 || !r.failed.is_empty())
    }
}

/// Per-node remount metadata. The orchestrator's caller supplies
/// this for any `NodeDelta::Added` so the new node mounts with
/// correct props + connect_to neighbours.
///
/// For `BodyChangedIncompatible` and `Removed` paths, the
/// orchestrator can derive props from the running scheduler's
/// existing node state — but for `Added`, those don't exist yet.
#[derive(Debug, Clone, Default)]
pub struct RemountInfo {
    pub props: Attrs,
    pub connect_to: Vec<String>,
}

/// Apply orchestrator. Holds engine adapter + per-component
/// remount-info hints + per-orchestrator safe-point timeout +
/// rollback policy.
pub struct ReloadOrchestrator<A: EngineAdapter> {
    adapter: A,
    /// Per-node-id remount info, supplied by the caller. Required
    /// for `NodeDelta::Added`; optional for `BodyChangedIncompatible`
    /// (orchestrator falls back to the existing node's props).
    remount_info: HashMap<String, RemountInfo>,
    /// How long to wait for the scheduler to reach global
    /// quiescence before swapping nodes. Defaults to
    /// [`DEFAULT_QUIESCENCE_TIMEOUT`]. Override for noisy systems
    /// where the steady-state event rate keeps the scheduler busy
    /// past 500ms.
    quiescence_timeout: Duration,
    /// Phase 7: when `true`, the orchestrator snapshots prior
    /// `NodeDefinition`s for compatible-reload swaps and restores
    /// them if the post-swap `process_batch` returns `Err`.
    /// Defaults to `true` (development with `--watch` wants
    /// rollback by default; production deployments can disable).
    rollback_enabled: bool,
    /// **M16-Backend Phase 1.** Optional telemetry sink. When
    /// installed, `apply()` fires `on_hot_reload` once after the
    /// delta has been applied (or rolled back) with a per-apply
    /// summary (replaced / remounted counts, edge changes,
    /// rollback status, error count). Per the M16-Backend Phase-1
    /// architectural decision, the orchestrator holds its own slot
    /// rather than routing through the scheduler — keeps the trait
    /// close to where the events fire and matches the existing M15
    /// Phase 10 sink shape.
    ///
    /// Storage: `arc_swap::ArcSwapOption<TelemetrySinkRef>` — same
    /// lock-free shape as Scheduler + dist's slots; the
    /// `TelemetrySinkRef` wrapper lets `Arc<dyn TelemetrySink>` live
    /// in `arc_swap` (which requires `T: Sized`).
    telemetry_sink: arc_swap::ArcSwapOption<graphreact_core::TelemetrySinkRef>,
}

impl<A: EngineAdapter> ReloadOrchestrator<A> {
    pub fn new(adapter: A) -> Self {
        Self {
            adapter,
            remount_info: HashMap::new(),
            quiescence_timeout: DEFAULT_QUIESCENCE_TIMEOUT,
            rollback_enabled: true,
            telemetry_sink: arc_swap::ArcSwapOption::empty(),
        }
    }

    /// **M16-Backend Phase 1.** Install a telemetry sink. Subsequent
    /// `apply()` calls fire `on_hot_reload` once at completion with
    /// a per-apply summary. Re-installation replaces the previous
    /// sink. Pass the SAME `Arc<dyn TelemetrySink>` to
    /// [`graphreact_core::Scheduler::install_telemetry_sink`] +
    /// `Cluster::install_telemetry_sink` for unified consumer
    /// registration; a `MultiplexSink` from `graphreact-observer`
    /// fans out to multiple consumers behind a single handle.
    pub fn install_telemetry_sink(&self, sink: std::sync::Arc<dyn graphreact_core::TelemetrySink>) {
        self.telemetry_sink.store(Some(std::sync::Arc::new(
            graphreact_core::TelemetrySinkRef::new(sink),
        )));
    }

    /// Clear the installed telemetry sink. Test surface; production
    /// teardown can also use this during graceful shutdown.
    pub fn clear_telemetry_sink(&self) {
        self.telemetry_sink.store(None);
    }

    /// Register remount info for a node id. Required before
    /// applying a delta that includes `NodeDelta::Added` for that
    /// id.
    pub fn set_remount_info(&mut self, node_id: impl Into<String>, info: RemountInfo) {
        self.remount_info.insert(node_id.into(), info);
    }

    /// Override the safe-point timeout used before incompatible
    /// reloads' drain step.
    pub fn set_quiescence_timeout(&mut self, timeout: Duration) {
        self.quiescence_timeout = timeout;
    }

    /// Toggle rollback support (Phase 7). When enabled, the
    /// orchestrator snapshots prior `NodeDefinition`s before each
    /// compatible swap and restores them if the post-swap
    /// `process_batch` errors. Default: `true`.
    pub fn set_rollback_enabled(&mut self, enabled: bool) {
        self.rollback_enabled = enabled;
    }

    /// Apply a [`ReloadDelta`] against the running scheduler.
    ///
    /// Returns a [`ReloadOutcome`] enumerating per-node and
    /// per-edge apply results plus any errors. The orchestrator
    /// continues past per-node errors so a failure on one node
    /// doesn't block the rest of the delta from applying.
    pub fn apply(&mut self, scheduler: &mut Scheduler, delta: ReloadDelta) -> ReloadOutcome {
        let mut outcome = ReloadOutcome::new();
        let (compat, incompat, topology) = match delta {
            ReloadDelta::NoChange => return outcome,
            ReloadDelta::Compatible(c) => (c, Vec::new(), Vec::new()),
            ReloadDelta::Incompatible(i) => (Vec::new(), i, Vec::new()),
            ReloadDelta::TopologyOnly(t) => (Vec::new(), Vec::new(), t),
            ReloadDelta::Mixed {
                compat,
                incompat,
                topology,
            } => (compat, incompat, topology),
        };

        // Order matters:
        //   1. Compatible reloads first — pure in-place swaps that
        //      don't disturb topology or messaging.
        //   2. Topology second — edge changes can affect what a
        //      remount-bound node connects to.
        //   3. Incompatible last — drain + remount is the most
        //      disruptive; running it after compat means the
        //      compatible nodes are already on their new bodies
        //      when the unmount-then-remount path fires.

        // Phase 7: snapshot prior definitions for every
        // compatible swap BEFORE applying. If the post-swap
        // render fails, we restore from these guards.
        let mut guards: Vec<RollbackGuard> = Vec::new();
        if self.rollback_enabled {
            for delta in &compat {
                if let NodeDeltaKind::BodyChangedCompatible { .. } = &delta.kind {
                    if let Some(prior) = scheduler.get_definition(&delta.node_id) {
                        guards.push(RollbackGuard::new(delta.node_id.clone(), prior));
                    }
                }
            }
        }

        for delta in compat {
            self.apply_compat(scheduler, &delta, &mut outcome);
        }
        for delta in topology {
            self.apply_edge(scheduler, &delta, &mut outcome);
        }

        // Phase 6 — safe-point gate. Drain the scheduler to
        // quiescence once before the incompatible batch so any
        // in-flight messages targeting the to-be-remounted nodes
        // settle before we tear them down. Per-node DRAINING
        // gating (refuse new inbound on individual nodes) is an
        // M-Prod refinement; for M15 the global drain is the safe
        // point.
        if !incompat.is_empty() {
            if let Err(e) = wait_for_global_quiescence(scheduler, self.quiescence_timeout) {
                outcome.errors.push(ApplyError::Scheduler(format!(
                    "safe-point wait failed: {e}"
                )));
                // Continue anyway — bounded-by-timeout drain is
                // best-effort. The per-node remount path itself
                // doesn't require strict pre-quiescence to be
                // correct (unmount_with_reason runs cleanups
                // synchronously); strict quiescence is a
                // message-loss guarantee, not a memory-safety one.
            }
        }

        for delta in incompat {
            self.apply_incompat(scheduler, &delta, &mut outcome);
        }

        // Phase 7 — first-tick check. Drive `process_batch` to
        // render the new bodies; if it errors OR panics AND rollback
        // is enabled AND we have guards (i.e., compatible swaps
        // happened), restore the prior definitions and re-render
        // against them.
        //
        // **M15 Phase 7 closeout.** The original Phase 7 only
        // covered the `Err`-return case. A Rust panic in an effect
        // callback OR a host hook (e.g. `Signal::get` on a dropped
        // cell, a `.unwrap()` failure deep in user code) would
        // propagate past `process_batch` and abort the daemon —
        // exactly the prototypability hazard the rollback gate is
        // supposed to handle. Wrapping the post-swap render in
        // `catch_unwind` makes panics roll back the same way `Err`
        // returns do.
        //
        // Safety note on `AssertUnwindSafe`: `&mut Scheduler` is not
        // `UnwindSafe` (very few mutable refs are). The scheduler's
        // state IS observably affected by a panic mid-batch — but
        // we're about to restore every guarded definition + re-run
        // `process_batch`, so the post-rollback state converges to
        // the pre-swap shape regardless of what the panicking
        // half-batch left behind. The remaining out-of-scope
        // hazard — panics that cross an extern "C" frame in the JSC
        // / CPython FFI shim — would already abort before reaching
        // this catch boundary; M-Prod's third-party security audit
        // covers per-bridge `catch_unwind` shims around every host
        // callback registration point.
        if self.rollback_enabled && !guards.is_empty() {
            let render_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                scheduler.process_batch()
            }));
            let render_problem: Option<String> = match render_result {
                Ok(Ok(())) => None,
                Ok(Err(render_err)) => Some(format!(
                    "first-tick render failed; rolled back {} compatible swap(s): {render_err}",
                    guards.len()
                )),
                Err(panic_payload) => {
                    let payload_msg = panic_payload_message(&panic_payload);
                    Some(format!(
                        "first-tick render panicked; rolled back {} compatible swap(s): {payload_msg}",
                        guards.len()
                    ))
                }
            };
            if let Some(message) = render_problem {
                let restore_outcome = restore_guards(scheduler, &guards);
                outcome.rollback = Some(restore_outcome);
                outcome.errors.push(ApplyError::Scheduler(message));
                // Re-render with restored definitions so the
                // post-apply state is the working pre-swap shape.
                // Wrapped in `catch_unwind` defensively — if the
                // restored body ALSO panics (a real "the user broke
                // their working snapshot too" bug), we surface it
                // rather than aborting the daemon.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    scheduler.process_batch()
                }));
            }
        }

        // M16-Backend Phase 1: emit `on_hot_reload` once at the end
        // of every apply, regardless of outcome. Source-path /
        // origin-peer are not visible at this layer (the
        // graphreact-cli watch driver + the dist crate's reload sink
        // own those concerns); pass `None` and let consumers correlate
        // through tracing-target prefixes if they need source context.
        self.fire_hot_reload_telemetry(&outcome, None, None);

        outcome
    }

    /// Fire `on_hot_reload` for an apply outcome. Separate helper so
    /// future caller-aware variants (e.g., a wrapper that knows the
    /// source path or remote origin) can pass richer metadata
    /// through.
    fn fire_hot_reload_telemetry(
        &self,
        outcome: &ReloadOutcome,
        source_path: Option<&str>,
        origin_peer: Option<&str>,
    ) {
        let guard = self.telemetry_sink.load();
        let Some(sink) = guard.as_ref() else {
            return;
        };
        let nodes_replaced = outcome
            .nodes
            .iter()
            .filter(|(_, oc)| matches!(oc, NodeApplyOutcome::Replaced))
            .count();
        let nodes_remounted = outcome
            .nodes
            .iter()
            .filter(|(_, oc)| matches!(oc, NodeApplyOutcome::Remounted))
            .count();
        let edges_changed = outcome.edges.len();
        let rolled_back = outcome.was_rolled_back();
        let error_count = outcome.errors.len();
        sink.on_hot_reload(&graphreact_core::HotReload {
            source_path,
            origin_peer,
            nodes_replaced,
            nodes_remounted,
            edges_changed,
            rolled_back,
            error_count,
        });
    }

    fn apply_compat(
        &self,
        scheduler: &mut Scheduler,
        delta: &NodeDelta,
        outcome: &mut ReloadOutcome,
    ) {
        if let NodeDeltaKind::BodyChangedCompatible {
            component: _,
            new_code,
            new_lang,
        } = &delta.kind
        {
            match self
                .adapter
                .build_definition(&delta.component, new_code, *new_lang)
            {
                Ok(new_def) => match scheduler.replace_definition(&delta.node_id, new_def) {
                    Ok(()) => outcome
                        .nodes
                        .push((delta.node_id.clone(), NodeApplyOutcome::Replaced)),
                    Err(e) => {
                        outcome.errors.push(ApplyError::Scheduler(format!(
                            "replace_definition({}) failed: {e}",
                            delta.node_id
                        )));
                    }
                },
                Err(e) => outcome.errors.push(e),
            }
        }
    }

    fn apply_incompat(
        &self,
        scheduler: &mut Scheduler,
        delta: &NodeDelta,
        outcome: &mut ReloadOutcome,
    ) {
        match &delta.kind {
            NodeDeltaKind::BodyChangedIncompatible {
                component: _,
                new_code,
                new_lang,
                ..
            } => {
                self.drain_unmount_remount(
                    scheduler,
                    &delta.node_id,
                    &delta.component,
                    new_code,
                    *new_lang,
                    outcome,
                );
            }
            NodeDeltaKind::PropsChanged | NodeDeltaKind::LangChanged { .. } => {
                // Re-fetch the source body from the most recent
                // resolved IR — the orchestrator's caller holds the
                // post-edit IR and registers any required remount
                // info via `set_remount_info`. For PropsChanged /
                // LangChanged, the caller must supply the new code
                // through `remount_info` if it differs; today the
                // orchestrator can't synthesize bodies.
                if let Some(info) = self.remount_info.get(&delta.node_id) {
                    let _ = info; // placeholder until Phase 8 wires
                                  // re-mount info in caller;
                                  // until then drain+unmount only.
                }
                self.drain_unmount_only(scheduler, &delta.node_id, outcome);
            }
            NodeDeltaKind::Added => {
                outcome.errors.push(ApplyError::MissingRemountInfo {
                    node_id: delta.node_id.clone(),
                    detail: "Added nodes require caller-supplied remount info + new body source — phase 5 ships drain/unmount/replace_definition; mounting brand-new nodes during reload lands when the caller wires set_remount_info + new-body lookup.".into(),
                });
            }
            NodeDeltaKind::Removed => {
                self.drain_unmount_only(scheduler, &delta.node_id, outcome);
            }
            NodeDeltaKind::ComponentRenamed { .. } => {
                // Rename = unmount old + mount new. Today: drain +
                // unmount; mounting the renamed component requires
                // caller-supplied body source via the engine
                // adapter, same as Added. Skip with a clear error.
                self.drain_unmount_only(scheduler, &delta.node_id, outcome);
                outcome.errors.push(ApplyError::MissingRemountInfo {
                    node_id: delta.node_id.clone(),
                    detail: "ComponentRenamed unmounted; remount of the new component requires caller-supplied body source.".into(),
                });
            }
            NodeDeltaKind::EntrypointsChanged => {
                outcome.nodes.push((
                    delta.node_id.clone(),
                    NodeApplyOutcome::Skipped {
                        reason: "entrypoint contract changed; deployment restart and manifest revalidation required"
                            .into(),
                    },
                ));
            }
            NodeDeltaKind::BodyChangedCompatible { .. } => {
                // Should not be in the Incompatible bucket. Defensive
                // coverage.
                outcome.nodes.push((
                    delta.node_id.clone(),
                    NodeApplyOutcome::Skipped {
                        reason: "BodyChangedCompatible miscategorised as Incompatible".into(),
                    },
                ));
            }
        }
    }

    fn drain_unmount_remount(
        &self,
        scheduler: &mut Scheduler,
        node_id: &str,
        component: &str,
        new_code: &str,
        new_lang: ComponentLang,
        outcome: &mut ReloadOutcome,
    ) {
        // Capture existing props + connect-to neighbours BEFORE
        // unmount so we can re-mount with the same shape.
        let (props, connect_to) = match snapshot_node_for_remount(scheduler, node_id) {
            Some(snap) => snap,
            None => {
                outcome.errors.push(ApplyError::MissingRemountInfo {
                    node_id: node_id.to_string(),
                    detail: "node not registered with scheduler at apply time".into(),
                });
                return;
            }
        };

        // Phase 6 will replace this with `wait_for_quiescence`. For
        // now, unmount immediately. process_batch's drain at next
        // iteration covers in-flight messages.
        let Some(key) = scheduler.key_for(node_id) else {
            outcome.errors.push(ApplyError::Scheduler(format!(
                "node `{node_id}` not registered (gone since classification?)"
            )));
            return;
        };
        scheduler.unmount_with_reason(key, ExitReason::Shutdown);
        // **M15 Phase 9.** `unmount_with_reason` clears scheduler-side
        // state (NodeInstance, monitors, links, manifests) but does
        // NOT mutate `scheduler.graph` — that's `remove_node_at_runtime`'s
        // job. We fire `graph.remove_node` here so the
        // `TopologyEvent::NodeRemoved` event reaches engine-side
        // topology listeners (e.g. `jsruntime`'s `clear_node_local_state`
        // callback), which clears `SIGNAL_BY_HOOK_POS` plus
        // `SIGNAL_REGISTRY` plus `MEMO_REGISTRY` entries for this
        // node. Without this, the subsequent `add_node_at_runtime`
        // re-adds the node id but the new mount's first `useState`
        // reads a stale dropped `Signal` and panics with
        // `ValueDroppedError`.
        let _ = scheduler.graph.borrow_mut().remove_node(node_id);

        // Build the new definition + remount.
        let new_def = match self.adapter.build_definition(component, new_code, new_lang) {
            Ok(def) => def,
            Err(e) => {
                outcome.errors.push(e);
                return;
            }
        };
        match scheduler.add_node_at_runtime(new_def, node_id.to_string(), props, connect_to) {
            Ok(_) => outcome
                .nodes
                .push((node_id.to_string(), NodeApplyOutcome::Remounted)),
            Err(e) => outcome.errors.push(ApplyError::Scheduler(format!(
                "add_node_at_runtime failed: {e}"
            ))),
        }
    }

    fn drain_unmount_only(
        &self,
        scheduler: &mut Scheduler,
        node_id: &str,
        outcome: &mut ReloadOutcome,
    ) {
        let Some(key) = scheduler.key_for(node_id) else {
            outcome.nodes.push((
                node_id.to_string(),
                NodeApplyOutcome::Skipped {
                    reason: "node already unregistered".into(),
                },
            ));
            return;
        };
        scheduler.unmount_with_reason(key, ExitReason::Shutdown);
        outcome
            .nodes
            .push((node_id.to_string(), NodeApplyOutcome::Remounted));
    }

    fn apply_edge(
        &self,
        scheduler: &mut Scheduler,
        delta: &EdgeDelta,
        outcome: &mut ReloadOutcome,
    ) {
        match delta {
            EdgeDelta::Added {
                source,
                target,
                attrs: _,
                kind: _,
            } => {
                let mut g = scheduler.graph.borrow_mut();
                if g.has_node(source) && g.has_node(target) {
                    g.add_edge(source.clone(), target.clone(), None, Attrs::new());
                    outcome
                        .edges
                        .push((source.clone(), target.clone(), EdgeApplyOutcome::Added));
                }
            }
            EdgeDelta::Removed {
                source,
                target,
                kind: _,
            } => {
                let mut g = scheduler.graph.borrow_mut();
                let _ = g.remove_edge(source, target, None);
                outcome
                    .edges
                    .push((source.clone(), target.clone(), EdgeApplyOutcome::Removed));
            }
            EdgeDelta::AttrsChanged { source, target, .. } => {
                // The CommunicationGraph stores attrs per edge but
                // the public surface is limited; for now record the
                // intent but defer mutation to Phase 6 which will
                // expose attrs-update through scheduler.
                outcome.edges.push((
                    source.clone(),
                    target.clone(),
                    EdgeApplyOutcome::AttrsChanged,
                ));
            }
        }
    }
}

/// **M15 Phase 7 closeout.** Best-effort message extraction from a
/// `catch_unwind` payload. Returns the panic's `&str` or `String`
/// payload when present; falls back to `<non-string panic payload>`
/// for `panic!(value: T)` invocations with non-string types. Mirrors
/// the shape `std::panic::PanicHookInfo::payload_as_str` would offer
/// once stabilised.
fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

/// Phase 7 helper — restore each guard's `prior_def` via
/// `Scheduler::replace_definition`. Returns a per-call
/// [`RollbackOutcome`] so the caller can surface what restored
/// vs. what failed (e.g., a node that was removed since the
/// snapshot).
fn restore_guards(scheduler: &mut Scheduler, guards: &[RollbackGuard]) -> RollbackOutcome {
    let mut outcome = RollbackOutcome {
        restored: 0,
        failed: Vec::new(),
    };
    for guard in guards {
        match scheduler.replace_definition(&guard.node_id, guard.prior_def.clone()) {
            Ok(()) => outcome.restored += 1,
            Err(_) => outcome.failed.push(guard.node_id.clone()),
        }
    }
    outcome
}

/// Capture the existing node's outgoing-edge targets for re-mount.
/// Returns `None` if the node isn't registered with the scheduler.
///
/// Today this returns `Attrs::new()` for props; Phase 9 wires the
/// caller-supplied post-edit IR through so the new node mounts with
/// the new IR's props (which may have changed in the way that
/// triggered the Incompatible classification).
fn snapshot_node_for_remount(scheduler: &Scheduler, node_id: &str) -> Option<(Attrs, Vec<String>)> {
    scheduler.key_for(node_id)?;
    let connect_to = scheduler
        .graph
        .borrow()
        .neighbors(node_id)
        .unwrap_or_default();
    Some((Attrs::new(), connect_to))
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::runtime::node_instance::NodeDefinition;
    use graphreact_core::{Attrs, CommunicationGraph, NodeContext, PermissionManifest, Scheduler};
    use std::cell::Cell;
    use std::rc::Rc;

    /// Tracks how many times a counting closure was instantiated +
    /// how many times its NodeDefinition was invoked.
    struct CountingAdapter {
        invocations: Rc<Cell<u32>>,
    }

    impl EngineAdapter for CountingAdapter {
        fn build_definition(
            &self,
            component: &str,
            _code: &str,
            _lang: ComponentLang,
        ) -> Result<NodeDefinition, ApplyError> {
            let invocations = Rc::clone(&self.invocations);
            Ok(NodeDefinition::new(
                component.to_string(),
                move |_ctx: &NodeContext| {
                    invocations.set(invocations.get() + 1);
                    PermissionManifest::default()
                },
            ))
        }
    }

    fn empty_def(name: &'static str) -> NodeDefinition {
        NodeDefinition::new(name, |_| PermissionManifest::default())
    }

    #[test]
    fn nochange_returns_empty_outcome() {
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        let invocations = Rc::new(Cell::new(0u32));
        let mut orch = ReloadOrchestrator::new(CountingAdapter {
            invocations: Rc::clone(&invocations),
        });
        let outcome = orch.apply(&mut scheduler, ReloadDelta::NoChange);
        assert!(outcome.is_success());
        assert!(outcome.nodes.is_empty());
        assert!(outcome.edges.is_empty());
    }

    #[test]
    fn compat_delta_calls_replace_definition() {
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        // Mount one node with a stub definition.
        scheduler
            .add_node_at_runtime(empty_def("Foo"), "n", Attrs::new(), vec![])
            .unwrap();

        let invocations = Rc::new(Cell::new(0u32));
        let mut orch = ReloadOrchestrator::new(CountingAdapter {
            invocations: Rc::clone(&invocations),
        });

        let delta = ReloadDelta::Compatible(vec![NodeDelta {
            node_id: "n".into(),
            component: "Foo".into(),
            kind: NodeDeltaKind::BodyChangedCompatible {
                component: "Foo".into(),
                new_code: "/* new */".into(),
                new_lang: ComponentLang::Js,
            },
        }]);
        let outcome = orch.apply(&mut scheduler, delta);
        assert!(outcome.is_success(), "errors: {:?}", outcome.errors);
        assert_eq!(outcome.nodes.len(), 1);
        assert_eq!(outcome.nodes[0].1, NodeApplyOutcome::Replaced);
        // process_batch should have run after replace_definition's
        // mark_dirty; the new closure incremented invocations.
        scheduler.process_batch().unwrap();
        assert!(invocations.get() >= 1);
    }

    #[test]
    fn incompat_delta_drains_unmounts_remounts() {
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        scheduler
            .add_node_at_runtime(empty_def("Foo"), "n", Attrs::new(), vec![])
            .unwrap();

        let invocations = Rc::new(Cell::new(0u32));
        let mut orch = ReloadOrchestrator::new(CountingAdapter {
            invocations: Rc::clone(&invocations),
        });

        let delta = ReloadDelta::Incompatible(vec![NodeDelta {
            node_id: "n".into(),
            component: "Foo".into(),
            kind: NodeDeltaKind::BodyChangedIncompatible {
                component: "Foo".into(),
                old_code: "/* old */".into(),
                new_code: "/* new */".into(),
                new_lang: ComponentLang::Js,
            },
        }]);
        let outcome = orch.apply(&mut scheduler, delta);
        assert!(outcome.is_success(), "errors: {:?}", outcome.errors);
        assert_eq!(outcome.nodes.len(), 1);
        assert_eq!(outcome.nodes[0].1, NodeApplyOutcome::Remounted);
        // Re-mounted node should still be registered.
        assert!(scheduler.has_node("n"));
    }

    #[test]
    fn topology_only_adds_edge_to_existing_nodes() {
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        scheduler
            .add_node_at_runtime(empty_def("A"), "a", Attrs::new(), vec![])
            .unwrap();
        scheduler
            .add_node_at_runtime(empty_def("B"), "b", Attrs::new(), vec![])
            .unwrap();

        let invocations = Rc::new(Cell::new(0u32));
        let mut orch = ReloadOrchestrator::new(CountingAdapter {
            invocations: Rc::clone(&invocations),
        });
        let delta = ReloadDelta::TopologyOnly(vec![EdgeDelta::Added {
            source: "a".into(),
            target: "b".into(),
            attrs: Default::default(),
            kind: graphreact_grx_parser::ir::EdgeKind::Explicit,
        }]);
        let outcome = orch.apply(&mut scheduler, delta);
        assert!(outcome.is_success());
        assert_eq!(outcome.edges.len(), 1);
        assert_eq!(outcome.edges[0].2, EdgeApplyOutcome::Added);
        let g = scheduler.graph.borrow();
        assert!(
            g.has_edge("a", "b", None) || g.has_edge("b", "a", None),
            "expected an edge between a and b after TopologyOnly add"
        );
    }

    #[test]
    fn incompat_path_drains_pending_messages_before_swap() {
        // Phase 6 safe-point gate: any in-flight message targeting
        // the to-be-remounted node should be delivered (or
        // observably dropped via existing manifest gating)
        // BEFORE unmount_with_reason fires. We exercise the path
        // by enqueuing a message immediately before a remount and
        // asserting the orchestrator reports success (drain +
        // remount completes without scheduler errors).
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        scheduler
            .add_node_at_runtime(empty_def("Foo"), "n", Attrs::new(), vec![])
            .unwrap();

        // Push a message into the external inbox.
        scheduler.send("n", "ping", serde_json::Value::Null);

        let invocations = Rc::new(Cell::new(0u32));
        let mut orch = ReloadOrchestrator::new(CountingAdapter {
            invocations: Rc::clone(&invocations),
        });
        let delta = ReloadDelta::Incompatible(vec![NodeDelta {
            node_id: "n".into(),
            component: "Foo".into(),
            kind: NodeDeltaKind::BodyChangedIncompatible {
                component: "Foo".into(),
                old_code: "/* old */".into(),
                new_code: "/* new */".into(),
                new_lang: ComponentLang::Js,
            },
        }]);
        let outcome = orch.apply(&mut scheduler, delta);
        assert!(
            outcome.is_success(),
            "drain+remount should succeed: {:?}",
            outcome.errors
        );
        assert!(scheduler.has_node("n"), "remount should leave node alive");
    }

    #[test]
    fn rollback_disabled_skips_guard_snapshot() {
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        scheduler
            .add_node_at_runtime(empty_def("Foo"), "n", Attrs::new(), vec![])
            .unwrap();

        let invocations = Rc::new(Cell::new(0u32));
        let mut orch = ReloadOrchestrator::new(CountingAdapter {
            invocations: Rc::clone(&invocations),
        });
        orch.set_rollback_enabled(false);

        let delta = ReloadDelta::Compatible(vec![NodeDelta {
            node_id: "n".into(),
            component: "Foo".into(),
            kind: NodeDeltaKind::BodyChangedCompatible {
                component: "Foo".into(),
                new_code: "/* new */".into(),
                new_lang: ComponentLang::Js,
            },
        }]);
        let outcome = orch.apply(&mut scheduler, delta);
        assert!(outcome.is_success());
        assert!(
            outcome.rollback.is_none(),
            "rollback feature disabled — outcome should record None"
        );
    }

    #[test]
    fn rollback_fires_when_post_swap_render_errors() {
        // Force a MaxDepthExceeded during the post-swap render
        // by setting the scheduler's process-depth cap to zero
        // BEFORE applying the reload. The orchestrator's apply
        // path should detect the failure, restore the prior
        // definition, and surface a RolledBack outcome.
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        scheduler
            .add_node_at_runtime(empty_def("Foo"), "n", Attrs::new(), vec![])
            .unwrap();

        let invocations = Rc::new(Cell::new(0u32));
        let mut orch = ReloadOrchestrator::new(CountingAdapter {
            invocations: Rc::clone(&invocations),
        });

        // Cap process-batch depth to 0 so the post-swap render
        // fails with MaxDepthExceeded.
        scheduler.set_max_process_depth(0);

        let delta = ReloadDelta::Compatible(vec![NodeDelta {
            node_id: "n".into(),
            component: "Foo".into(),
            kind: NodeDeltaKind::BodyChangedCompatible {
                component: "Foo".into(),
                new_code: "/* new */".into(),
                new_lang: ComponentLang::Js,
            },
        }]);
        let outcome = orch.apply(&mut scheduler, delta);
        assert!(
            !outcome.is_success(),
            "expected post-swap render to fail with MaxDepthExceeded"
        );
        assert!(outcome.was_rolled_back(), "rollback should have fired");
        let rb = outcome.rollback.as_ref().expect("rollback outcome present");
        assert_eq!(rb.restored, 1, "one prior definition should be restored");
        assert!(rb.failed.is_empty(), "no nodes should fail to restore");
    }

    /// **M15 Phase 7 closeout.** A Rust panic in the post-swap body
    /// must trigger rollback the same way an `Err` return does. This
    /// is the load-bearing prototypability gate for hot-reload — a
    /// `.unwrap()` deep in user code (or a `Signal::get` on a
    /// dropped cell, or any other Rust-side panic) must roll back
    /// to the prior body, not abort the daemon.
    #[test]
    fn rollback_fires_when_post_swap_render_panics() {
        struct PanickingAdapter;
        impl EngineAdapter for PanickingAdapter {
            fn build_definition(
                &self,
                component: &str,
                _code: &str,
                _lang: ComponentLang,
            ) -> Result<NodeDefinition, ApplyError> {
                Ok(NodeDefinition::new(component.to_string(), |_ctx| {
                    panic!("user-body panic from PanickingAdapter");
                }))
            }
        }

        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        scheduler
            .add_node_at_runtime(empty_def("Foo"), "n", Attrs::new(), vec![])
            .unwrap();

        let mut orch = ReloadOrchestrator::new(PanickingAdapter);
        let delta = ReloadDelta::Compatible(vec![NodeDelta {
            node_id: "n".into(),
            component: "Foo".into(),
            kind: NodeDeltaKind::BodyChangedCompatible {
                component: "Foo".into(),
                new_code: "/* boom */".into(),
                new_lang: ComponentLang::Js,
            },
        }]);

        // The orchestrator must NOT propagate the panic. catch_unwind
        // inside `apply` is the load-bearing rollback gate; a missing
        // catch here would abort the test process.
        let outcome = orch.apply(&mut scheduler, delta);

        assert!(
            !outcome.is_success(),
            "panic must surface as a non-success outcome"
        );
        assert!(
            outcome.was_rolled_back(),
            "rollback should have fired on panic; outcome={outcome:?}"
        );
        let rb = outcome.rollback.as_ref().expect("rollback outcome present");
        assert_eq!(rb.restored, 1, "prior definition should be restored");
        assert!(rb.failed.is_empty(), "restoration should not fail");

        let any_panic_error = outcome
            .errors
            .iter()
            .any(|e| matches!(e, ApplyError::Scheduler(msg) if msg.contains("panicked")));
        assert!(
            any_panic_error,
            "errors should mention `panicked`: {:?}",
            outcome.errors,
        );
    }

    #[test]
    fn rollback_outcome_was_rolled_back_helper() {
        let oc = ReloadOutcome {
            nodes: vec![],
            edges: vec![],
            errors: vec![],
            rollback: None,
        };
        assert!(!oc.was_rolled_back());

        let oc = ReloadOutcome {
            nodes: vec![],
            edges: vec![],
            errors: vec![],
            rollback: Some(crate::rollback::RollbackOutcome {
                restored: 1,
                failed: vec![],
            }),
        };
        assert!(oc.was_rolled_back());
    }

    #[test]
    fn added_node_without_remount_info_records_error() {
        let mut scheduler = Scheduler::new(CommunicationGraph::new());
        scheduler.start().unwrap();
        let invocations = Rc::new(Cell::new(0u32));
        let mut orch = ReloadOrchestrator::new(CountingAdapter {
            invocations: Rc::clone(&invocations),
        });
        let delta = ReloadDelta::Incompatible(vec![NodeDelta {
            node_id: "fresh".into(),
            component: "Foo".into(),
            kind: NodeDeltaKind::Added,
        }]);
        let outcome = orch.apply(&mut scheduler, delta);
        assert!(!outcome.is_success());
        assert!(matches!(
            outcome.errors[0],
            ApplyError::MissingRemountInfo { .. }
        ));
    }
}
