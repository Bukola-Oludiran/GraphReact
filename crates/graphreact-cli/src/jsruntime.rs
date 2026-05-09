//! JSC runtime composition layer (M12.3a-phase3c).
//!
//! Where phase 3a stopped at "component bodies are re-callable
//! functions", phase 3b layered graphreact-core's [`Scheduler`] on
//! top with real `useState`. Phase 3c adds real `useEffect`: JS
//! callbacks captured via a JS-side callback registry (token
//! addressing), invoked from Rust at flush time via
//! [`graphreact_engine_jsc::JscEngine::invoke_global`].
//!
//! ## Hook routing
//!
//! When JS code in a component body calls `useState(0)`, the
//! `HOOK_PRELUDE` stub delegates to a registered host callback —
//! [`build_host_state_callback`] — which:
//! 1. Reads the current node id from [`CURRENT_NODE_ID`] (TLS set by
//!    [`ComponentEvalGuard`] at the start of each evaluation).
//! 2. Reads the JS-side hook position from [`JS_HOOK_INDEX`]
//!    (incremented each call; reset between renders).
//! 3. Calls [`graphreact_core::use_state`] which uses graphreact-core's
//!    OWN TLS (`CURRENT_NODE` / `HOOK_INDEX`, set by
//!    `lifecycle::evaluate_node`) to allocate or reuse the hook slot.
//! 4. Looks up or mints a stable u64 id for this `(node_id, hook_pos)`
//!    pair, stores the [`Signal`] in [`signal_registry`], returns
//!    `{id, value}` to JS.
//!
//! Subsequent setter calls (`host_state_set(id, value)`) look up the
//! Signal by id and call `.set(v)` — which marks the owning node
//! dirty and triggers re-evaluation on the next process_batch.
//!
//! ## Why two hook indices
//!
//! graphreact-core's `HOOK_INDEX` is private to that crate and tracks
//! Rust-side hook slots. [`JS_HOOK_INDEX`] is our parallel counter
//! that lives in this layer — it's used only to derive a stable id
//! per `(node_id, hook_pos)` so the Rust-side Signal can be retrieved
//! from JS. Both counters increment in lockstep with each
//! `core_use_state` call, so the keys stay synchronised.
//!
//! The "two indices" approach avoids upstreaming a `current_hook_index()`
//! getter to graphreact-core just for the JSC layer's bookkeeping —
//! cheaper as a layer-local mirror.

#![cfg(target_os = "macos")]
// Signal ids are minted by an in-process counter — bounded well under
// 2^53 in any realistic run, so the round-trip through JS Number
// (f64 mantissa = 53 bits) is lossless. Node depths come from the IR
// where they're already small `usize` values from `usize::MAX`-bounded
// tree traversal. The bounded casts trigger clippy's pedantic lints
// uniformly; allowing them at module scope keeps individual sites
// readable.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use serde_json::Value;

use graphreact_core::{
    delay_source as core_delay_source, timer_source as core_timer_source,
    use_accept_edge as core_use_accept_edge, use_compose as core_use_compose,
    use_demonitor as core_use_demonitor,
    use_edge_accept_handler as core_use_edge_accept_handler, use_effect as core_use_effect,
    use_effect_async as core_use_effect_async,
    use_emit as core_use_emit, use_group_join as core_use_group_join,
    use_group_leave as core_use_group_leave, use_link as core_use_link,
    use_memo as core_use_memo, use_message as core_use_message,
    use_monitor as core_use_monitor, use_neighbors as core_use_neighbors,
    use_source as core_use_source, use_spawn_child as core_use_spawn_child,
    use_state as core_use_state, use_topology as core_use_topology,
    use_unlink as core_use_unlink, use_unmount_self as core_use_unmount_self, Attrs,
    CommunicationGraph, ComposeChildSpec, EdgeAcceptPredicate, EffectCleanup, EmitOptions,
    EventSource, GraphDirectory, IncomingMessage, MonitorRef as CoreMonitorRef, NodeContext,
    NodeDefinition, PermissionManifest, Scheduler, Signal, Subscription, TopologyEvent,
    TopologyEventKind, TopologyFilterOptions,
};
use graphreact_engine_jsc::{drain_microtasks_static, invoke_global_static, JscEngine};
use graphreact_engine_zig::{
    BoxCallback, Engine, EngineError, FunctionHandle, HostValue, IsolateHandle, IsolateOptions,
};
use graphreact_grx_parser::{multi_file::resolve_multi_file, ir::IrNode, GrxFile, GrxIr, SourceLoc};

use crate::runtime::RuntimeError;

// ---------------------------------------------------------------------
// Per-thread registries
// ---------------------------------------------------------------------
//
// `Signal<Value>` contains an `Rc<dyn Fn()>` (the enqueue closure
// pinned to the owning node's scheduler) and is therefore
// `!Send + !Sync`. The scheduler is single-threaded by design (M4
// baseline), so thread-locals are the correct shape — the JSC
// callback trampoline runs on the same thread that drives
// `Scheduler::process_batch`, which is the same thread that owns the
// Signals.
//
// Multi-threaded clusters (M14) will revisit this when the scheduler
// itself becomes multi-threaded; until then, thread-local mirrors
// the runtime's own threading model.

thread_local! {
    /// Each registered Signal indexed by a stable u64 id. Ids survive
    /// re-renders — the same `(node_id, hook_pos)` always resolves to
    /// the same id, even though `core_use_state` is called fresh each
    /// render.
    static SIGNAL_REGISTRY: RefCell<FxHashMap<u64, Signal<Value>>> =
        RefCell::new(FxHashMap::default());

    /// Stable id assignment per `(node_id, hook_pos)`. First render
    /// allocates an id; subsequent renders reuse it. Together with
    /// [`SIGNAL_REGISTRY`] this lets JS-side closures (which capture
    /// an id at render time) keep working across re-renders without
    /// chasing a moving handle.
    static SIGNAL_BY_HOOK_POS: RefCell<FxHashMap<(String, usize), u64>> =
        RefCell::new(FxHashMap::default());

    /// Monotonic id source for [`SIGNAL_REGISTRY`]. Per-thread so a
    /// fresh mount on this thread doesn't reuse a stale id from a
    /// previous run.
    static NEXT_SIGNAL_ID: Cell<u64> = const { Cell::new(1) };

    /// useMemo / useCallback cached values, keyed by `(node_id,
    /// hook_pos)`. Stores `serde_json::Value` (which can hold either a
    /// real memo value or a callback token for `useCallback`'s
    /// identity-stable function pattern). Phase 3e-α (2026-05-02).
    static MEMO_REGISTRY: RefCell<FxHashMap<(String, usize), Value>> =
        RefCell::new(FxHashMap::default());

    /// "Hot" key carried between a host_memo_check and the matching
    /// host_memo_store call. host_memo_check sets it when the memo is
    /// stale (deps changed or first call); host_memo_store consumes it
    /// to know which entry to write. Set to None outside that pair —
    /// JS calls them sequentially within a single body evaluation, so
    /// there's no risk of leaking across renders.
    static PENDING_MEMO_KEY: RefCell<Option<(String, usize)>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------
// Per-evaluation TLS
// ---------------------------------------------------------------------

thread_local! {
    /// Set by [`ComponentEvalGuard`] at the start of each component
    /// evaluation; cleared on drop. Read by host callbacks (host_state,
    /// host_state_set, …) to know which node is currently rendering.
    static CURRENT_NODE_ID: RefCell<Option<String>> = const { RefCell::new(None) };

    /// JS-side hook position counter. Reset to 0 by
    /// [`ComponentEvalGuard`]; incremented by each host_state call.
    /// Mirrors graphreact-core's private HOOK_INDEX so we can derive a
    /// stable `(node_id, hook_pos)` key for [`SIGNAL_BY_HOOK_POS`].
    static JS_HOOK_INDEX: Cell<usize> = const { Cell::new(0) };

    /// Engine handle for deferred-fire host callbacks (effects,
    /// future async hooks). Set by [`EngineGuard`] around
    /// [`JsRuntime::process_batch`] so closures invoked from
    /// `flush_effects` can re-enter JSC without capturing
    /// `Rc<RefCell<JscEngine>>` directly. The `BoxCallback` type
    /// stored in engine-jsc's process-global registry is `Send+Sync`,
    /// which `Rc` violates — TLS sidesteps the constraint.
    static CURRENT_ENGINE: RefCell<Option<Rc<RefCell<JscEngine>>>> =
        const { RefCell::new(None) };

    /// Isolate handle paired with [`CURRENT_ENGINE`].
    static CURRENT_ISO: Cell<Option<IsolateHandle>> = const { Cell::new(None) };

    /// Directory handle for `host_directory_*` callbacks. Same
    /// `Send+Sync` workaround as [`CURRENT_ENGINE`] — `Rc` is `!Send`
    /// but the callback registry forces `Send+Sync`. Read at
    /// call-time, not capture-time. Phase 3e-δ.
    static CURRENT_DIRECTORY: RefCell<Option<Rc<RefCell<GraphDirectory>>>> =
        const { RefCell::new(None) };

    /// Live graph handle for `host_graph_*` callbacks. Cloned (cheap
    /// `Rc::clone`) from `Scheduler::graph` at `mount_with_scheduler`
    /// time. Reads under `borrow()` reflect the live state — dynamic
    /// `useSpawnChild` / `useCompose` / `useAcceptEdge` mutations are
    /// visible on the next `useGraphQuery` call.
    ///
    /// Refactored from "frozen snapshot" → "live handle" in
    /// M12.3a-followon-C (2026-05-02). The "snapshot" name is kept for
    /// continuity with the 3e-δ + followon-B call sites; the type is
    /// now a live view, not a copy.
    static CURRENT_GRAPH_SNAPSHOT: RefCell<Option<Rc<GraphSnapshot>>> =
        const { RefCell::new(None) };

    /// Per-node accumulator of `__hostCallbacks` token IDs minted
    /// during a body invocation. Populated by `host_track_token`,
    /// which the JS-side `__registerCallback` calls immediately after
    /// minting a fresh token. Drained on node unmount by
    /// [`release_tokens_for_node`], which dispatches a
    /// `__releaseTokens([t1, t2, ...])` invoke to the JS side.
    ///
    /// Tracking is conditional on [`CURRENT_NODE_ID`] being set — that
    /// pins the body-eval window. Tokens registered during effect-fire
    /// (CURRENT_NODE_ID unset; cleanup tokens) are NOT tracked here:
    /// `host_effect`'s closure auto-releases the cleanup token after
    /// the cleanup runs (see jsruntime.rs `host_effect`'s
    /// `EffectCleanup` block — explicit `__releaseCallback(ct)` after
    /// the invoke), so the cleanup-fire path doesn't leak.
    ///
    /// M12.3a-followon-D.
    static NODE_TOKENS: RefCell<FxHashMap<String, Vec<u64>>> =
        RefCell::new(FxHashMap::default());

    /// M12.3b-phase1 (useEffectAsync on macOS — JS Promise → Rust Future
    /// bridge). When `host_effect_async`'s registered closure fires (from
    /// `flush_effects_async`), it allocates a fresh completion id, parks
    /// a `tokio::sync::oneshot::Sender` here keyed by that id, then
    /// invokes the JS body which schedules its async work. Once the
    /// JS-side promise chain settles, it calls
    /// `host_effect_async_complete(completionId, kind, payload)`; that
    /// callback removes the Sender from this map and signals it.
    ///
    /// `oneshot::Sender<EffectAsyncSettlement>` is `Send + Sync` (T is
    /// Send), so the TLS RefCell store is sound. Lookups happen on the
    /// same thread that registered (the local-set thread driving
    /// `process_batch_async`) — no cross-thread access.
    static EFFECT_ASYNC_COMPLETIONS: RefCell<FxHashMap<u64, tokio::sync::oneshot::Sender<EffectAsyncSettlement>>> =
        RefCell::new(FxHashMap::default());

    /// Monotonic id source for [`EFFECT_ASYNC_COMPLETIONS`]. M12.3b-phase1.
    static NEXT_EFFECT_ASYNC_COMPLETION_ID: Cell<u64> = const { Cell::new(1) };
}

/// Settlement envelope sent from `host_effect_async_complete` (JS side
/// has resolved or rejected the user's async function) to the parked
/// future inside `host_effect_async`'s closure. M12.3b-phase1.
#[derive(Debug)]
pub(crate) enum EffectAsyncSettlement {
    /// Async fn resolved with a non-function value — no cleanup to run.
    Value,
    /// Async fn resolved with a function — that's the cleanup callback.
    /// The u64 is the JS-side `__hostCallbacks` token for it.
    Cleanup(u64),
    /// Async fn rejected; the String is the stringified error.
    Throw(String),
}

fn next_effect_async_completion_id() -> u64 {
    NEXT_EFFECT_ASYNC_COMPLETION_ID.with(|c| {
        let id = c.get();
        c.set(id.wrapping_add(1).max(1));
        id
    })
}

/// Live read-only handle to the scheduler's `CommunicationGraph` plus
/// the per-node `PermissionManifest` index, installed in
/// [`CURRENT_GRAPH_SNAPSHOT`] for the duration of a `process_batch`.
/// Consulted by the 11 `host_graph_*` callbacks that back the Tier 1
/// surface of `useGraphQuery`'s `__graphQueryProxy`, and by the
/// 9 `host_tier2_*` callbacks (M12.3a-followon-E) that back the Tier 2
/// policy-derived reachability surface.
///
/// As of M12.3a-followon-C (2026-05-02) the graph is an
/// `Rc<RefCell<_>>` clone of `Scheduler::graph` (NOT a rebuilt copy).
/// As of M12.3a-followon-E (2026-05-02) the manifests is an
/// `Rc<RefCell<_>>` clone of `Scheduler::manifests` (NOT a snapshotted
/// copy). Reads happen via `borrow()` at call time, so dynamic
/// mutations (`useSpawnChild` / `useCompose` / `useAcceptEdge`) and
/// re-render-driven manifest updates land in subsequent
/// `useGraphQuery` invocations rather than waiting for a remount.
/// The Rcs are `!Send`, matching the rest of the runtime's
/// TLS-installed handles.
pub struct GraphSnapshot {
    pub graph: Rc<RefCell<CommunicationGraph>>,
    /// Per-node permission manifests, keyed by node id. Source of
    /// truth for Tier 2 reachability — direction emerges per message
    /// type from `manifest.emits/accepts` pairs. M12.3a-followon-E.
    pub manifests: Rc<RefCell<FxHashMap<String, PermissionManifest>>>,
    /// Per-node edge-accept predicates, keyed by node id. Source of
    /// truth for Tier 3 negotiation-aware planning — Tier 3 = Tier 2 ∪
    /// {(s, u) where `predicates[u](s, msgType)` returns true}. Nodes
    /// without an entry inherit the default-accept behavior, so the
    /// Tier 3 view degrades gracefully to Tier 2 when no node
    /// explicitly declines. M12.3a-followon-F.
    pub edge_accept_predicates: Rc<RefCell<FxHashMap<String, EdgeAcceptPredicate>>>,
}

impl GraphSnapshot {
    /// Construct a live snapshot from scheduler handles. All three Rcs
    /// are cloned (cheap), so the snapshot observes runtime mutations
    /// applied through the scheduler. M12.3a-followon-F widens the
    /// signature to include edge-accept predicates for Tier 3 queries.
    pub fn from_live(
        graph: Rc<RefCell<CommunicationGraph>>,
        manifests: Rc<RefCell<FxHashMap<String, PermissionManifest>>>,
        edge_accept_predicates: Rc<RefCell<FxHashMap<String, EdgeAcceptPredicate>>>,
    ) -> Self {
        Self {
            graph,
            manifests,
            edge_accept_predicates,
        }
    }
}

/// RAII guard that scopes a single component evaluation. Resets
/// [`JS_HOOK_INDEX`] and stamps [`CURRENT_NODE_ID`] on construction;
/// clears both on drop. Panics + early returns are safe.
pub(crate) struct ComponentEvalGuard;

impl ComponentEvalGuard {
    pub fn new(node_id: &str) -> Self {
        JS_HOOK_INDEX.with(|i| i.set(0));
        CURRENT_NODE_ID.with(|n| *n.borrow_mut() = Some(node_id.to_string()));
        Self
    }
}

impl Drop for ComponentEvalGuard {
    fn drop(&mut self) {
        JS_HOOK_INDEX.with(|i| i.set(0));
        CURRENT_NODE_ID.with(|n| *n.borrow_mut() = None);
    }
}

fn current_node_id() -> Option<String> {
    CURRENT_NODE_ID.with(|n| n.borrow().clone())
}

/// RAII guard scoping the engine + isolate + directory + graph TLS
/// across an entire [`JsRuntime::process_batch`] (or test) call.
/// Deferred-fire host callbacks (effects, message handlers,
/// directory accessors, graph accessors) need TLS access — not `Rc`
/// capture — to satisfy the `Send+Sync` bound on engine-jsc's
/// callback registry.
///
/// `directory` and `graph_snapshot` are optional (phase 3e-δ
/// additions): callers that don't need them can pass `None`. Tests
/// that only exercise pre-3e-δ hooks (state/effect/emit/message/
/// memo/neighbors/etc.) construct `EngineGuard::new(...)` without
/// the new args; phase 3e-δ tests use [`EngineGuard::with_directory`].
pub(crate) struct EngineGuard;

impl EngineGuard {
    /// Engine + iso only. Used by tests that don't exercise the
    /// 3e-δ directory / graph-query proxy surface; equivalent to
    /// `with_directory(..., default(), default())`.
    #[cfg(test)]
    pub fn new(engine: Rc<RefCell<JscEngine>>, iso: IsolateHandle) -> Self {
        CURRENT_ENGINE.with(|cell| *cell.borrow_mut() = Some(engine));
        CURRENT_ISO.with(|cell| cell.set(Some(iso)));
        // Leave directory + graph TLS slots cleared — host_directory_*
        // and host_graph_* callbacks safely return defaults when TLS
        // isn't installed.
        Self
    }

    /// Phase 3e-δ variant: also installs a directory handle and graph
    /// snapshot for the duration of the guard.
    pub fn with_directory(
        engine: Rc<RefCell<JscEngine>>,
        iso: IsolateHandle,
        directory: Rc<RefCell<GraphDirectory>>,
        graph_snapshot: Rc<GraphSnapshot>,
    ) -> Self {
        CURRENT_ENGINE.with(|cell| *cell.borrow_mut() = Some(engine));
        CURRENT_ISO.with(|cell| cell.set(Some(iso)));
        CURRENT_DIRECTORY.with(|cell| *cell.borrow_mut() = Some(directory));
        CURRENT_GRAPH_SNAPSHOT.with(|cell| *cell.borrow_mut() = Some(graph_snapshot));
        Self
    }
}

impl Drop for EngineGuard {
    fn drop(&mut self) {
        CURRENT_ENGINE.with(|cell| *cell.borrow_mut() = None);
        CURRENT_ISO.with(|cell| cell.set(None));
        CURRENT_DIRECTORY.with(|cell| *cell.borrow_mut() = None);
        CURRENT_GRAPH_SNAPSHOT.with(|cell| *cell.borrow_mut() = None);
    }
}

fn current_directory() -> Option<Rc<RefCell<GraphDirectory>>> {
    CURRENT_DIRECTORY.with(|cell| cell.borrow().clone())
}

fn current_graph_snapshot() -> Option<Rc<GraphSnapshot>> {
    CURRENT_GRAPH_SNAPSHOT.with(|cell| cell.borrow().clone())
}

fn current_engine() -> Option<Rc<RefCell<JscEngine>>> {
    CURRENT_ENGINE.with(|cell| cell.borrow().clone())
}

fn current_iso() -> Option<IsolateHandle> {
    CURRENT_ISO.with(Cell::get)
}

fn next_js_hook_index() -> usize {
    JS_HOOK_INDEX.with(|i| {
        let p = i.get();
        i.set(p + 1);
        p
    })
}

/// Drain the per-node token bucket (M12.3a-followon-D). Called by the
/// runtime's unmount-sweep path once the scheduler reports a node was
/// removed. Returns the tokens that were registered during the node's
/// lifetime so the caller can dispatch a single `__releaseTokens`
/// invoke to the JS side.
fn drain_node_tokens(node_id: &str) -> Vec<u64> {
    NODE_TOKENS.with(|m| m.borrow_mut().remove(node_id).unwrap_or_default())
}

/// Total tokens currently tracked across all nodes — diagnostic + test
/// helper. The number tracks the JS-side `__hostCallbacks` length up
/// to a small floor of pre-existing prelude entries (none today since
/// the prelude only registers entries through `__registerCallback` on
/// component-author-driven hook calls).
#[cfg(test)]
fn tracked_token_count() -> usize {
    NODE_TOKENS.with(|m| m.borrow().values().map(Vec::len).sum())
}

/// Release the JS-side `__hostCallbacks` entries for every token
/// registered during the lifetime of `node_id`. No-op if the node had
/// no tracked tokens. Engine borrow held only for the duration of the
/// `__releaseTokens` invoke; safe to call between scheduler ticks.
///
/// Errors from the underlying `invoke_global` are surfaced via
/// `eprintln!` rather than thrown — the sweep is best-effort cleanup
/// and shouldn't fail the caller.
fn release_tokens_for_node(
    engine: &Rc<RefCell<JscEngine>>,
    iso: IsolateHandle,
    node_id: &str,
) {
    let tokens = drain_node_tokens(node_id);
    if tokens.is_empty() {
        return;
    }
    let arr: Vec<HostValue> = tokens
        .into_iter()
        .map(|t| HostValue::Number(t as f64))
        .collect();
    if let Err(e) = engine.borrow_mut().invoke_global(
        iso,
        "__releaseTokens",
        &[HostValue::Array(arr)],
    ) {
        eprintln!(
            "[graphreact-cli] __releaseTokens dispatch failed for node {node_id}: {e}"
        );
    }
}

// ---------------------------------------------------------------------
// Host callback factories
// ---------------------------------------------------------------------

/// Build the `host_track_token(token)` callback. The JS-side
/// `__registerCallback` invokes this immediately after minting a fresh
/// token and assigning it into `__hostCallbacks`. The token gets
/// recorded under [`CURRENT_NODE_ID`] (the active body's node) in
/// [`NODE_TOKENS`]; on node unmount [`release_tokens_for_node`]
/// dispatches `__releaseTokens([t1, ...])` to delete those entries
/// from the JS-side map.
///
/// No-op when CURRENT_NODE_ID isn't set — that pins the body-eval
/// window. Tokens minted during effect-fire (cleanup tokens) are
/// auto-released by `host_effect`'s closure after the cleanup runs;
/// they don't need per-node tracking. M12.3a-followon-D.
pub(crate) fn build_host_track_token_callback() -> BoxCallback {
    Box::new(|args| {
        let token = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            _ => return Ok(HostValue::Null),
        };
        let Some(node_id) = current_node_id() else {
            return Ok(HostValue::Null);
        };
        NODE_TOKENS.with(|m| {
            m.borrow_mut().entry(node_id).or_default().push(token);
        });
        Ok(HostValue::Null)
    })
}

/// Stub `host_track_token` for callers that don't drive token GC
/// (legacy dry-mount paths). Silently no-ops; the JS-side
/// `__registerCallback` short-circuits when the host callback is
/// missing anyway, but exposing a stub keeps the callback registry
/// shape uniform.
pub(crate) fn build_host_track_token_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Null))
}

/// Build the `host_state(initial) → {id, value}` callback. JS-side
/// `useState` delegates to this; the closure runs on every render.
///
/// First render at a given hook position: allocates a fresh
/// `Signal<Value>` via `core_use_state`, mints a stable id, stores
/// the signal in [`SIGNAL_REGISTRY`], maps `(node_id, hook_pos) → id`
/// in [`SIGNAL_BY_HOOK_POS`], returns `{id, value: initial}`.
///
/// Subsequent renders: `core_use_state` returns the same `Signal`
/// (state preserved by graphreact-core's hook slots); we look up the
/// id from `SIGNAL_BY_HOOK_POS` and return `{id, value: current}`.
pub(crate) fn build_host_state_callback() -> BoxCallback {
    Box::new(|args| {
        let initial: Value = args
            .first()
            .cloned()
            .map(HostValue::into_json)
            .unwrap_or(Value::Null);

        let node_id = current_node_id()
            .ok_or_else(|| EngineError::Internal("host_state: no active node".into()))?;
        let hook_pos = next_js_hook_index();

        // Always advance graphreact-core's TLS HOOK_INDEX too. The
        // returned signal is canonical (preserved across renders
        // automatically by core's hook slot machinery).
        let signal = core_use_state(initial.clone());

        let key = (node_id, hook_pos);
        let id = SIGNAL_BY_HOOK_POS.with(|map| {
            if let Some(&id) = map.borrow().get(&key) {
                return id;
            }
            let new_id = NEXT_SIGNAL_ID.with(|c| {
                let id = c.get();
                c.set(id + 1);
                id
            });
            SIGNAL_REGISTRY.with(|reg| reg.borrow_mut().insert(new_id, signal));
            map.borrow_mut().insert(key, new_id);
            new_id
        });

        let current_value = SIGNAL_REGISTRY
            .with(|reg| reg.borrow().get(&id).map(Signal::get))
            .unwrap_or(initial);

        Ok(HostValue::Object(vec![
            ("id".into(), HostValue::Number(id as f64)),
            ("value".into(), HostValue::from_json(current_value)),
        ]))
    })
}

/// Build the `host_state_set(id, value)` callback. JS-side setter
/// closures route through this; looking up the Signal by id and
/// calling `.set(v)` marks the owning node dirty. The next
/// `Scheduler::process_batch` re-evaluates it.
pub(crate) fn build_host_state_set_callback() -> BoxCallback {
    Box::new(|args| {
        let id = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_state_set: expected Number id, got {other:?}"
                )))
            }
        };
        let value: Value = args
            .get(1)
            .cloned()
            .map(HostValue::into_json)
            .unwrap_or(Value::Null);

        SIGNAL_REGISTRY.with(|reg| {
            let registry = reg.borrow();
            let Some(signal) = registry.get(&id) else {
                return Err(EngineError::Internal(format!(
                    "host_state_set: no signal registered for id {id}"
                )));
            };
            signal.set(value);
            Ok(HostValue::Null)
        })
    })
}

/// Build the `host_effect(token, deps)` callback. JS-side
/// `useEffect` registers the callback function in the JS-side
/// `__hostCallbacks` registry and passes the token here. The
/// callback is invoked AFTER the body returns (via graphreact-core's
/// `flush_effects`), so re-entry into JSC is safe — the engine
/// borrow held during `call_function` has been released by then.
///
/// On invocation, the inner closure calls `__invokeCallback(token,
/// [])` via [`JscEngine::invoke_global`]. The JS-side wrapper
/// returns an envelope `{kind, value?, token?}` — when the user's
/// effect callback returns a function (the conventional cleanup
/// shape), `__invokeCallback` registers that function under a fresh
/// token and returns `{kind: 'cleanup', token: ct}`. We translate
/// that into an [`EffectCleanup`] closure that re-invokes
/// `__invokeCallback(ct)` when it fires.
pub(crate) fn build_host_effect_callback() -> BoxCallback {
    Box::new(|args| {
        let token = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_effect: expected Number token, got {other:?}"
                )))
            }
        };
        let deps: Vec<Value> = match args.get(1) {
            Some(HostValue::Array(arr)) => arr.iter().cloned().map(HostValue::into_json).collect(),
            None | Some(HostValue::Null) => Vec::new(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_effect: expected Array deps, got {other:?}"
                )))
            }
        };

        // Read engine + iso from TLS once at host_effect-call time;
        // capture into the deferred-fire closure. core_use_effect's
        // F bound is Fn + 'static (no Send+Sync), so capturing
        // !Send Rc is fine — the BoxCallback we're inside is
        // Send+Sync, but it doesn't capture engine; it just reads
        // TLS each call.
        let engine = current_engine().ok_or_else(|| {
            EngineError::Internal("host_effect: engine TLS unset".into())
        })?;
        let iso = current_iso().ok_or_else(|| {
            EngineError::Internal("host_effect: iso TLS unset".into())
        })?;

        core_use_effect(
            move || -> Option<EffectCleanup> {
                let result = engine.borrow_mut().invoke_global(
                    iso,
                    "__invokeCallback",
                    &[
                        HostValue::Number(token as f64),
                        HostValue::Array(vec![]),
                    ],
                );
                let envelope = match result {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[graphreact-cli] useEffect invocation failed: {e}");
                        return None;
                    }
                };
                let HostValue::Object(pairs) = envelope else {
                    return None;
                };
                let mut kind: Option<String> = None;
                let mut cleanup_token: Option<u64> = None;
                let mut throw_message: Option<String> = None;
                for (k, v) in &pairs {
                    match (k.as_str(), v) {
                        ("kind", HostValue::String(s)) => kind = Some(s.clone()),
                        ("token", HostValue::Number(t)) => cleanup_token = Some(*t as u64),
                        ("message", HostValue::String(s)) => {
                            throw_message = Some(s.clone());
                        }
                        _ => {}
                    }
                }
                let kind_str = kind.as_deref().unwrap_or("");
                if kind_str == "throw" {
                    eprintln!(
                        "[graphreact-cli] useEffect callback threw: {}",
                        throw_message.unwrap_or_default()
                    );
                    return None;
                }
                if kind_str != "cleanup" {
                    return None;
                }
                let ct = cleanup_token?;
                let engine_for_cleanup = Rc::clone(&engine);
                let iso_for_cleanup = iso;
                let cleanup: EffectCleanup = Box::new(move || {
                    let _ = engine_for_cleanup.borrow_mut().invoke_global(
                        iso_for_cleanup,
                        "__invokeCallback",
                        &[
                            HostValue::Number(ct as f64),
                            HostValue::Array(vec![]),
                        ],
                    );
                    // Release the JS-side cleanup closure now that
                    // it's run (FnOnce semantics on the Rust side
                    // mirror "cleanup runs once").
                    let _ = engine_for_cleanup.borrow_mut().invoke_global(
                        iso_for_cleanup,
                        "__releaseCallback",
                        &[HostValue::Number(ct as f64)],
                    );
                });
                Some(cleanup)
            },
            deps,
        );
        Ok(HostValue::Null)
    })
}

/// Stub `host_effect` callback for callers that don't drive a
/// [`Scheduler`]. JS-side `useEffect` short-circuits when
/// `host_effect` is undefined, so this stub exists primarily to
/// keep dry-mount paths from leaving JS-side callbacks dangling in
/// `__hostCallbacks`.
pub(crate) fn build_host_effect_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Build the `host_effect_async(token, deps)` callback. **M12.3b-phase1.**
/// Mirrors `host_effect` but routes through `core_use_effect_async`. The
/// JS-side `useEffectAsync(asyncFn, deps)` registers `asyncFn` under a
/// token (just like `useEffect`); we install an async-effect closure
/// that, when fired by `flush_effects_async`:
///
/// 1. Allocates a fresh completion id and parks a one-shot `Sender` in
///    [`EFFECT_ASYNC_COMPLETIONS`] under that id.
/// 2. Invokes the JS body (`__invokeCallback(token, [completionId])`)
///    via [`invoke_global_static`]. The body wraps the user's `asyncFn`
///    in `Promise.resolve().then(...)` and on settle calls
///    `host_effect_async_complete(completionId, kind, payload)`.
/// 3. Awaits the one-shot receiver. The JS resolution path settles
///    the channel from the same thread (TLS-driven; the JS body's
///    promise chain runs synchronously when no genuine async I/O is
///    involved, courtesy of JSC's auto-drain at function-call boundary).
///    For genuine async I/O (real `setTimeout`, `fetch`, etc.) the
///    drain has to be triggered by some other code path; that's the
///    M12.3b-phase4 deliverable (manual microtask drain via vendored
///    WebKit's C++ shim).
/// 4. Translates `EffectAsyncSettlement` into an [`AsyncEffectCleanup`]
///    (or `None` for value/throw) and returns it to graphreact-core's
///    `flush_effects_async`.
///
/// Re-entrancy: the closure runs from `flush_effects_async`, which is
/// driven by `Scheduler::process_batch_async` — outside any active
/// engine borrow. `invoke_global_static` is used (not
/// `engine.borrow_mut().invoke_global`) for symmetry with the followon-F
/// predicate path; either would work here.
pub(crate) fn build_host_effect_async_callback() -> BoxCallback {
    Box::new(|args| {
        let token = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_effect_async: expected Number token, got {other:?}"
                )))
            }
        };
        let deps: Vec<Value> = match args.get(1) {
            Some(HostValue::Array(arr)) => arr.iter().cloned().map(HostValue::into_json).collect(),
            None | Some(HostValue::Null) => Vec::new(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_effect_async: expected Array deps, got {other:?}"
                )))
            }
        };

        let iso = current_iso().ok_or_else(|| {
            EngineError::Internal("host_effect_async: iso TLS unset".into())
        })?;

        core_use_effect_async(
            move || {
                async move {
                    let completion_id = next_effect_async_completion_id();
                    let (tx, rx) =
                        tokio::sync::oneshot::channel::<EffectAsyncSettlement>();
                    EFFECT_ASYNC_COMPLETIONS.with(|cell| {
                        cell.borrow_mut().insert(completion_id, tx);
                    });

                    // Fire the JS body. It schedules a then-chain that
                    // eventually calls host_effect_async_complete; that
                    // callback signals the oneshot Sender we just parked.
                    if let Err(e) = invoke_global_static(
                        iso,
                        "__invokeCallback",
                        &[
                            HostValue::Number(token as f64),
                            HostValue::Array(vec![HostValue::Number(completion_id as f64)]),
                        ],
                    ) {
                        eprintln!(
                            "[graphreact-cli] useEffectAsync invocation failed: {e}"
                        );
                        // Reap the parked Sender; nothing's going to
                        // signal it now.
                        EFFECT_ASYNC_COMPLETIONS.with(|cell| {
                            cell.borrow_mut().remove(&completion_id);
                        });
                        return None;
                    }

                    // Await settlement. The oneshot was already inserted
                    // pre-invocation, so any synchronous JSC promise
                    // resolution that signaled before this `.await` is
                    // already buffered in the channel.
                    //
                    // **M12.3b-phase1.5.** For genuine async I/O —
                    // where Rust tokio tasks invoke
                    // `invoke_global_static` to resolve a JS Promise
                    // mid-await — JSC's auto-drain at the resolver's
                    // call boundary already handles continuation
                    // dispatch in most cases. The `select!` loop here
                    // adds a periodic manual drain as a safety net for
                    // any microtask the auto-drain missed (e.g.,
                    // queued from a path that doesn't re-enter JSC,
                    // or queued by JSC's own internal resolution
                    // machinery between auto-drains). The drain is a
                    // no-op when the queue is empty, so it's cheap.
                    let mut rx = rx;
                    let settle = loop {
                        tokio::select! {
                            biased;
                            result = &mut rx => {
                                match result {
                                    Ok(s) => break s,
                                    Err(_) => {
                                        // Sender dropped without
                                        // sending — JS body never
                                        // settled. Fall back to no
                                        // cleanup.
                                        return None;
                                    }
                                }
                            }
                            () = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                                let _ = drain_microtasks_static(iso);
                            }
                        }
                    };

                    match settle {
                        EffectAsyncSettlement::Cleanup(ct) => {
                            let cleanup: graphreact_core::AsyncEffectCleanup =
                                Box::new(move || {
                                    Box::pin(async move {
                                        let _ = invoke_global_static(
                                            iso,
                                            "__invokeCallback",
                                            &[
                                                HostValue::Number(ct as f64),
                                                HostValue::Array(vec![]),
                                            ],
                                        );
                                        let _ = invoke_global_static(
                                            iso,
                                            "__releaseCallback",
                                            &[HostValue::Number(ct as f64)],
                                        );
                                    })
                                });
                            Some(cleanup)
                        }
                        EffectAsyncSettlement::Throw(msg) => {
                            eprintln!(
                                "[graphreact-cli] useEffectAsync rejected: {msg}"
                            );
                            None
                        }
                        EffectAsyncSettlement::Value => None,
                    }
                }
            },
            deps,
        );
        Ok(HostValue::Null)
    })
}

/// Stub `host_effect_async` for callers that don't drive a `Scheduler`.
pub(crate) fn build_host_effect_async_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Build the `host_effect_async_complete(completionId, kind, payload)`
/// callback. **M12.3b-phase1.** Looks up the parked one-shot Sender for
/// `completionId` in [`EFFECT_ASYNC_COMPLETIONS`], removes it, and
/// signals it with the appropriate [`EffectAsyncSettlement`].
///
/// Args:
/// - `completionId: Number` — the id assigned by the parked future.
/// - `kind: String` — one of `'value'`, `'cleanup'`, `'throw'`.
/// - `payload: Number | String | Null` — for `'cleanup'`: the cleanup
///   token; for `'throw'`: the stringified error message; otherwise
///   ignored.
///
/// If the Sender is missing (already consumed, or never registered for
/// some race), the call silently no-ops — same defensive shape as
/// `host_track_token`.
pub(crate) fn build_host_effect_async_complete_callback() -> BoxCallback {
    Box::new(|args| {
        let completion_id = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            _ => return Ok(HostValue::Null),
        };
        let kind = match args.get(1) {
            Some(HostValue::String(s)) => s.as_str(),
            _ => return Ok(HostValue::Null),
        };

        let settlement = match kind {
            "value" => EffectAsyncSettlement::Value,
            "cleanup" => match args.get(2) {
                Some(HostValue::Number(n)) => EffectAsyncSettlement::Cleanup(*n as u64),
                _ => EffectAsyncSettlement::Value,
            },
            "throw" => match args.get(2) {
                Some(HostValue::String(s)) => EffectAsyncSettlement::Throw(s.clone()),
                _ => EffectAsyncSettlement::Throw(String::new()),
            },
            _ => return Ok(HostValue::Null),
        };

        let sender = EFFECT_ASYNC_COMPLETIONS.with(|cell| cell.borrow_mut().remove(&completion_id));
        if let Some(tx) = sender {
            // If the Receiver has been dropped (the future was
            // cancelled mid-await), `send` returns Err — silently
            // ignore.
            let _ = tx.send(settlement);
        }
        Ok(HostValue::Null)
    })
}

/// Stub `host_effect_async_complete` for the dry-mount path.
pub(crate) fn build_host_effect_async_complete_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Build the `host_emit(type, payload, opts)` callback. JS-side
/// `useEmit()` returns a closure that forwards to this; calls land
/// during body execution OR from inside a `useEffect` callback. Both
/// contexts have HOOK_CTX populated for the active node — the
/// scheduler installs it for body+`flush_effects` (lifecycle.rs:33–38)
/// and re-installs it for message-handler invocation (scheduler.rs:1261).
/// `core_use_emit()` reads HOOK_CTX to resolve the per-node emit
/// closure, so the dispatch always credits the right `from`.
///
/// Re-entrancy: this callback never touches `JscEngine` — it only
/// pushes onto the scheduler's message queue via the closure returned
/// from `core_use_emit()`. Safe to fire while an outer engine borrow
/// is held (e.g., when called from inside a body's `call_function`).
pub(crate) fn build_host_emit_callback() -> BoxCallback {
    Box::new(|args| {
        let r#type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_emit: expected String type, got {other:?}"
                )))
            }
        };
        let payload: Value = args
            .get(1)
            .cloned()
            .map(HostValue::into_json)
            .unwrap_or(Value::Null);
        let opts = parse_emit_options(args.get(2));
        let emit_fn = core_use_emit();
        emit_fn(&r#type, payload, opts);
        Ok(HostValue::Null)
    })
}

/// Stub `host_emit` callback for the dry-mount path.
pub(crate) fn build_host_emit_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Build the `host_message(type, token)` callback. JS-side
/// `useMessage(type, fn)` registers `fn` in the JS-side
/// `__hostCallbacks` token registry (mirroring useEffect) and forwards
/// the token here. We translate that into a Rust closure registered
/// via `core_use_message`; on every delivery of a message of the given
/// type to this node, the closure invokes
/// `__invokeCallback(token, [{from, type, payload, ttl}])` via
/// [`JscEngine::invoke_global`].
///
/// The marshalled message shape mirrors NAPI's
/// `crates/graphreact-napi/src/hooks.rs::invoke_message_handler` so
/// the JS surface is identical across engines.
///
/// Re-entrancy: handlers fire from `lifecycle::deliver`
/// (scheduler.rs:1262), which is well outside the body's
/// `call_function`. No engine borrow is held when the closure runs,
/// so `engine.borrow_mut().invoke_global(...)` is safe. The closure
/// captures the engine `Rc` and isolate handle directly (read out of
/// TLS once at registration time) — deferred TLS reads aren't needed
/// the way `host_effect` needs them.
pub(crate) fn build_host_message_callback() -> BoxCallback {
    Box::new(|args| {
        let r#type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_message: expected String type, got {other:?}"
                )))
            }
        };
        let token = match args.get(1) {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_message: expected Number token, got {other:?}"
                )))
            }
        };

        let engine = current_engine().ok_or_else(|| {
            EngineError::Internal("host_message: engine TLS unset".into())
        })?;
        let iso = current_iso().ok_or_else(|| {
            EngineError::Internal("host_message: iso TLS unset".into())
        })?;

        core_use_message(r#type, move |msg: &IncomingMessage| {
            let host_msg = HostValue::Object(vec![
                ("from".into(), HostValue::String(msg.from.clone())),
                ("type".into(), HostValue::String(msg.r#type.clone())),
                ("payload".into(), HostValue::from_json(msg.payload.clone())),
                ("ttl".into(), HostValue::Number(f64::from(msg.ttl))),
            ]);
            let result = engine.borrow_mut().invoke_global(
                iso,
                "__invokeCallback",
                &[
                    HostValue::Number(token as f64),
                    HostValue::Array(vec![host_msg]),
                ],
            );
            if let Err(e) = result {
                eprintln!(
                    "[graphreact-cli] useMessage handler invocation failed: {e}"
                );
            }
        });
        Ok(HostValue::Null)
    })
}

/// Stub `host_message` callback for the dry-mount path.
pub(crate) fn build_host_message_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Build the `host_memo_check(deps) → envelope` callback. JS-side
/// `useMemo` / `useCallback` call this first to learn whether the
/// cached value is still valid:
///
/// - `{cached: true, value}` — deps match a prior render at this hook
///   position; JS uses the value and skips the factory call.
/// - `{cached: false}` (optionally with `previous: <prior_value>`) —
///   deps changed (or first call); JS computes the new value and
///   dispatches `host_memo_store(new_value)` to update the cache.
///
/// Implementation note: graphreact-core's `core_use_memo` advances
/// graphreact-core's private `HOOK_INDEX` AND tells us whether the
/// factory ran (deps changed) — we can't observe the latter through
/// the public API directly, so we use a side-effect flag set inside
/// the factory closure. The flag closure runs synchronously inside
/// `core_use_memo` and never touches JSC, so it's safe even though
/// the outer body's `engine.borrow_mut().call_function` is on the
/// stack.
///
/// The actual cached value lives in [`MEMO_REGISTRY`] at the jsruntime
/// layer (keyed by `(node_id, hook_pos)`) — `core_use_memo`'s slot
/// stores `Value::Null` as a placeholder. Splitting the cache this
/// way avoids the round-trip through core's `Box<dyn Any>` (which
/// downcasts to a fixed `T: Clone + 'static`) for what is effectively
/// arbitrary JSON.
pub(crate) fn build_host_memo_check_callback() -> BoxCallback {
    Box::new(|args| {
        let deps: Vec<Value> = match args.first() {
            Some(HostValue::Array(arr)) => arr.iter().cloned().map(HostValue::into_json).collect(),
            None | Some(HostValue::Null) => Vec::new(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_memo_check: expected Array deps, got {other:?}"
                )))
            }
        };
        let node_id = current_node_id().ok_or_else(|| {
            EngineError::Internal("host_memo_check: no active node".into())
        })?;
        let hook_pos = next_js_hook_index();
        let key = (node_id, hook_pos);

        // Sync graphreact-core's HOOK_INDEX and detect whether deps
        // changed via a side-effect flag. The factory's only purpose
        // is to flip the flag — its return value (`Value::Null`) is
        // never read; the real value lives in `MEMO_REGISTRY`.
        let computed_flag = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag_inner = std::rc::Rc::clone(&computed_flag);
        let _: Value = core_use_memo(
            move || {
                flag_inner.set(true);
                Value::Null
            },
            deps,
        );
        let needs_recompute = computed_flag.get();

        if needs_recompute {
            // Stash the previous cached value (if any) so JS can
            // release stale tokens (used by useCallback). Then mark
            // the slot as pending-store.
            let previous = MEMO_REGISTRY.with(|reg| reg.borrow().get(&key).cloned());
            PENDING_MEMO_KEY.with(|p| *p.borrow_mut() = Some(key));
            let mut envelope = vec![("cached".to_string(), HostValue::Bool(false))];
            if let Some(prev) = previous {
                envelope.push(("previous".to_string(), HostValue::from_json(prev)));
            }
            return Ok(HostValue::Object(envelope));
        }

        // Deps unchanged: hand back the cached value. If the registry
        // has no entry (shouldn't happen if check/store pairing is
        // intact), fall through to recompute as a safety measure.
        let cached = MEMO_REGISTRY.with(|reg| reg.borrow().get(&key).cloned());
        match cached {
            Some(v) => Ok(HostValue::Object(vec![
                ("cached".to_string(), HostValue::Bool(true)),
                ("value".to_string(), HostValue::from_json(v)),
            ])),
            None => {
                PENDING_MEMO_KEY.with(|p| *p.borrow_mut() = Some(key));
                Ok(HostValue::Object(vec![(
                    "cached".to_string(),
                    HostValue::Bool(false),
                )]))
            }
        }
    })
}

/// Build the `host_memo_store(value)` callback. Consumes the
/// [`PENDING_MEMO_KEY`] left by the most-recent `host_memo_check` and
/// writes the value into [`MEMO_REGISTRY`].
pub(crate) fn build_host_memo_store_callback() -> BoxCallback {
    Box::new(|args| {
        let value: Value = args
            .first()
            .cloned()
            .map(HostValue::into_json)
            .unwrap_or(Value::Null);
        let key = PENDING_MEMO_KEY.with(|p| p.borrow_mut().take());
        if let Some(key) = key {
            MEMO_REGISTRY.with(|reg| {
                reg.borrow_mut().insert(key, value);
            });
        }
        Ok(HostValue::Null)
    })
}

/// Stub `host_memo_check` callback for the dry-mount path. Always
/// reports "not cached" so the JS-side fallback runs the factory
/// every render — semantically a no-op cache.
pub(crate) fn build_host_memo_check_stub_callback() -> BoxCallback {
    Box::new(|_args| {
        Ok(HostValue::Object(vec![(
            "cached".to_string(),
            HostValue::Bool(false),
        )]))
    })
}

/// Stub `host_memo_store` callback for the dry-mount path.
pub(crate) fn build_host_memo_store_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Build the `host_neighbors()` callback. Reads
/// `HOOK_CTX.neighbors` (set by the lifecycle for the active node)
/// via `core_use_neighbors` and returns the IDs as a JS array.
///
/// JS-side `useNeighbors(filter?)` calls this and applies an optional
/// JS-side filter — keeping the predicate in JS avoids a per-element
/// JSON FFI hop.
pub(crate) fn build_host_neighbors_callback() -> BoxCallback {
    Box::new(|_args| {
        let neighbors: Vec<String> = core_use_neighbors::<fn(&str) -> bool>(None);
        Ok(HostValue::Array(
            neighbors.into_iter().map(HostValue::String).collect(),
        ))
    })
}

/// Stub for the dry-mount path — returns an empty array.
pub(crate) fn build_host_neighbors_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Array(vec![])))
}

/// Build the `host_topology(token, filter)` callback. JS-side
/// `useTopology(handler, filter?)` registers `handler` under a token
/// (mirroring useMessage / useEffect) and dispatches the token here.
/// The Rust-side closure registered via `core_use_topology` fires
/// from `dispatch_topology_event` (well outside any body's
/// `call_function`, so re-entry into JSC via `invoke_global` is
/// safe), and re-invokes the JS handler with a marshalled
/// `{kind, nodeId? | u, v, key?}` event object.
pub(crate) fn build_host_topology_callback() -> BoxCallback {
    Box::new(|args| {
        let token = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_topology: expected Number token, got {other:?}"
                )))
            }
        };
        let filter = parse_topology_filter(args.get(1));

        let engine = current_engine().ok_or_else(|| {
            EngineError::Internal("host_topology: engine TLS unset".into())
        })?;
        let iso = current_iso().ok_or_else(|| {
            EngineError::Internal("host_topology: iso TLS unset".into())
        })?;

        core_use_topology(
            move |event: &TopologyEvent| {
                let host_event = topology_event_to_host_value(event);
                let result = engine.borrow_mut().invoke_global(
                    iso,
                    "__invokeCallback",
                    &[
                        HostValue::Number(token as f64),
                        HostValue::Array(vec![host_event]),
                    ],
                );
                if let Err(e) = result {
                    eprintln!(
                        "[graphreact-cli] useTopology handler invocation failed: {e}"
                    );
                }
            },
            filter,
        );
        Ok(HostValue::Null)
    })
}

/// Stub for the dry-mount path.
pub(crate) fn build_host_topology_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Build the `host_accept_edge(target)` callback. JS-side
/// `useAcceptEdge()` returns a closure that calls this with a target
/// id; we route through `core_use_accept_edge()` which returns the
/// `Rc<dyn Fn(&str)>` from HOOK_CTX. Read-only HOOK_CTX access; no
/// engine touch; safe to call while the body's `call_function`
/// borrow is held.
pub(crate) fn build_host_accept_edge_callback() -> BoxCallback {
    Box::new(|args| {
        let target = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_accept_edge: expected String target, got {other:?}"
                )))
            }
        };
        let accept_edge = core_use_accept_edge();
        accept_edge(&target);
        Ok(HostValue::Null)
    })
}

/// Stub for the dry-mount path.
pub(crate) fn build_host_accept_edge_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Build the `host_spawn_child(spec)` callback. JS-side `useSpawnChild()`
/// returns a closure that the body invokes with a spec object
/// `{id, component, props?, connectTo?}`. Before dispatching, JS
/// registers the `component` function under a `__hostCallbacks`
/// token (componentToken) so it survives the JSON FFI; the spec we
/// receive carries the token instead of the function reference.
///
/// We construct a [`NodeDefinition`] whose component closure invokes
/// `__invokeCallback(componentToken, [ctx])` via `JscEngine::invoke_global`
/// on every render. The closure captures engine + iso (read from TLS at
/// registration time, like host_message); evaluation happens from the
/// scheduler's main loop where no engine borrow is held, so the
/// re-entry into JSC is safe.
///
/// The composed spec is then passed to `core_use_spawn_child()(spec)`,
/// which enqueues a `SpawnChild` PendingMutation for the next cascade
/// round. Idempotent on existing IDs — repeat calls per render skip.
pub(crate) fn build_host_spawn_child_callback() -> BoxCallback {
    Box::new(|args| {
        let spec_value = args.first().cloned().ok_or_else(|| {
            EngineError::Internal("host_spawn_child: missing spec argument".into())
        })?;
        let spec = build_compose_spec_from_host_value(spec_value)?;
        let spawn = core_use_spawn_child();
        spawn(spec);
        Ok(HostValue::Null)
    })
}

/// Stub for the dry-mount path.
pub(crate) fn build_host_spawn_child_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Build the `host_unmount_self()` callback. JS-side `useUnmountSelf()`
/// returns a closure that calls this; we route through
/// `core_use_unmount_self()` which enqueues an `UnmountNode` mutation
/// for the active node.
pub(crate) fn build_host_unmount_self_callback() -> BoxCallback {
    Box::new(|_args| {
        let unmount = core_use_unmount_self();
        unmount();
        Ok(HostValue::Null)
    })
}

/// Stub for the dry-mount path.
pub(crate) fn build_host_unmount_self_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Build the `host_compose(specs)` callback. JS-side `useCompose()`
/// returns a closure that takes an array of specs (same shape as
/// `host_spawn_child` accepts, with each `component` already replaced
/// by a `componentToken`). We marshal the array into
/// `Vec<ComposeChildSpec>` and call `core_use_compose()(specs)`,
/// which spawns missing children and tracks the active node's
/// declared composition.
pub(crate) fn build_host_compose_callback() -> BoxCallback {
    Box::new(|args| {
        let arr = match args.first() {
            Some(HostValue::Array(arr)) => arr.clone(),
            None | Some(HostValue::Null) => Vec::new(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_compose: expected Array specs, got {other:?}"
                )))
            }
        };
        let specs: Vec<ComposeChildSpec> = arr
            .into_iter()
            .filter_map(|s| build_compose_spec_from_host_value(s).ok())
            .collect();
        let compose = core_use_compose();
        compose(specs);
        Ok(HostValue::Null)
    })
}

/// Stub for the dry-mount path.
pub(crate) fn build_host_compose_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

// --- Phase 3e-δ: ctx.directory finders ---

/// Build the `host_directory_find_by_id(id)` callback. Returns the
/// matching directory entry as `{id, role, depth, path}` or null.
pub(crate) fn build_host_directory_find_by_id_callback() -> BoxCallback {
    Box::new(|args| {
        let id = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let Some(directory) = current_directory() else {
            return Ok(HostValue::Null);
        };
        let directory_borrow = directory.borrow();
        match directory_borrow.find_by_id(&id) {
            Some(entry) => Ok(HostValue::Object(vec![
                ("id".to_string(), HostValue::String(entry.id.clone())),
                ("role".to_string(), HostValue::String(entry.role.clone())),
                ("depth".to_string(), HostValue::Number(f64::from(entry.depth))),
                (
                    "path".to_string(),
                    HostValue::Array(
                        entry
                            .path
                            .iter()
                            .map(|s| HostValue::String(s.clone()))
                            .collect(),
                    ),
                ),
            ])),
            None => Ok(HostValue::Null),
        }
    })
}

/// Build the `host_directory_find_by_role(role)` callback.
pub(crate) fn build_host_directory_find_by_role_callback() -> BoxCallback {
    Box::new(|args| {
        let role = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let Some(directory) = current_directory() else {
            return Ok(HostValue::Array(vec![]));
        };
        let ids = directory.borrow().find_by_role(&role);
        Ok(HostValue::Array(
            ids.into_iter().map(HostValue::String).collect(),
        ))
    })
}

/// Build the `host_directory_find_by_depth(depth)` callback.
pub(crate) fn build_host_directory_find_by_depth_callback() -> BoxCallback {
    Box::new(|args| {
        let depth = match args.first() {
            Some(HostValue::Number(n)) => *n as u32,
            _ => return Ok(HostValue::Array(vec![])),
        };
        let Some(directory) = current_directory() else {
            return Ok(HostValue::Array(vec![]));
        };
        let ids = directory.borrow().find_by_depth(depth);
        Ok(HostValue::Array(
            ids.into_iter().map(HostValue::String).collect(),
        ))
    })
}

/// Build the `host_directory_find_in_subtree(ancestorId)` callback.
pub(crate) fn build_host_directory_find_in_subtree_callback() -> BoxCallback {
    Box::new(|args| {
        let ancestor = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let Some(directory) = current_directory() else {
            return Ok(HostValue::Array(vec![]));
        };
        let ids = directory.borrow().find_in_subtree(&ancestor);
        Ok(HostValue::Array(
            ids.into_iter().map(HostValue::String).collect(),
        ))
    })
}

/// Build the `host_directory_is_ancestor(ancestorId, descendantId)` callback.
pub(crate) fn build_host_directory_is_ancestor_callback() -> BoxCallback {
    Box::new(|args| {
        let ancestor = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let descendant = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let Some(directory) = current_directory() else {
            return Ok(HostValue::Bool(false));
        };
        let result = directory.borrow().is_ancestor(&ancestor, &descendant);
        Ok(HostValue::Bool(result))
    })
}

/// Stubs for dry-mount.
pub(crate) fn build_host_directory_find_by_id_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}
pub(crate) fn build_host_directory_find_by_role_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_directory_find_by_depth_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_directory_find_in_subtree_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_directory_is_ancestor_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Bool(false)))
}

// --- Phase 3e-δ + M12.3a-followon-B + followon-C: __graphQueryProxy accessors ---
//
// As of M12.3a-followon-C (2026-05-02), these read from a LIVE
// `Rc<RefCell<CommunicationGraph>>` cloned from `Scheduler::graph` —
// dynamic `useSpawnChild` / `useCompose` / `useAcceptEdge` mutations
// are visible on the next `useGraphQuery` invocation. Each callback
// `borrow()`s the live graph at call time. `borrow_mut()` is never
// taken from inside a host_graph_* callback — the scheduler holds it
// during applied mutations, never during JS body invocation.
//
// followon-B widened the surface from 4 basic accessors (neighbors /
// hasNode / hasEdge / nodeCount) to 11 by adding 7 algorithm wrappers
// (shortestPath / allPaths / aStarPath / bfs / dfs /
// connectedComponents / cycleDetection). topologicalSort and
// edge-filtered subgraph views remain deferred — both are subsumed by
// sub-item E (Tier 2: policy-derived reachability), where direction
// emerges per message-type from `PermissionManifest.emits/accepts`
// pairs rather than from the underlying undirected `CommunicationGraph`.
//
// Per-element JS filter predicates (e.g. `bfs(from, n => n !== 'x')`)
// stay deferred indefinitely — orthogonal to Tier 2/3 design and no
// plausible fixture demand.

pub(crate) fn build_host_graph_neighbors_callback() -> BoxCallback {
    Box::new(|args| {
        let id = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let neighbors = snapshot.graph.borrow().neighbors(&id).unwrap_or_default();
        Ok(HostValue::Array(
            neighbors.into_iter().map(HostValue::String).collect(),
        ))
    })
}

pub(crate) fn build_host_graph_has_node_callback() -> BoxCallback {
    Box::new(|args| {
        let id = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Bool(false));
        };
        let has = snapshot.graph.borrow().has_node(&id);
        Ok(HostValue::Bool(has))
    })
}

pub(crate) fn build_host_graph_has_edge_callback() -> BoxCallback {
    Box::new(|args| {
        let u = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let v = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Bool(false));
        };
        let has = snapshot.graph.borrow().has_edge(&u, &v, None);
        Ok(HostValue::Bool(has))
    })
}

pub(crate) fn build_host_graph_node_count_callback() -> BoxCallback {
    Box::new(|_args| {
        let count = current_graph_snapshot()
            .map(|s| s.graph.borrow().nodes().count() as f64)
            .unwrap_or(0.0);
        Ok(HostValue::Number(count))
    })
}

// followon-B: algorithm-surface accessors. Each wraps a function from
// graphreact-algorithms. Errors collapse to a "neutral" return
// (null / empty / false) — JS callers shouldn't see Rust errors as
// thrown exceptions, since the typical pattern is
// `useGraphQuery(g => ({ ... }))` over a static snapshot where a
// missing source node just means the path doesn't exist.

pub(crate) fn build_host_graph_shortest_path_callback() -> BoxCallback {
    Box::new(|args| {
        let from = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let to = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Null);
        };
        let g = snapshot.graph.borrow();
        match graphreact_algorithms::shortest_path(
            &g,
            &from,
            &to,
            &graphreact_algorithms::PathOptions::default(),
        ) {
            Ok(Some(path)) => Ok(HostValue::Array(
                path.into_iter().map(HostValue::String).collect(),
            )),
            Ok(None) | Err(_) => Ok(HostValue::Null),
        }
    })
}

pub(crate) fn build_host_graph_all_paths_callback() -> BoxCallback {
    Box::new(|args| {
        let from = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let to = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let max_depth = match args.get(2) {
            Some(HostValue::Number(n)) if *n > 0.0 => Some(*n as usize),
            _ => None,
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let opts = graphreact_algorithms::AllPathsOptions {
            edge_filter: None,
            max_depth,
        };
        let g = snapshot.graph.borrow();
        match graphreact_algorithms::all_paths(&g, &from, &to, &opts) {
            Ok(paths) => Ok(HostValue::Array(
                paths
                    .into_iter()
                    .map(|p| HostValue::Array(p.into_iter().map(HostValue::String).collect()))
                    .collect(),
            )),
            Err(_) => Ok(HostValue::Array(vec![])),
        }
    })
}

pub(crate) fn build_host_graph_a_star_path_callback() -> BoxCallback {
    Box::new(|args| {
        let from = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let to = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Null);
        };
        // No JS-side heuristic in followon-B (per-call FFI overhead);
        // without one A* degenerates to BFS shortest-path with uniform
        // edge cost — same as graphreact-algorithms documents.
        let opts = graphreact_algorithms::AStarOptions::default();
        let g = snapshot.graph.borrow();
        match graphreact_algorithms::astar_path(&g, &from, &to, &opts, None) {
            Ok(Some(path)) => Ok(HostValue::Array(
                path.into_iter().map(HostValue::String).collect(),
            )),
            Ok(None) | Err(_) => Ok(HostValue::Null),
        }
    })
}

pub(crate) fn build_host_graph_bfs_callback() -> BoxCallback {
    Box::new(|args| {
        let from = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let depth_limit = match args.get(1) {
            Some(HostValue::Number(n)) if *n > 0.0 => Some(*n as usize),
            _ => None,
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let g = snapshot.graph.borrow();
        match graphreact_algorithms::bfs_nodes(&g, &from, depth_limit) {
            Ok(nodes) => Ok(HostValue::Array(
                nodes.into_iter().map(HostValue::String).collect(),
            )),
            Err(_) => Ok(HostValue::Array(vec![])),
        }
    })
}

pub(crate) fn build_host_graph_dfs_callback() -> BoxCallback {
    Box::new(|args| {
        let from = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let depth_limit = match args.get(1) {
            Some(HostValue::Number(n)) if *n > 0.0 => Some(*n as usize),
            _ => None,
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let g = snapshot.graph.borrow();
        match graphreact_algorithms::dfs_nodes(&g, &from, depth_limit) {
            Ok(nodes) => Ok(HostValue::Array(
                nodes.into_iter().map(HostValue::String).collect(),
            )),
            Err(_) => Ok(HostValue::Array(vec![])),
        }
    })
}

pub(crate) fn build_host_graph_connected_components_callback() -> BoxCallback {
    Box::new(|_args| {
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let g = snapshot.graph.borrow();
        match graphreact_algorithms::connected_components(&g) {
            Ok(components) => {
                let mut out: Vec<HostValue> = Vec::with_capacity(components.len());
                for component in components {
                    // FxHashSet → sorted Vec<String> for stable JS-side ordering.
                    let mut members: Vec<String> = component.into_iter().collect();
                    members.sort();
                    out.push(HostValue::Array(
                        members.into_iter().map(HostValue::String).collect(),
                    ));
                }
                Ok(HostValue::Array(out))
            }
            Err(_) => Ok(HostValue::Array(vec![])),
        }
    })
}

pub(crate) fn build_host_graph_cycle_detection_callback() -> BoxCallback {
    Box::new(|_args| {
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Object(vec![
                ("hasCycle".to_string(), HostValue::Bool(false)),
                ("cycle".to_string(), HostValue::Null),
            ]));
        };
        let g = snapshot.graph.borrow();
        let has = graphreact_algorithms::has_cycle(&g).unwrap_or(false);
        let cycle = graphreact_algorithms::find_cycle(&g).ok().flatten();
        let cycle_value = match cycle {
            Some(edges) => HostValue::Array(
                edges
                    .into_iter()
                    .map(|(u, v)| HostValue::Array(vec![HostValue::String(u), HostValue::String(v)]))
                    .collect(),
            ),
            None => HostValue::Null,
        };
        Ok(HostValue::Object(vec![
            ("hasCycle".to_string(), HostValue::Bool(has)),
            ("cycle".to_string(), cycle_value),
        ]))
    })
}

pub(crate) fn build_host_graph_neighbors_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_graph_has_node_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Bool(false)))
}
pub(crate) fn build_host_graph_has_edge_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Bool(false)))
}
pub(crate) fn build_host_graph_node_count_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Number(0.0)))
}
pub(crate) fn build_host_graph_shortest_path_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}
pub(crate) fn build_host_graph_all_paths_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_graph_a_star_path_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}
pub(crate) fn build_host_graph_bfs_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_graph_dfs_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_graph_connected_components_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_graph_cycle_detection_stub_callback() -> BoxCallback {
    Box::new(|_args| {
        Ok(HostValue::Object(vec![
            ("hasCycle".to_string(), HostValue::Bool(false)),
            ("cycle".to_string(), HostValue::Null),
        ]))
    })
}

// --- M12.3a-followon-E: Tier 2 (policy-derived reachability) ---
//
// Tier 2 = the directed reachability graph derived from per-node
// `PermissionManifest.emits/accepts` pairs for a given message type.
// Per the three-tier possibility model, Tier 2 generalises Tier 1
// (the live `CommunicationGraph`) by treating direction as
// policy-derived: edge (s, u) exists in tier2(T) iff `s.manifest.emits`
// contains `T` AND `u.manifest.accepts` contains `T`. This makes
// `topologicalSort(T)` well-defined (closes followon-B's deferral),
// and lets pathfinding answer "what message types can flow where" for
// the application classes documented in
// `project_graphreact_application_space.md`.
//
// The eight named application classes (Self-Healing Infrastructure,
// Living Contracts, Organizational Intelligence, Generative World
// Building, Knowledge Graphs That Reason, Adaptive Education,
// Biological Pathway Simulation, Decentralized Reputation Systems)
// all need at least Tier 2; ComplyFlow + Thymus are the two
// near-term candidates that surface the need first. See
// `project_graphreact_three_tier_possibility_model.md` for the
// canonical framing.
//
// Implementation: each Tier 2 query takes a message-type string,
// builds the directed adjacency on demand from `snapshot.manifests`,
// and runs the corresponding traversal/pathfinding algorithm.
// Adjacency build is O(N²) per query for N registered nodes; cheap at
// fixture scale. A future slice can cache per-batch if needed.
//
// Cost-from-policy A* heuristics shipped: `latency`, `compliance`,
// `hierarchy`. The remaining two from the design (`trust-attenuated`,
// `prerequisite-distance`) follow the same callback signature and
// land additively as application fixtures surface the need.
//
// Followon-E phase 1 ships these accessors over the directed Tier 2
// graph: `nodeCount`, `hasNode`, `neighbors`, `hasEdge`, `bfs`, `dfs`,
// `shortestPath`, `topologicalSort`, `aStarPath`. Deferred to
// followon-E phase 2: `allPaths`, `connectedComponents` (SCC),
// `cycleDetection` (directed). Tier 1 versions of those remain
// available via `g.tier1()`.

/// Directed adjacency + node set for a single message type, derived
/// from per-node `PermissionManifest.emits/accepts` pairs. Built on
/// demand by [`build_tier2_adj`] from a `GraphSnapshot.manifests`
/// borrow. Iteration order over `nodes` is sorted for stable BFS /
/// DFS / topo output; `out` lists are also sorted.
struct Tier2Adj {
    /// All nodes registered with the scheduler at query time. Sorted.
    nodes: Vec<String>,
    /// `out[s]` = sorted list of nodes `u` such that `s` emits T and
    /// `u` accepts T (where T was the message type passed to the
    /// builder). Sources without any outgoing edges have an entry
    /// with an empty Vec.
    out: FxHashMap<String, Vec<String>>,
}

/// Tier 3 directed adjacency. M12.3a-followon-F. Same shape as
/// [`Tier2Adj`] but built by [`build_tier3_adj`] which consults each
/// candidate target's edge-accept predicate (installed by
/// `use_edge_accept_handler`) before including the (s, u) pair. Both
/// types implement [`DirectedAdj`] so the algorithm helpers
/// (`tier2_bfs`/`tier2_astar_path`/etc.) work over either tier without
/// duplication.
struct Tier3Adj {
    /// All nodes registered with the scheduler at query time. Sorted.
    nodes: Vec<String>,
    /// `out[s]` = sorted list of nodes `u` for which the Tier 2
    /// (structural or directory-shortcut, per the same flag) candidacy
    /// passes AND `predicates.get(u)`, when present, returns true for
    /// `(s, msg_type)`.
    out: FxHashMap<String, Vec<String>>,
}

/// Common shape consumed by Tier 2 / Tier 3 algorithm helpers. Lets
/// `tier2_*` functions remain a single canonical implementation while
/// being callable with either a [`Tier2Adj`] or a [`Tier3Adj`].
/// M12.3a-followon-F.
trait DirectedAdj {
    fn nodes_slice(&self) -> &[String];
    fn neighbors(&self, id: &str) -> &[String];
    fn has_node(&self, id: &str) -> bool;
    fn has_edge(&self, u: &str, v: &str) -> bool;
}

impl DirectedAdj for Tier2Adj {
    fn nodes_slice(&self) -> &[String] {
        &self.nodes
    }

    fn neighbors(&self, id: &str) -> &[String] {
        self.out.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    fn has_node(&self, id: &str) -> bool {
        self.out.contains_key(id)
    }

    fn has_edge(&self, u: &str, v: &str) -> bool {
        self.out.get(u).is_some_and(|outs| outs.iter().any(|t| t == v))
    }
}

impl DirectedAdj for Tier3Adj {
    fn nodes_slice(&self) -> &[String] {
        &self.nodes
    }

    fn neighbors(&self, id: &str) -> &[String] {
        self.out.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    fn has_node(&self, id: &str) -> bool {
        self.out.contains_key(id)
    }

    fn has_edge(&self, u: &str, v: &str) -> bool {
        self.out.get(u).is_some_and(|outs| outs.iter().any(|t| t == v))
    }
}

/// Build the Tier 2 directed adjacency for `msg_type`. Two flavors,
/// selected by `include_directory_shortcuts`:
///
/// **Structural flavor** (`include_directory_shortcuts == false`):
/// edge `(s, u)` exists iff
/// 1. A Tier 1 edge `s ↔ u` exists in the live `CommunicationGraph`
///    (so the message has a real structural path to traverse), AND
/// 2. `s.manifest.emits` contains `msg_type`, AND
/// 3. `u.manifest.accepts` contains `msg_type`.
///
/// Preserves both topology AND policy. Multi-hop pathfinding remains
/// meaningful — cost-from-policy A* heuristics can choose between
/// alternative chains based on node/edge attrs.
///
/// **Directory-shortcut flavor** (`include_directory_shortcuts == true`):
/// edge `(s, u)` exists iff `s.manifest.emits` contains `msg_type` AND
/// `u.manifest.accepts` contains `msg_type` — Tier 1 edge presence is
/// IGNORED. This is the canonical "Tier 2 = directory-augmented
/// reachability" semantic from the three-tier possibility model: any
/// emitter can reach any acceptor in 1 hop via direct messaging
/// (`emit({direct: targetId})`), the directory makes the lookup work
/// without an edge.
///
/// Path-quality questions ("which chain is most compliant?") want the
/// structural flavor; reachability questions ("can S talk to U at
/// all?") want the directory-shortcut flavor. Same accessor surface;
/// different adjacency.
fn build_tier2_adj(
    graph: &CommunicationGraph,
    manifests: &FxHashMap<String, PermissionManifest>,
    msg_type: &str,
    include_directory_shortcuts: bool,
) -> Tier2Adj {
    let mut nodes: Vec<String> = manifests.keys().cloned().collect();
    nodes.sort();

    let mut out: FxHashMap<String, Vec<String>> = FxHashMap::default();
    for id in &nodes {
        let emits_t = manifests
            .get(id)
            .is_some_and(|m| m.emits.iter().any(|t| t == msg_type));
        let outs: Vec<String> = if !emits_t {
            Vec::new()
        } else if include_directory_shortcuts {
            // Directory-shortcut flavor: every acceptor for T is
            // reachable in 1 hop, regardless of Tier 1 edge presence.
            nodes
                .iter()
                .filter(|n| {
                    n.as_str() != id.as_str()
                        && manifests
                            .get(*n)
                            .is_some_and(|m| m.accepts.iter().any(|t| t == msg_type))
                })
                .cloned()
                .collect()
        } else {
            // Structural flavor: filter Tier 1 graph neighbors by
            // acceptor status. Sorted for deterministic traversal.
            let mut neighbors = graph.neighbors(id).unwrap_or_default();
            neighbors.sort();
            neighbors
                .into_iter()
                .filter(|n| {
                    n != id
                        && manifests
                            .get(n)
                            .is_some_and(|m| m.accepts.iter().any(|t| t == msg_type))
                })
                .collect()
        };
        out.insert(id.clone(), outs);
    }
    Tier2Adj { nodes, out }
}

/// Build the Tier 3 directed adjacency for `msg_type`. Tier 3 = Tier 2
/// ∪ {(s, u) where target u's edge-accept predicate (installed via
/// `use_edge_accept_handler`) accepts the (s, msg_type) pair}. The
/// flavor flag (`include_directory_shortcuts`) is forwarded to the
/// Tier 2 candidacy step — Tier 3 narrows whichever Tier 2 view the
/// caller asked for.
///
/// **Negotiation semantics (M12.3a-followon-F):** Tier 3 captures
/// "what's reachable IF the runtime DECIDED to negotiate edges." For
/// each candidate (s, u), invoke `predicates[u](s, msg_type)`. If the
/// predicate is registered AND returns false, exclude the pair. If
/// the predicate is absent (no `use_edge_accept_handler` called for
/// that node), default-accept — preserves Phase-9 status quo and
/// makes Tier 3 ≥ Tier 2 monotonically: a system with NO predicates
/// installed has Tier 3 == Tier 2.
///
/// The predicate runs at query time; the AcceptEdge mutation is NOT
/// triggered. Pure dry-run: gives the caller the projected
/// reachability without committing the underlying graph rewrite.
fn build_tier3_adj(
    graph: &CommunicationGraph,
    manifests: &FxHashMap<String, PermissionManifest>,
    predicates: &FxHashMap<String, EdgeAcceptPredicate>,
    msg_type: &str,
    include_directory_shortcuts: bool,
) -> Tier3Adj {
    // Reuse Tier 2 candidacy as the supremum, then filter through
    // each target's predicate. This guarantees Tier 3 ⊆ Tier 2 (when
    // predicates veto) and Tier 3 == Tier 2 (when no predicates are
    // installed).
    let tier2 = build_tier2_adj(graph, manifests, msg_type, include_directory_shortcuts);

    let mut out: FxHashMap<String, Vec<String>> = FxHashMap::default();
    for s in &tier2.nodes {
        let candidates = tier2.out.get(s).cloned().unwrap_or_default();
        let filtered: Vec<String> = candidates
            .into_iter()
            .filter(|u| match predicates.get(u) {
                Some(predicate) => predicate(s, msg_type),
                None => true, // Default-accept.
            })
            .collect();
        out.insert(s.clone(), filtered);
    }

    Tier3Adj {
        nodes: tier2.nodes,
        out,
    }
}

/// Tier 2/3 BFS over the directed adjacency. Returns nodes in order
/// of discovery (including `start`). Honours optional `depth_limit`
/// (caller-provided; matches Tier 1 `bfs` shape). Generic over
/// [`DirectedAdj`] so Tier 3 callers (M12.3a-followon-F) reuse the
/// same body.
fn tier2_bfs(adj: &dyn DirectedAdj, start: &str, depth_limit: Option<usize>) -> Vec<String> {
    use std::collections::VecDeque;
    if !adj.has_node(start) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((start.to_string(), 0));
    visited.insert(start.to_string());
    while let Some((cur, depth)) = queue.pop_front() {
        out.push(cur.clone());
        if let Some(limit) = depth_limit {
            if depth >= limit {
                continue;
            }
        }
        for nbr in adj.neighbors(&cur) {
            if visited.insert(nbr.clone()) {
                queue.push_back((nbr.clone(), depth + 1));
            }
        }
    }
    out
}

/// Tier 2/3 DFS over the directed adjacency. Pre-order traversal.
fn tier2_dfs(adj: &dyn DirectedAdj, start: &str, depth_limit: Option<usize>) -> Vec<String> {
    if !adj.has_node(start) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stack: Vec<(String, usize)> = vec![(start.to_string(), 0)];
    while let Some((cur, depth)) = stack.pop() {
        if !visited.insert(cur.clone()) {
            continue;
        }
        out.push(cur.clone());
        if let Some(limit) = depth_limit {
            if depth >= limit {
                continue;
            }
        }
        // Push in reverse so the first neighbor comes off the stack
        // first — preserves the sorted-neighbor traversal order.
        for nbr in adj.neighbors(&cur).iter().rev() {
            if !visited.contains(nbr) {
                stack.push((nbr.clone(), depth + 1));
            }
        }
    }
    out
}

/// Tier 2 BFS-based shortest path (uniform edge cost). Returns the
/// path as `Vec<String>` (inclusive of `from` and `to`); `None` if
/// `to` isn't reachable.
fn tier2_shortest_path(adj: &dyn DirectedAdj, from: &str, to: &str) -> Option<Vec<String>> {
    use std::collections::VecDeque;
    if !adj.has_node(from) || !adj.has_node(to) {
        return None;
    }
    if from == to {
        return Some(vec![from.to_string()]);
    }
    let mut prev: FxHashMap<String, String> = FxHashMap::default();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(from.to_string());
    visited.insert(from.to_string());
    while let Some(cur) = queue.pop_front() {
        for nbr in adj.neighbors(&cur) {
            if !visited.insert(nbr.clone()) {
                continue;
            }
            prev.insert(nbr.clone(), cur.clone());
            if nbr == to {
                // Reconstruct path.
                let mut path = vec![to.to_string()];
                let mut step = to.to_string();
                while let Some(p) = prev.get(&step) {
                    path.push(p.clone());
                    step = p.clone();
                    if step == from {
                        break;
                    }
                }
                path.reverse();
                return Some(path);
            }
            queue.push_back(nbr.clone());
        }
    }
    None
}

/// Recursive DFS helper for [`tier2_all_paths`]. Lifted to module
/// scope to satisfy `clippy::items_after_statements`.
fn tier2_all_paths_dfs(
    adj: &dyn DirectedAdj,
    cur: &str,
    target: &str,
    path: &mut Vec<String>,
    on_path: &mut std::collections::HashSet<String>,
    max_depth: Option<usize>,
    out: &mut Vec<Vec<String>>,
) {
    if let Some(d) = max_depth {
        if path.len() > d {
            return;
        }
    }
    for nbr in adj.neighbors(cur) {
        if on_path.contains(nbr) {
            continue;
        }
        path.push(nbr.clone());
        on_path.insert(nbr.clone());
        if nbr == target {
            out.push(path.clone());
        } else {
            tier2_all_paths_dfs(adj, nbr, target, path, on_path, max_depth, out);
        }
        path.pop();
        on_path.remove(nbr);
    }
}

/// Tier 2 all simple directed paths from `from` to `to`. DFS with a
/// visited-stack to prevent revisiting nodes within the SAME path
/// (matches graphreact-algorithms' `all_paths` semantics for
/// undirected, adapted for directed traversal). Honours optional
/// `max_depth` cap. Returns paths sorted by length (then lexicographic
/// for stable output).
fn tier2_all_paths(
    adj: &dyn DirectedAdj,
    from: &str,
    to: &str,
    max_depth: Option<usize>,
) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    if !adj.has_node(from) || !adj.has_node(to) {
        return out;
    }
    if from == to {
        out.push(vec![from.to_string()]);
        return out;
    }

    let mut path = vec![from.to_string()];
    let mut on_path: std::collections::HashSet<String> = std::collections::HashSet::new();
    on_path.insert(from.to_string());
    tier2_all_paths_dfs(adj, from, to, &mut path, &mut on_path, max_depth, &mut out);

    // Sort paths: shorter first; ties broken by lexicographic order
    // for deterministic output.
    out.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    out
}

/// Tarjan's SCC inner recursion — lifted to module scope to satisfy
/// `clippy::items_after_statements`. State threaded through mutable
/// references; node ids are Strings since `Tier2Adj` is keyed by
/// String already (cheap clones at fixture scale).
#[allow(clippy::too_many_arguments)]
fn tier2_strongconnect(
    v: &str,
    adj: &dyn DirectedAdj,
    index_counter: &mut usize,
    index: &mut FxHashMap<String, usize>,
    lowlink: &mut FxHashMap<String, usize>,
    on_stack: &mut std::collections::HashSet<String>,
    stack: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    index.insert(v.to_string(), *index_counter);
    lowlink.insert(v.to_string(), *index_counter);
    *index_counter += 1;
    stack.push(v.to_string());
    on_stack.insert(v.to_string());

    for w in adj.neighbors(v) {
        if !index.contains_key(w) {
            tier2_strongconnect(w, adj, index_counter, index, lowlink, on_stack, stack, out);
            let w_low = lowlink.get(w).copied().unwrap_or(usize::MAX);
            let v_low = lowlink.get(v).copied().unwrap_or(usize::MAX);
            lowlink.insert(v.to_string(), v_low.min(w_low));
        } else if on_stack.contains(w) {
            let w_idx = index.get(w).copied().unwrap_or(usize::MAX);
            let v_low = lowlink.get(v).copied().unwrap_or(usize::MAX);
            lowlink.insert(v.to_string(), v_low.min(w_idx));
        }
    }

    if lowlink.get(v).copied().unwrap_or(usize::MAX)
        == index.get(v).copied().unwrap_or(usize::MAX)
    {
        let mut component: Vec<String> = Vec::new();
        loop {
            let w = stack.pop().expect("stack non-empty during SCC pop");
            on_stack.remove(&w);
            let done = w == v;
            component.push(w);
            if done {
                break;
            }
        }
        component.sort();
        out.push(component);
    }
}

/// Tier 2 strongly connected components via Tarjan's algorithm over
/// the directed adjacency. Returns components as `Vec<Vec<String>>`,
/// each inner Vec sorted lexicographically. Outer Vec sorted by
/// component's smallest member for stable output.
fn tier2_strongly_connected_components(adj: &dyn DirectedAdj) -> Vec<Vec<String>> {
    use std::collections::HashSet;

    let mut index_counter: usize = 0;
    let mut index: FxHashMap<String, usize> = FxHashMap::default();
    let mut lowlink: FxHashMap<String, usize> = FxHashMap::default();
    let mut on_stack: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();
    let mut out: Vec<Vec<String>> = Vec::new();

    // Iterate in sorted order for deterministic output.
    for v in adj.nodes_slice() {
        if !index.contains_key(v) {
            tier2_strongconnect(
                v,
                adj,
                &mut index_counter,
                &mut index,
                &mut lowlink,
                &mut on_stack,
                &mut stack,
                &mut out,
            );
        }
    }

    // Sort components by smallest member for stable outer-Vec order.
    out.sort_by(|a, b| {
        let amin = a.first().cloned().unwrap_or_default();
        let bmin = b.first().cloned().unwrap_or_default();
        amin.cmp(&bmin)
    });
    out
}

/// 3-color DFS coloring for [`tier2_directed_cycle_detection`]. White
/// = unvisited, Gray = on the current DFS path, Black = fully
/// processed. Lifted to module scope to satisfy
/// `clippy::items_after_statements`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier2DfsColor {
    White,
    Gray,
    Black,
}

/// Recursive DFS for cycle detection — returns the closing edge
/// `(v, w)` when a back-edge into a Gray node is found, else `None`.
fn tier2_cycle_dfs(
    v: &str,
    adj: &dyn DirectedAdj,
    color: &mut std::collections::HashMap<String, Tier2DfsColor>,
    parent: &mut std::collections::HashMap<String, String>,
) -> Option<(String, String)> {
    color.insert(v.to_string(), Tier2DfsColor::Gray);
    for w in adj.neighbors(v) {
        match color.get(w).copied().unwrap_or(Tier2DfsColor::White) {
            Tier2DfsColor::White => {
                parent.insert(w.clone(), v.to_string());
                if let Some(found) = tier2_cycle_dfs(w, adj, color, parent) {
                    return Some(found);
                }
            }
            Tier2DfsColor::Gray => {
                return Some((v.to_string(), w.clone()));
            }
            Tier2DfsColor::Black => {}
        }
    }
    color.insert(v.to_string(), Tier2DfsColor::Black);
    None
}

/// Tier 2 directed cycle detection via 3-color DFS (white/gray/black
/// node coloring). Returns `(has_cycle, cycle_edges)` where
/// `cycle_edges` is `Some(Vec<(u, v)>)` representing one detected
/// cycle's edge sequence on success, `None` if acyclic.
fn tier2_directed_cycle_detection(adj: &dyn DirectedAdj) -> (bool, Option<Vec<(String, String)>>) {
    use std::collections::HashMap;

    let mut color: HashMap<String, Tier2DfsColor> = HashMap::new();
    let mut parent: HashMap<String, String> = HashMap::new();

    for v in adj.nodes_slice() {
        if color.get(v).copied().unwrap_or(Tier2DfsColor::White) == Tier2DfsColor::White {
            if let Some((closer, target)) = tier2_cycle_dfs(v, adj, &mut color, &mut parent) {
                // Reconstruct cycle: walk parent[] backward from
                // `closer` until we hit `target`, then add the
                // closing edge.
                let mut nodes_in_cycle: Vec<String> = vec![closer.clone()];
                let mut step = closer.clone();
                while let Some(p) = parent.get(&step) {
                    if p == &target {
                        nodes_in_cycle.push(target.clone());
                        break;
                    }
                    nodes_in_cycle.push(p.clone());
                    step = p.clone();
                    if nodes_in_cycle.len() > adj.nodes_slice().len() {
                        break;
                    }
                }
                nodes_in_cycle.reverse();
                let mut edges: Vec<(String, String)> = Vec::new();
                for win in nodes_in_cycle.windows(2) {
                    edges.push((win[0].clone(), win[1].clone()));
                }
                edges.push((closer, target));
                return (true, Some(edges));
            }
        }
    }
    (false, None)
}

/// Tier 2 directed Kahn's topological sort. Returns the ordering on
/// success; `None` if the directed graph has a cycle (Kahn's leaves
/// some nodes unprocessed). Closes followon-B's `topologicalSort`
/// deferral — direction is now well-defined per message type.
fn tier2_topological_sort(adj: &dyn DirectedAdj) -> Option<Vec<String>> {
    use std::collections::VecDeque;
    // In-degree per node.
    let mut in_deg: FxHashMap<String, usize> =
        adj.nodes_slice().iter().map(|n| (n.clone(), 0)).collect();
    for u in adj.nodes_slice() {
        for v in adj.neighbors(u) {
            *in_deg.entry(v.clone()).or_insert(0) += 1;
        }
    }
    // Seed queue with sources (in-degree 0). Iterate in sorted order
    // for deterministic output.
    let mut queue: VecDeque<String> = VecDeque::new();
    for n in adj.nodes_slice() {
        if in_deg.get(n).copied().unwrap_or(0) == 0 {
            queue.push_back(n.clone());
        }
    }
    let mut out: Vec<String> = Vec::with_capacity(adj.nodes_slice().len());
    while let Some(cur) = queue.pop_front() {
        out.push(cur.clone());
        for nbr in adj.neighbors(&cur) {
            if let Some(d) = in_deg.get_mut(nbr) {
                if *d > 0 {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(nbr.clone());
                    }
                }
            }
        }
    }
    if out.len() == adj.nodes_slice().len() {
        Some(out)
    } else {
        None
    }
}

/// Cost-from-policy heuristic identifier — selects the per-step cost
/// + estimate-to-target functions used by Tier 2 A*. The full set of
/// five named in the application-space framing is shipped:
///
/// - `latency` — sum of edge `latency` attrs (Self-Healing #1, Org Intelligence #3).
/// - `compliance` — penalise hops through nodes lacking `compliant` attr
///   or edges lacking `encrypted` attr (ComplyFlow Layer 2, Self-Healing #1).
/// - `hierarchy` — minimise depth-difference via directory entries
///   (Org Intelligence #3).
/// - `trust-attenuated` — TTL-attenuated path cost (Reputation #8).
///   Reads edge `trust` attr (number 0..1); per-step cost = 1.0 / trust
///   so low-trust hops are exponentially expensive.
/// - `prerequisite-distance` — minimise prerequisite-gap by node
///   `difficulty` attr (Adaptive Education #6). Per-step cost
///   = 1.0 + difficulty(target).
//
// (clippy::doc_list_item_without_indent: the `///` comment block
// above already wraps the bullet continuations correctly; suppressed
// inline since refactoring the doc style here would obscure the
// per-heuristic descriptions.)
#[allow(clippy::doc_lazy_continuation)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AStarHeuristic {
    /// Default for back-compat: uniform edge cost, zero estimate.
    /// `aStarPath` without a heuristic option degenerates to BFS
    /// shortest-path — same as Tier 1's `aStarPath`.
    Uniform,
    Latency,
    Compliance,
    Hierarchy,
    TrustAttenuated,
    PrerequisiteDistance,
}

impl AStarHeuristic {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "latency" => Some(Self::Latency),
            "compliance" => Some(Self::Compliance),
            "hierarchy" => Some(Self::Hierarchy),
            "trust-attenuated" => Some(Self::TrustAttenuated),
            "prerequisite-distance" => Some(Self::PrerequisiteDistance),
            _ => None,
        }
    }
}

/// Per-step (current → next) cost function for Tier 2 A*.
fn tier2_step_cost(
    h: AStarHeuristic,
    snapshot: &GraphSnapshot,
    from: &str,
    to: &str,
    directory: Option<&Rc<RefCell<GraphDirectory>>>,
) -> f64 {
    match h {
        AStarHeuristic::Uniform => 1.0,
        AStarHeuristic::Latency => {
            // Sum the `latency` attr on the underlying edge if present
            // (Tier 2 path corresponds to direct messaging when no
            // edge exists, in which case the cost defaults to 1.0).
            let g = snapshot.graph.borrow();
            if let Ok(attrs) = g.get_edge_data(from, to, None) {
                attrs
                    .get("latency")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(1.0)
            } else {
                // No real edge → policy-only direct-messaging hop.
                // Use the unit cost; fixtures can override by adding a
                // `latency` attr to the actual `<edge ...>` if they
                // want to penalise direct hops.
                1.0
            }
        }
        AStarHeuristic::Compliance => {
            // Per-hop compliance cost: penalise unencrypted edges
            // (edge attr `encrypted` falsy) and non-compliant target
            // nodes (node attr `compliant` falsy). Both default to the
            // permissive case (`true`) so attr-less fixtures behave as
            // BFS-equivalent.
            let g = snapshot.graph.borrow();
            let mut cost = 1.0;
            if let Ok(eattrs) = g.get_edge_data(from, to, None) {
                let encrypted = eattrs
                    .get("encrypted")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                if !encrypted {
                    cost += 10.0;
                }
            }
            if let Ok(nattrs) = g.get_node_data(to) {
                let compliant = nattrs
                    .get("compliant")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                if !compliant {
                    cost += 10.0;
                }
            }
            cost
        }
        AStarHeuristic::Hierarchy => {
            // Cost = absolute depth difference between current and
            // target. Reads from the directory; falls back to unit
            // cost if either entry is missing.
            let Some(dir) = directory else { return 1.0 };
            let dir = dir.borrow();
            let from_depth = dir.find_by_id(from).map(|e| i64::from(e.depth));
            let to_depth = dir.find_by_id(to).map(|e| i64::from(e.depth));
            match (from_depth, to_depth) {
                (Some(a), Some(b)) => 1.0 + (a - b).unsigned_abs() as f64,
                _ => 1.0,
            }
        }
        AStarHeuristic::TrustAttenuated => {
            // Cost = 1.0 / trust where trust ∈ (0, 1]. Low-trust hops
            // become exponentially expensive (trust=0.1 → cost=10;
            // trust=0.5 → cost=2). Trust attr lives on the edge;
            // missing/zero trust treated as fully-trusted (cost=1.0)
            // so attr-less fixtures behave as BFS-equivalent.
            let g = snapshot.graph.borrow();
            let trust = g
                .get_edge_data(from, to, None)
                .ok()
                .and_then(|attrs| attrs.get("trust").and_then(serde_json::Value::as_f64));
            match trust {
                Some(t) if t > 0.0 => 1.0 / t,
                _ => 1.0,
            }
        }
        AStarHeuristic::PrerequisiteDistance => {
            // Cost = 1.0 + difficulty(target). Each step pays the
            // intrinsic difficulty of the concept being approached.
            // For Adaptive Education: paths through low-difficulty
            // intermediates are preferred (gentler curriculum). Node
            // attr `difficulty` defaults to 0.0 (no penalty) so
            // attr-less fixtures behave as BFS-equivalent.
            let g = snapshot.graph.borrow();
            let difficulty = g
                .get_node_data(to)
                .ok()
                .and_then(|attrs| attrs.get("difficulty").and_then(serde_json::Value::as_f64))
                .unwrap_or(0.0);
            1.0 + difficulty.max(0.0)
        }
    }
}

/// Estimated remaining cost (heuristic h) from `cur` to `target`. Must
/// be admissible (≤ true cost) for A* optimality. The shipped
/// heuristics return either 0 (BFS-equivalent, always admissible) or
/// |depth(cur) - depth(target)| for hierarchy (admissible since each
/// hop changes depth by at most 1 in either direction).
fn tier2_heuristic(
    h: AStarHeuristic,
    cur: &str,
    target: &str,
    directory: Option<&Rc<RefCell<GraphDirectory>>>,
) -> f64 {
    match h {
        // BFS-equivalent admissibility: zero estimate is always ≤ true cost.
        AStarHeuristic::Uniform
        | AStarHeuristic::Latency
        | AStarHeuristic::Compliance
        | AStarHeuristic::TrustAttenuated
        | AStarHeuristic::PrerequisiteDistance => 0.0,
        AStarHeuristic::Hierarchy => {
            // Each hop changes depth by at most 1, so |Δdepth| is an
            // admissible lower bound on remaining hops.
            let Some(dir) = directory else { return 0.0 };
            let dir = dir.borrow();
            let cur_depth = dir.find_by_id(cur).map(|e| i64::from(e.depth));
            let tgt_depth = dir.find_by_id(target).map(|e| i64::from(e.depth));
            match (cur_depth, tgt_depth) {
                (Some(a), Some(b)) => (a - b).unsigned_abs() as f64,
                _ => 0.0,
            }
        }
    }
}

/// Wrapper for f64 priorities in [`tier2_astar_path`]'s
/// [`std::collections::BinaryHeap`]. BinaryHeap is max-heap; this
/// ordering flips so the smallest-f entry pops first. NaNs treat as
/// equal (no path scoring should produce NaN; we return `Equal` as a
/// safe fallback).
#[derive(PartialEq)]
struct OrderedF(f64);
impl Eq for OrderedF {}
impl PartialOrd for OrderedF {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedF {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .0
            .partial_cmp(&self.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// Tier 2 A* over the directed adjacency with a built-in
/// cost-from-policy heuristic. Returns the optimal path (inclusive of
/// `from` and `to`) or `None` if `to` isn't reachable.
fn tier2_astar_path(
    adj: &dyn DirectedAdj,
    snapshot: &GraphSnapshot,
    directory: Option<&Rc<RefCell<GraphDirectory>>>,
    from: &str,
    to: &str,
    heuristic: AStarHeuristic,
) -> Option<Vec<String>> {
    use std::collections::BinaryHeap;
    if !adj.has_node(from) || !adj.has_node(to) {
        return None;
    }

    let mut g_score: FxHashMap<String, f64> = FxHashMap::default();
    let mut prev: FxHashMap<String, String> = FxHashMap::default();
    let mut open: BinaryHeap<(OrderedF, String)> = BinaryHeap::new();
    g_score.insert(from.to_string(), 0.0);
    open.push((
        OrderedF(tier2_heuristic(heuristic, from, to, directory)),
        from.to_string(),
    ));

    while let Some((_, cur)) = open.pop() {
        if cur == to {
            // Reconstruct path.
            let mut path = vec![to.to_string()];
            let mut step = to.to_string();
            while let Some(p) = prev.get(&step) {
                path.push(p.clone());
                step = p.clone();
                if step == from {
                    break;
                }
            }
            path.reverse();
            return Some(path);
        }
        let cur_g = g_score.get(&cur).copied().unwrap_or(f64::INFINITY);
        for nbr in adj.neighbors(&cur) {
            let step = tier2_step_cost(heuristic, snapshot, &cur, nbr, directory);
            let tentative = cur_g + step;
            let known = g_score.get(nbr).copied().unwrap_or(f64::INFINITY);
            if tentative < known {
                prev.insert(nbr.clone(), cur.clone());
                g_score.insert(nbr.clone(), tentative);
                let f = tentative + tier2_heuristic(heuristic, nbr, to, directory);
                open.push((OrderedF(f), nbr.clone()));
            }
        }
    }
    None
}

// --- M12.3a-followon-E: host_tier2_* callbacks ---
//
// Each callback's first arg is the message-type string (Tier 2's
// scope); the LAST arg is a Bool indicating whether to use the
// directory-shortcut flavor of Tier 2 (every emit/accept pair is an
// edge regardless of Tier 1 edge presence) instead of the structural
// flavor (Tier 1 edge AND emit/accept match). Subsequent args mirror
// the Tier 1 (`host_graph_*`) callbacks. Errors collapse to the same
// neutral defaults as Tier 1 (null / empty / false) — JS callers see
// policy-derived "no path" the same way they see graph-structural
// "no path."

/// Decode the trailing `directoryShortcuts` Bool flag from a Tier 2
/// host_tier2_* call's args. JS-side `__makeTier2Proxy` always
/// appends this as the last positional arg.
fn tier2_directory_shortcuts_flag(args: &[HostValue]) -> bool {
    matches!(args.last(), Some(HostValue::Bool(true)))
}

/// Tier 2 nodeCount(msgType) — number of registered nodes that
/// participate in T (either emit or accept). Currently equals the
/// total registered count (the directed adjacency includes every
/// node, with empty `out` for non-emitters); a future variant could
/// filter to participants only if a fixture demands it.
pub(crate) fn build_host_tier2_node_count_callback() -> BoxCallback {
    Box::new(|args| {
        let _msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Number(0.0)),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Number(0.0));
        };
        let count = snapshot.manifests.borrow().len() as f64;
        Ok(HostValue::Number(count))
    })
}

pub(crate) fn build_host_tier2_has_node_callback() -> BoxCallback {
    Box::new(|args| {
        let _msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let id: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Bool(false));
        };
        let has = snapshot.manifests.borrow().contains_key(&id);
        Ok(HostValue::Bool(has))
    })
}

pub(crate) fn build_host_tier2_neighbors_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let id: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let manifests = snapshot.manifests.borrow();
        let graph = snapshot.graph.borrow();
        let dir_shortcuts = tier2_directory_shortcuts_flag(args);
        let adj = build_tier2_adj(&graph, &manifests, &msg_type, dir_shortcuts);
        Ok(HostValue::Array(
            adj.neighbors(&id)
                .iter()
                .cloned()
                .map(HostValue::String)
                .collect(),
        ))
    })
}

pub(crate) fn build_host_tier2_has_edge_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let u: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let v: String = match args.get(2) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Bool(false));
        };
        let manifests = snapshot.manifests.borrow();
        let graph = snapshot.graph.borrow();
        let dir_shortcuts = tier2_directory_shortcuts_flag(args);
        let adj = build_tier2_adj(&graph, &manifests, &msg_type, dir_shortcuts);
        Ok(HostValue::Bool(adj.has_edge(&u, &v)))
    })
}

pub(crate) fn build_host_tier2_bfs_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let depth_limit = match args.get(2) {
            Some(HostValue::Number(n)) if *n > 0.0 => Some(*n as usize),
            _ => None,
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let manifests = snapshot.manifests.borrow();
        let graph = snapshot.graph.borrow();
        let dir_shortcuts = tier2_directory_shortcuts_flag(args);
        let adj = build_tier2_adj(&graph, &manifests, &msg_type, dir_shortcuts);
        Ok(HostValue::Array(
            tier2_bfs(&adj, &from, depth_limit)
                .into_iter()
                .map(HostValue::String)
                .collect(),
        ))
    })
}

pub(crate) fn build_host_tier2_dfs_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let depth_limit = match args.get(2) {
            Some(HostValue::Number(n)) if *n > 0.0 => Some(*n as usize),
            _ => None,
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let manifests = snapshot.manifests.borrow();
        let graph = snapshot.graph.borrow();
        let dir_shortcuts = tier2_directory_shortcuts_flag(args);
        let adj = build_tier2_adj(&graph, &manifests, &msg_type, dir_shortcuts);
        Ok(HostValue::Array(
            tier2_dfs(&adj, &from, depth_limit)
                .into_iter()
                .map(HostValue::String)
                .collect(),
        ))
    })
}

pub(crate) fn build_host_tier2_shortest_path_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let to: String = match args.get(2) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Null);
        };
        let manifests = snapshot.manifests.borrow();
        let graph = snapshot.graph.borrow();
        let dir_shortcuts = tier2_directory_shortcuts_flag(args);
        let adj = build_tier2_adj(&graph, &manifests, &msg_type, dir_shortcuts);
        match tier2_shortest_path(&adj, &from, &to) {
            Some(path) => Ok(HostValue::Array(
                path.into_iter().map(HostValue::String).collect(),
            )),
            None => Ok(HostValue::Null),
        }
    })
}

/// Tier 2 topological sort over the directed adjacency for `msgType`.
/// Returns `Vec<String>` on success; `null` if the directed graph
/// contains a cycle (Kahn's algorithm leaves cyclic nodes unprocessed).
/// **Closes followon-B's `topologicalSort` deferral.**
pub(crate) fn build_host_tier2_topological_sort_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Null);
        };
        let manifests = snapshot.manifests.borrow();
        let graph = snapshot.graph.borrow();
        let dir_shortcuts = tier2_directory_shortcuts_flag(args);
        let adj = build_tier2_adj(&graph, &manifests, &msg_type, dir_shortcuts);
        match tier2_topological_sort(&adj) {
            Some(order) => Ok(HostValue::Array(
                order.into_iter().map(HostValue::String).collect(),
            )),
            None => Ok(HostValue::Null),
        }
    })
}

/// Tier 2 A* with a built-in cost-from-policy heuristic. Args:
/// `(msgType, from, to, heuristicName?)`. Heuristic defaults to
/// `'uniform'` (BFS-equivalent) when absent or unrecognised.
pub(crate) fn build_host_tier2_a_star_path_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let to: String = match args.get(2) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let heuristic: AStarHeuristic = match args.get(3) {
            Some(HostValue::String(s)) => {
                AStarHeuristic::from_str(s).unwrap_or(AStarHeuristic::Uniform)
            }
            _ => AStarHeuristic::Uniform,
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Null);
        };
        let manifests = snapshot.manifests.borrow();
        let graph = snapshot.graph.borrow();
        let dir_shortcuts = tier2_directory_shortcuts_flag(args);
        let adj = build_tier2_adj(&graph, &manifests, &msg_type, dir_shortcuts);
        let dir_handle = current_directory();
        match tier2_astar_path(&adj, &snapshot, dir_handle.as_ref(), &from, &to, heuristic) {
            Some(path) => Ok(HostValue::Array(
                path.into_iter().map(HostValue::String).collect(),
            )),
            None => Ok(HostValue::Null),
        }
    })
}

/// Tier 2 directed allPaths(msgType, from, to, maxDepth?,
/// directoryShortcuts). Returns Vec<Vec<String>> sorted by length.
pub(crate) fn build_host_tier2_all_paths_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let to: String = match args.get(2) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let max_depth = match args.get(3) {
            Some(HostValue::Number(n)) if *n > 0.0 => Some(*n as usize),
            _ => None,
        };
        let dir_shortcuts = tier2_directory_shortcuts_flag(args);
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let graph = snapshot.graph.borrow();
        let manifests = snapshot.manifests.borrow();
        let adj = build_tier2_adj(&graph, &manifests, &msg_type, dir_shortcuts);
        let paths = tier2_all_paths(&adj, &from, &to, max_depth);
        Ok(HostValue::Array(
            paths
                .into_iter()
                .map(|p| HostValue::Array(p.into_iter().map(HostValue::String).collect()))
                .collect(),
        ))
    })
}

/// Tier 2 directed connectedComponents(msgType, directoryShortcuts) —
/// strongly connected components via Tarjan's. Returns
/// Vec<Vec<String>>; each component sorted; outer Vec sorted by
/// smallest member.
pub(crate) fn build_host_tier2_connected_components_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let dir_shortcuts = tier2_directory_shortcuts_flag(args);
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let graph = snapshot.graph.borrow();
        let manifests = snapshot.manifests.borrow();
        let adj = build_tier2_adj(&graph, &manifests, &msg_type, dir_shortcuts);
        let components = tier2_strongly_connected_components(&adj);
        Ok(HostValue::Array(
            components
                .into_iter()
                .map(|c| HostValue::Array(c.into_iter().map(HostValue::String).collect()))
                .collect(),
        ))
    })
}

/// Tier 2 directed cycleDetection(msgType, directoryShortcuts) —
/// 3-color DFS over the directed adjacency. Returns
/// `{hasCycle, cycle: [[u,v], ...] | null}`.
pub(crate) fn build_host_tier2_cycle_detection_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => {
                return Ok(HostValue::Object(vec![
                    ("hasCycle".to_string(), HostValue::Bool(false)),
                    ("cycle".to_string(), HostValue::Null),
                ]));
            }
        };
        let dir_shortcuts = tier2_directory_shortcuts_flag(args);
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Object(vec![
                ("hasCycle".to_string(), HostValue::Bool(false)),
                ("cycle".to_string(), HostValue::Null),
            ]));
        };
        let graph = snapshot.graph.borrow();
        let manifests = snapshot.manifests.borrow();
        let adj = build_tier2_adj(&graph, &manifests, &msg_type, dir_shortcuts);
        let (has, cycle) = tier2_directed_cycle_detection(&adj);
        let cycle_value = match cycle {
            Some(edges) => HostValue::Array(
                edges
                    .into_iter()
                    .map(|(u, v)| {
                        HostValue::Array(vec![HostValue::String(u), HostValue::String(v)])
                    })
                    .collect(),
            ),
            None => HostValue::Null,
        };
        Ok(HostValue::Object(vec![
            ("hasCycle".to_string(), HostValue::Bool(has)),
            ("cycle".to_string(), cycle_value),
        ]))
    })
}

// Stub variants for the dry-mount path. JS-side `__graphQueryProxy`
// short-circuits each tier2 accessor on missing host callback, but
// keeping the registry shape uniform avoids `host_tier2_X is not
// defined` warnings if a dry-mount JS body exercises them.
pub(crate) fn build_host_tier2_node_count_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Number(0.0)))
}
pub(crate) fn build_host_tier2_has_node_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Bool(false)))
}
pub(crate) fn build_host_tier2_neighbors_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_tier2_has_edge_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Bool(false)))
}
pub(crate) fn build_host_tier2_bfs_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_tier2_dfs_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_tier2_shortest_path_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Null))
}
pub(crate) fn build_host_tier2_topological_sort_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Null))
}
pub(crate) fn build_host_tier2_a_star_path_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Null))
}
pub(crate) fn build_host_tier2_all_paths_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_tier2_connected_components_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_tier2_cycle_detection_stub_callback() -> BoxCallback {
    Box::new(|_| {
        Ok(HostValue::Object(vec![
            ("hasCycle".to_string(), HostValue::Bool(false)),
            ("cycle".to_string(), HostValue::Null),
        ]))
    })
}

// --- M12.3a-followon-F: useEdgeAcceptHandler + host_tier3_* callbacks ---
//
// Tier 3 = Tier 2 ∪ {(s, u) where target u's edge-accept predicate
// accepts (s, msg_type)}. Predicates are installed via
// `useEdgeAcceptHandler(fn(sourceId, msgType) -> bool)` from a node's
// component body. The predicate is consulted at query time in pure
// dry-run mode (no AcceptEdge mutation triggered).
//
// Each `host_tier3_*` callback's first arg is the message-type string;
// the LAST arg is the directory-shortcuts Bool (same convention as
// Tier 2). Subsequent args mirror the Tier 2 callbacks. The callbacks
// build a `Tier3Adj` from `snapshot.{graph, manifests,
// edge_accept_predicates}` and reuse the polymorphic `tier2_*`
// algorithm helpers via the [`DirectedAdj`] trait.

/// Build the `host_edge_accept_handler(token)` callback. JS-side
/// `useEdgeAcceptHandler(fn)` registers the predicate function under a
/// token (mirroring `useTopology` / `useMessage`) and dispatches the
/// token here. The closure registered with `core_use_edge_accept_handler`
/// re-enters JSC via `invoke_global("__invokeCallback", [token,
/// [sourceId, msgType]])` whenever the runtime asks "would this node
/// accept (s, msgType)?" — both AcceptEdge mutation paths and Tier 3
/// queries trigger it. Callback invocations happen outside the body's
/// `call_function` borrow (drain_pending_mutations / host_tier3_*
/// callbacks are themselves invoked from outside the active body), so
/// JSC re-entry is safe.
pub(crate) fn build_host_edge_accept_handler_callback() -> BoxCallback {
    Box::new(|args| {
        let token = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_edge_accept_handler: expected Number token, got {other:?}"
                )))
            }
        };

        let iso = current_iso().ok_or_else(|| {
            EngineError::Internal("host_edge_accept_handler: iso TLS unset".into())
        })?;

        // Predicate fires from two paths: (1) `drain_pending_mutations
        // ::AcceptEdge`, OUTSIDE any active engine borrow — safe; and
        // (2) `host_tier3_*` callbacks during querier body eval, which
        // run INSIDE an `engine.borrow_mut().call_function(...)` borrow
        // held by the outer body. The second path requires
        // `invoke_global_static` (free function, FFI-only) instead of
        // `engine.borrow_mut().invoke_global(...)` to avoid a Rust
        // RefCell double-borrow. JSC's underlying state is process-
        // global and JSObjectCallAsFunction is re-entrant, so the
        // bypass is sound — see the doc comment on
        // `invoke_global_static` in graphreact-engine-jsc.
        core_use_edge_accept_handler(move |source_id: &str, msg_type: &str| -> bool {
            let result = invoke_global_static(
                iso,
                "__invokeCallback",
                &[
                    HostValue::Number(token as f64),
                    HostValue::Array(vec![
                        HostValue::String(source_id.to_string()),
                        HostValue::String(msg_type.to_string()),
                    ]),
                ],
            );
            match result {
                Ok(envelope) => predicate_bool_from_envelope(envelope),
                Err(e) => {
                    eprintln!(
                        "[graphreact-cli] useEdgeAcceptHandler invocation failed: {e}"
                    );
                    // On invocation failure, fall back to default-accept
                    // so a buggy predicate doesn't silently sever Tier 3
                    // reachability.
                    true
                }
            }
        });
        Ok(HostValue::Null)
    })
}

/// Stub for the dry-mount path.
pub(crate) fn build_host_edge_accept_handler_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Decode the boolean verdict from an `__invokeCallback` envelope. The
/// envelope shape is `{kind: 'value', value: <user return>}`; we treat
/// any non-Bool / non-Number / non-true value as `false` (the user is
/// expected to return a boolean — a non-boolean is a programming
/// error, surfaced by reading as "veto" rather than silently
/// default-accepting).
fn predicate_bool_from_envelope(envelope: HostValue) -> bool {
    let HostValue::Object(pairs) = envelope else {
        return false;
    };
    for (k, v) in pairs {
        if k == "value" {
            return match v {
                HostValue::Bool(b) => b,
                HostValue::Number(n) => n != 0.0,
                _ => false,
            };
        }
    }
    false
}

/// Decode the trailing `directoryShortcuts` Bool flag from a Tier 3
/// host_tier3_* call's args. Same shape as Tier 2 — JS-side
/// `__makeTier3Proxy` always appends this as the last positional arg.
fn tier3_directory_shortcuts_flag(args: &[HostValue]) -> bool {
    matches!(args.last(), Some(HostValue::Bool(true)))
}

/// Helper: build the Tier 3 adjacency from the active TLS snapshot.
/// Returns `None` if the snapshot isn't installed (not inside an
/// `EngineGuard::with_directory`). Borrows graph + manifests +
/// predicates only for the duration of the build.
fn build_tier3_adj_from_snapshot(
    snapshot: &GraphSnapshot,
    msg_type: &str,
    include_directory_shortcuts: bool,
) -> Tier3Adj {
    let graph = snapshot.graph.borrow();
    let manifests = snapshot.manifests.borrow();
    let predicates = snapshot.edge_accept_predicates.borrow();
    build_tier3_adj(
        &graph,
        &manifests,
        &predicates,
        msg_type,
        include_directory_shortcuts,
    )
}

pub(crate) fn build_host_tier3_node_count_callback() -> BoxCallback {
    Box::new(|args| {
        let _msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Number(0.0)),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Number(0.0));
        };
        let count = snapshot.manifests.borrow().len() as f64;
        Ok(HostValue::Number(count))
    })
}

pub(crate) fn build_host_tier3_has_node_callback() -> BoxCallback {
    Box::new(|args| {
        let _msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let id: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Bool(false));
        };
        let has = snapshot.manifests.borrow().contains_key(&id);
        Ok(HostValue::Bool(has))
    })
}

pub(crate) fn build_host_tier3_neighbors_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let id: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let dir_shortcuts = tier3_directory_shortcuts_flag(args);
        let adj = build_tier3_adj_from_snapshot(&snapshot, &msg_type, dir_shortcuts);
        Ok(HostValue::Array(
            adj.neighbors(&id)
                .iter()
                .cloned()
                .map(HostValue::String)
                .collect(),
        ))
    })
}

pub(crate) fn build_host_tier3_has_edge_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let u: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let v: String = match args.get(2) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Bool(false));
        };
        let dir_shortcuts = tier3_directory_shortcuts_flag(args);
        let adj = build_tier3_adj_from_snapshot(&snapshot, &msg_type, dir_shortcuts);
        Ok(HostValue::Bool(adj.has_edge(&u, &v)))
    })
}

pub(crate) fn build_host_tier3_bfs_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let depth_limit = match args.get(2) {
            Some(HostValue::Number(n)) if *n > 0.0 => Some(*n as usize),
            _ => None,
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let dir_shortcuts = tier3_directory_shortcuts_flag(args);
        let adj = build_tier3_adj_from_snapshot(&snapshot, &msg_type, dir_shortcuts);
        Ok(HostValue::Array(
            tier2_bfs(&adj, &from, depth_limit)
                .into_iter()
                .map(HostValue::String)
                .collect(),
        ))
    })
}

pub(crate) fn build_host_tier3_dfs_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let depth_limit = match args.get(2) {
            Some(HostValue::Number(n)) if *n > 0.0 => Some(*n as usize),
            _ => None,
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let dir_shortcuts = tier3_directory_shortcuts_flag(args);
        let adj = build_tier3_adj_from_snapshot(&snapshot, &msg_type, dir_shortcuts);
        Ok(HostValue::Array(
            tier2_dfs(&adj, &from, depth_limit)
                .into_iter()
                .map(HostValue::String)
                .collect(),
        ))
    })
}

pub(crate) fn build_host_tier3_shortest_path_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let to: String = match args.get(2) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Null);
        };
        let dir_shortcuts = tier3_directory_shortcuts_flag(args);
        let adj = build_tier3_adj_from_snapshot(&snapshot, &msg_type, dir_shortcuts);
        match tier2_shortest_path(&adj, &from, &to) {
            Some(path) => Ok(HostValue::Array(
                path.into_iter().map(HostValue::String).collect(),
            )),
            None => Ok(HostValue::Null),
        }
    })
}

/// Tier 3 topological sort over the directed adjacency for `msgType`.
/// Mirrors Tier 2 but over the negotiation-pruned reachability set.
pub(crate) fn build_host_tier3_topological_sort_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Null);
        };
        let dir_shortcuts = tier3_directory_shortcuts_flag(args);
        let adj = build_tier3_adj_from_snapshot(&snapshot, &msg_type, dir_shortcuts);
        match tier2_topological_sort(&adj) {
            Some(order) => Ok(HostValue::Array(
                order.into_iter().map(HostValue::String).collect(),
            )),
            None => Ok(HostValue::Null),
        }
    })
}

/// Tier 3 A* with a built-in cost-from-policy heuristic. Args:
/// `(msgType, from, to, heuristicName?)`. Same heuristic set as Tier 2;
/// the only difference is the underlying directed adjacency.
pub(crate) fn build_host_tier3_a_star_path_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let to: String = match args.get(2) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        let heuristic: AStarHeuristic = match args.get(3) {
            Some(HostValue::String(s)) => {
                AStarHeuristic::from_str(s).unwrap_or(AStarHeuristic::Uniform)
            }
            _ => AStarHeuristic::Uniform,
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Null);
        };
        let dir_shortcuts = tier3_directory_shortcuts_flag(args);
        let adj = build_tier3_adj_from_snapshot(&snapshot, &msg_type, dir_shortcuts);
        let dir_handle = current_directory();
        match tier2_astar_path(&adj, &snapshot, dir_handle.as_ref(), &from, &to, heuristic) {
            Some(path) => Ok(HostValue::Array(
                path.into_iter().map(HostValue::String).collect(),
            )),
            None => Ok(HostValue::Null),
        }
    })
}

/// Tier 3 directed allPaths(msgType, from, to, maxDepth?,
/// directoryShortcuts) over the negotiation-pruned reachability.
pub(crate) fn build_host_tier3_all_paths_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let to: String = match args.get(2) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let max_depth = match args.get(3) {
            Some(HostValue::Number(n)) if *n > 0.0 => Some(*n as usize),
            _ => None,
        };
        let dir_shortcuts = tier3_directory_shortcuts_flag(args);
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let adj = build_tier3_adj_from_snapshot(&snapshot, &msg_type, dir_shortcuts);
        let paths = tier2_all_paths(&adj, &from, &to, max_depth);
        Ok(HostValue::Array(
            paths
                .into_iter()
                .map(|p| HostValue::Array(p.into_iter().map(HostValue::String).collect()))
                .collect(),
        ))
    })
}

/// Tier 3 directed connectedComponents(msgType, directoryShortcuts) —
/// SCC over the negotiation-pruned adjacency.
pub(crate) fn build_host_tier3_connected_components_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let dir_shortcuts = tier3_directory_shortcuts_flag(args);
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let adj = build_tier3_adj_from_snapshot(&snapshot, &msg_type, dir_shortcuts);
        let components = tier2_strongly_connected_components(&adj);
        Ok(HostValue::Array(
            components
                .into_iter()
                .map(|c| HostValue::Array(c.into_iter().map(HostValue::String).collect()))
                .collect(),
        ))
    })
}

/// Tier 3 directed cycleDetection(msgType, directoryShortcuts).
pub(crate) fn build_host_tier3_cycle_detection_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => {
                return Ok(HostValue::Object(vec![
                    ("hasCycle".to_string(), HostValue::Bool(false)),
                    ("cycle".to_string(), HostValue::Null),
                ]));
            }
        };
        let dir_shortcuts = tier3_directory_shortcuts_flag(args);
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Object(vec![
                ("hasCycle".to_string(), HostValue::Bool(false)),
                ("cycle".to_string(), HostValue::Null),
            ]));
        };
        let adj = build_tier3_adj_from_snapshot(&snapshot, &msg_type, dir_shortcuts);
        let (has, cycle) = tier2_directed_cycle_detection(&adj);
        let cycle_value = match cycle {
            Some(edges) => HostValue::Array(
                edges
                    .into_iter()
                    .map(|(u, v)| {
                        HostValue::Array(vec![HostValue::String(u), HostValue::String(v)])
                    })
                    .collect(),
            ),
            None => HostValue::Null,
        };
        Ok(HostValue::Object(vec![
            ("hasCycle".to_string(), HostValue::Bool(has)),
            ("cycle".to_string(), cycle_value),
        ]))
    })
}

// Tier 3 stub variants for the dry-mount path. Same neutral defaults
// as Tier 2 stubs — JS-side proxy short-circuits on missing host
// callback, but the registry-shape uniformity avoids `host_tier3_X is
// not defined` warnings.
pub(crate) fn build_host_tier3_node_count_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Number(0.0)))
}
pub(crate) fn build_host_tier3_has_node_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Bool(false)))
}
pub(crate) fn build_host_tier3_neighbors_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_tier3_has_edge_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Bool(false)))
}
pub(crate) fn build_host_tier3_bfs_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_tier3_dfs_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_tier3_shortest_path_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Null))
}
pub(crate) fn build_host_tier3_topological_sort_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Null))
}
pub(crate) fn build_host_tier3_a_star_path_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Null))
}
pub(crate) fn build_host_tier3_all_paths_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_tier3_connected_components_stub_callback() -> BoxCallback {
    Box::new(|_| Ok(HostValue::Array(vec![])))
}
pub(crate) fn build_host_tier3_cycle_detection_stub_callback() -> BoxCallback {
    Box::new(|_| {
        Ok(HostValue::Object(vec![
            ("hasCycle".to_string(), HostValue::Bool(false)),
            ("cycle".to_string(), HostValue::Null),
        ]))
    })
}

// --- Phase 3e-ε: useSource + timer/delay event source factories ---

/// Build the `host_use_source(descriptor, deps)` callback. JS-side
/// `useSource(factoryFn, deps)` invokes `factoryFn()` to produce a
/// descriptor object `{kind, ...}` and dispatches it here. We decode
/// the kind to construct the right Rust-native [`Arc<dyn EventSource>`]
/// (timer / delay from graphreact-core; httpSource / watchSource
/// deferred — descriptors fall through to a no-op closure).
///
/// `core_use_source` registers the factory + deps as a hook slot.
/// The scheduler invokes the factory once on first mount (or on
/// deps-change) and `start()`s the resulting source. Events flow
/// cross-thread via the scheduler's `external_inbox` and reach the
/// active node as messages on the next `process_batch` — caught by
/// `useMessage(type, handler)`.
pub(crate) fn build_host_use_source_callback() -> BoxCallback {
    Box::new(|args| {
        let descriptor = match args.first() {
            Some(HostValue::Object(pairs)) => pairs.clone(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_use_source: expected Object descriptor, got {other:?}"
                )))
            }
        };
        let deps: Vec<Value> = match args.get(1) {
            Some(HostValue::Array(arr)) => arr.iter().cloned().map(HostValue::into_json).collect(),
            None | Some(HostValue::Null) => Vec::new(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_use_source: expected Array deps, got {other:?}"
                )))
            }
        };

        let mut kind: Option<String> = None;
        let mut interval_ms: u64 = 0;
        let mut delay_ms: u64 = 0;
        let mut http_port: u16 = 0;
        let mut watch_path: String = String::new();
        for (k, v) in descriptor {
            match (k.as_str(), v) {
                ("kind", HostValue::String(s)) => kind = Some(s),
                ("intervalMs", HostValue::Number(n)) => interval_ms = n as u64,
                ("delayMs", HostValue::Number(n)) => delay_ms = n as u64,
                ("opts", HostValue::Object(opts_pairs)) => {
                    for (ok, ov) in opts_pairs {
                        match (ok.as_str(), ov) {
                            ("port", HostValue::Number(n)) => {
                                http_port = (n as u64).min(u64::from(u16::MAX)) as u16;
                            }
                            ("path", HostValue::String(s)) => watch_path = s,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        let kind_str = kind.unwrap_or_default();
        let factory: Box<dyn FnOnce() -> std::sync::Arc<dyn EventSource>> = match kind_str.as_str() {
            "timer" => Box::new(move || core_timer_source(interval_ms)),
            "delay" => Box::new(move || core_delay_source(delay_ms)),
            // M12.3a-followon-A: http + watch firing now real. Both port
            // forward graphreact-napi/sources.rs into
            // crates/graphreact-cli/src/sources.rs (tokio::net + notify
            // deps now in graphreact-cli's Cargo.toml). httpSource
            // requires the binary to be inside a tokio runtime context
            // (grx main() installs one); watchSource uses notify's own
            // thread and is runtime-agnostic.
            "http" => Box::new(move || crate::sources::http_source(http_port)),
            "watch" => Box::new(move || crate::sources::watch_source(watch_path)),
            _ => Box::new(|| {
                core_timer_source(u64::MAX) // never fires; unknown-kind placeholder
            }),
        };

        core_use_source(move || factory(), deps);
        Ok(HostValue::Null)
    })
}

/// Stub for the dry-mount path.
pub(crate) fn build_host_use_source_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

// --- Phase 3e-ζ: supervision hooks ---

/// Build the `host_monitor(targetId) → {id} | null` callback. JS-side
/// `useMonitor(targetId)` returns the result. The active node will
/// receive `__DOWN__` system messages when target unmounts —
/// `useMessage('__DOWN__', ...)` catches them.
pub(crate) fn build_host_monitor_callback() -> BoxCallback {
    Box::new(|args| {
        let target = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        match core_use_monitor(&target) {
            Some(mref) => Ok(HostValue::Object(vec![(
                "id".to_string(),
                HostValue::Number(mref.0 as f64),
            )])),
            None => Ok(HostValue::Null),
        }
    })
}

/// Build the `host_demonitor(id)` callback.
pub(crate) fn build_host_demonitor_callback() -> BoxCallback {
    Box::new(|args| {
        let id = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            _ => return Ok(HostValue::Null),
        };
        core_use_demonitor(CoreMonitorRef(id));
        Ok(HostValue::Null)
    })
}

/// Build the `host_link(targetId) → bool` callback.
pub(crate) fn build_host_link_callback() -> BoxCallback {
    Box::new(|args| {
        let target = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        Ok(HostValue::Bool(core_use_link(&target)))
    })
}

/// Build the `host_unlink(targetId)` callback.
pub(crate) fn build_host_unlink_callback() -> BoxCallback {
    Box::new(|args| {
        let target = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        core_use_unlink(&target);
        Ok(HostValue::Null)
    })
}

/// Build the `host_group_join(group)` callback.
pub(crate) fn build_host_group_join_callback() -> BoxCallback {
    Box::new(|args| {
        let group = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        core_use_group_join(&group);
        Ok(HostValue::Null)
    })
}

/// Build the `host_group_leave(group)` callback.
pub(crate) fn build_host_group_leave_callback() -> BoxCallback {
    Box::new(|args| {
        let group = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        core_use_group_leave(&group);
        Ok(HostValue::Null)
    })
}

/// Stubs for the dry-mount path.
pub(crate) fn build_host_monitor_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}
pub(crate) fn build_host_demonitor_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}
pub(crate) fn build_host_link_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Bool(false)))
}
pub(crate) fn build_host_unlink_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}
pub(crate) fn build_host_group_join_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}
pub(crate) fn build_host_group_leave_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Decode a JS-shape compose spec into a graphreact-core
/// [`ComposeChildSpec`]. The JS shape is
/// `{id, componentToken, props, connectTo}` — the JS prelude
/// converts the user-facing `component` function into a token via
/// `__registerCallback` before dispatching, so by the time we see it
/// here the function is already in the JS-side registry.
///
/// The resulting `ComposeChildSpec::definition` is a
/// [`NodeDefinition`] whose component closure re-enters JSC via
/// `invoke_global("__invokeCallback", [token, [ctx]])` on every
/// render. The closure captures engine + iso through Rc clones — the
/// SpawnChild / Compose mutations that consume this spec are applied
/// at the start of the next cascade round (no outstanding engine
/// borrow at that point).
fn build_compose_spec_from_host_value(value: HostValue) -> Result<ComposeChildSpec, EngineError> {
    let HostValue::Object(pairs) = value else {
        return Err(EngineError::Internal(format!(
            "expected ComposeChildSpec object, got {value:?}"
        )));
    };

    let mut id: Option<String> = None;
    let mut component_token: Option<u64> = None;
    let mut props: Attrs = Attrs::new();
    let mut connect_to: Vec<String> = Vec::new();

    for (k, v) in pairs {
        match (k.as_str(), v) {
            ("id", HostValue::String(s)) => id = Some(s),
            ("componentToken", HostValue::Number(n)) => component_token = Some(n as u64),
            ("props", HostValue::Object(prop_pairs)) => {
                for (pk, pv) in prop_pairs {
                    props.insert(pk, pv.into_json());
                }
            }
            ("connectTo", HostValue::Array(arr)) => {
                connect_to = arr
                    .into_iter()
                    .filter_map(|x| match x {
                        HostValue::String(s) => Some(s),
                        _ => None,
                    })
                    .collect();
            }
            _ => {}
        }
    }

    let id = id.ok_or_else(|| {
        EngineError::Internal("ComposeChildSpec: missing `id`".into())
    })?;
    let component_token = component_token.ok_or_else(|| {
        EngineError::Internal("ComposeChildSpec: missing `componentToken`".into())
    })?;

    let engine = current_engine().ok_or_else(|| {
        EngineError::Internal("compose spec: engine TLS unset".into())
    })?;
    let iso = current_iso().ok_or_else(|| {
        EngineError::Internal("compose spec: iso TLS unset".into())
    })?;

    let definition_name = format!("JsChild::{id}");
    let definition = NodeDefinition::new(
        definition_name,
        move |ctx: &NodeContext| -> PermissionManifest {
            let _guard = ComponentEvalGuard::new(&ctx.id);
            let host_ctx = build_host_ctx(ctx);
            let envelope = engine.borrow_mut().invoke_global(
                iso,
                "__invokeCallback",
                &[
                    HostValue::Number(component_token as f64),
                    HostValue::Array(vec![host_ctx]),
                ],
            );
            match envelope {
                Ok(env) => manifest_from_invoke_envelope(env),
                Err(e) => {
                    eprintln!(
                        "[graphreact-cli] dynamically-spawned component invocation failed: {e}"
                    );
                    PermissionManifest::default()
                }
            }
        },
    );

    Ok(ComposeChildSpec {
        id,
        definition,
        props,
        connect_to,
    })
}

/// Extract a manifest from an `__invokeCallback` envelope. The
/// envelope shape is `{kind, value? | token? | message?}`; we only
/// care about `kind: 'value'` for component bodies (they return
/// manifest objects, not cleanup functions).
fn manifest_from_invoke_envelope(envelope: HostValue) -> PermissionManifest {
    let HostValue::Object(pairs) = envelope else {
        return PermissionManifest::default();
    };
    for (k, v) in pairs {
        if k == "value" {
            return host_value_to_manifest(v);
        }
    }
    PermissionManifest::default()
}

/// Decode a JS-shape `TopologyFilter` from the host_topology call's
/// second arg. JS shape: `{kinds?: string[], neighborsOnly?: bool}`.
fn parse_topology_filter(arg: Option<&HostValue>) -> Option<TopologyFilterOptions> {
    let HostValue::Object(pairs) = arg? else {
        return None;
    };
    let mut kinds: Option<Vec<TopologyEventKind>> = None;
    let mut neighbors_only = false;
    for (k, v) in pairs {
        match (k.as_str(), v) {
            ("kinds", HostValue::Array(arr)) => {
                let parsed: Vec<TopologyEventKind> = arr
                    .iter()
                    .filter_map(|x| match x {
                        HostValue::String(s) => match s.as_str() {
                            "node-added" | "NodeAdded" => Some(TopologyEventKind::NodeAdded),
                            "node-removed" | "NodeRemoved" => Some(TopologyEventKind::NodeRemoved),
                            "edge-added" | "EdgeAdded" => Some(TopologyEventKind::EdgeAdded),
                            "edge-removed" | "EdgeRemoved" => Some(TopologyEventKind::EdgeRemoved),
                            "edge-updated" | "EdgeUpdated" => Some(TopologyEventKind::EdgeUpdated),
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect();
                kinds = Some(parsed);
            }
            ("neighborsOnly", HostValue::Bool(b)) => neighbors_only = *b,
            _ => {}
        }
    }
    Some(TopologyFilterOptions {
        kinds,
        neighbors_only,
    })
}

/// Marshal a [`TopologyEvent`] into the JS-shape envelope mirroring
/// NAPI's `invoke_topology_handler` (`{kind, nodeId? | u, v, key?}`).
fn topology_event_to_host_value(event: &TopologyEvent) -> HostValue {
    match event {
        TopologyEvent::NodeAdded { node_id } => HostValue::Object(vec![
            ("kind".to_string(), HostValue::String("node-added".into())),
            ("nodeId".to_string(), HostValue::String(node_id.clone())),
        ]),
        TopologyEvent::NodeRemoved { node_id } => HostValue::Object(vec![
            (
                "kind".to_string(),
                HostValue::String("node-removed".into()),
            ),
            ("nodeId".to_string(), HostValue::String(node_id.clone())),
        ]),
        TopologyEvent::EdgeAdded { u, v, key } => HostValue::Object(vec![
            ("kind".to_string(), HostValue::String("edge-added".into())),
            ("u".to_string(), HostValue::String(u.clone())),
            ("v".to_string(), HostValue::String(v.clone())),
            ("key".to_string(), HostValue::String(key.clone())),
        ]),
        TopologyEvent::EdgeRemoved { u, v, key } => HostValue::Object(vec![
            (
                "kind".to_string(),
                HostValue::String("edge-removed".into()),
            ),
            ("u".to_string(), HostValue::String(u.clone())),
            ("v".to_string(), HostValue::String(v.clone())),
            ("key".to_string(), HostValue::String(key.clone())),
        ]),
        TopologyEvent::EdgeUpdated { u, v, key } => HostValue::Object(vec![
            (
                "kind".to_string(),
                HostValue::String("edge-updated".into()),
            ),
            ("u".to_string(), HostValue::String(u.clone())),
            ("v".to_string(), HostValue::String(v.clone())),
            ("key".to_string(), HostValue::String(key.clone())),
        ]),
    }
}

/// Decode an `EmitOptions` from the `opts` arg passed by JS-side
/// `useEmit`'s returned closure. Accepts the JS shape
/// `{to?: string[], ttl?: number, edgeKey?: string, direct?: string}`
/// and silently ignores unknown keys. `null` / missing → defaults.
fn parse_emit_options(arg: Option<&HostValue>) -> EmitOptions {
    let Some(HostValue::Object(pairs)) = arg else {
        return EmitOptions::default();
    };
    let mut opts = EmitOptions::default();
    for (k, v) in pairs {
        match (k.as_str(), v) {
            ("to", HostValue::Array(arr)) => {
                opts.to = Some(
                    arr.iter()
                        .filter_map(|x| {
                            if let HostValue::String(s) = x {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .collect(),
                );
            }
            ("ttl", HostValue::Number(n)) => opts.ttl = Some(*n as u32),
            ("edgeKey", HostValue::String(s)) => opts.edge_key = Some(s.clone()),
            ("direct", HostValue::String(s)) => opts.direct = Some(s.clone()),
            _ => {}
        }
    }
    opts
}

/// Stub `host_state` callback for callers that don't drive a
/// [`Scheduler`] — e.g. the legacy [`crate::runtime::dry_mount_jsc`]
/// path which still uses the eval/call shape from phase 3a. Returns
/// `{id: 0, value: initial}` so JS-side `useState` resolves to
/// `[initial, no-op-setter]`. State preservation requires
/// [`mount_with_scheduler`].
pub(crate) fn build_host_state_stub_callback() -> BoxCallback {
    Box::new(|args| {
        let initial: Value = args
            .first()
            .cloned()
            .map(HostValue::into_json)
            .unwrap_or(Value::Null);
        Ok(HostValue::Object(vec![
            ("id".into(), HostValue::Number(0.0)),
            ("value".into(), HostValue::from_json(initial)),
        ]))
    })
}

/// Stub `host_state_set` callback for the legacy path. No-op.
pub(crate) fn build_host_state_set_stub_callback() -> BoxCallback {
    Box::new(|_args| Ok(HostValue::Null))
}

/// Test-only: write a signal's value by id without going through
/// JSC. Used by phase 3b tests to externally trigger a re-render.
#[cfg(test)]
pub(crate) fn set_signal(id: u64, value: Value) {
    SIGNAL_REGISTRY.with(|reg| {
        if let Some(signal) = reg.borrow().get(&id) {
            signal.set(value);
        }
    });
}

/// Test-only: read the signal id minted for `(node_id, hook_pos)`.
/// Mirrors the helper in `pyruntime`. **M15 Phase 8.**
#[cfg(test)]
pub(crate) fn signal_id_for(node_id: &str, hook_pos: usize) -> Option<u64> {
    SIGNAL_BY_HOOK_POS.with(|map| map.borrow().get(&(node_id.to_string(), hook_pos)).copied())
}

/// **M15 Phase 9.** Clear all per-`(node_id, hook_pos)` thread-local
/// entries for a node that just unmounted. Runs synchronously from
/// the topology listener so that a subsequent re-mount of the same
/// node id (M15 Incompatible reload's drain plus remount path)
/// doesn't pick up stale `Signal` handles whose underlying
/// `GenerationalBox` was dropped on unmount.
///
/// Without this sweep, M15's incompat reload of a JSC node would
/// panic on the new body's first `useState` call —
/// `SIGNAL_BY_HOOK_POS["c1", 0]` would still point at the dead
/// signal id, `SIGNAL_REGISTRY[id].get()` would call
/// `GenerationalBox::read().unwrap()` on a dropped box, and the
/// resulting panic would propagate across the JSC FFI boundary
/// (SIGABRT under release).
///
/// Counterpart helper to [`release_tokens_for_node`] — that function
/// re-enters JSC to release JS-side `__hostCallbacks` tokens (deferred
/// to `process_batch_async` because it needs `EngineGuard`); this one
/// only mutates Rust-side thread-locals so it's safe to run inline.
pub(crate) fn clear_node_local_state(node_id: &str) {
    let removed_ids: Vec<u64> = SIGNAL_BY_HOOK_POS.with(|map| {
        let mut m = map.borrow_mut();
        let to_remove: Vec<((String, usize), u64)> = m
            .iter()
            .filter(|((id, _), _)| id == node_id)
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        for (k, _) in &to_remove {
            m.remove(k);
        }
        to_remove.into_iter().map(|(_, v)| v).collect()
    });
    SIGNAL_REGISTRY.with(|reg| {
        let mut r = reg.borrow_mut();
        for id in &removed_ids {
            r.remove(id);
        }
    });
    MEMO_REGISTRY.with(|reg| {
        reg.borrow_mut().retain(|(id, _), _| id != node_id);
    });
}

/// Crate-wide JSC test serializer. JSC has process-global engine
/// state; every test that touches `JscEngine::initialize` /
/// `create_isolate` / `eval_component` etc. must hold this guard.
///
/// **Why crate-wide and not module-local**: tests run in parallel by
/// default. Module-local locks (e.g., the `engine_lock` previously
/// nested inside `mod tests`) only serialize tests within their own
/// module — concurrent tests in `jsruntime::tests` and
/// `hotreload_adapter::tests` would race on the global JSC state and
/// produce SIGSEGV. Hoisting this to `pub(crate)` lets every JSC-
/// touching test in the crate share one mutex. **M15 Phase 8.**
#[cfg(test)]
pub(crate) fn engine_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Test-only: reset thread-local state between tests. Required
/// because `SIGNAL_BY_HOOK_POS` and [`MEMO_REGISTRY`] both key by
/// `node_id` (a String) and tests reusing the same node id across
/// runs would alias entries.
#[cfg(test)]
pub(crate) fn reset_global_state() {
    SIGNAL_REGISTRY.with(|reg| reg.borrow_mut().clear());
    SIGNAL_BY_HOOK_POS.with(|map| map.borrow_mut().clear());
    NEXT_SIGNAL_ID.with(|c| c.set(1));
    MEMO_REGISTRY.with(|reg| reg.borrow_mut().clear());
    PENDING_MEMO_KEY.with(|p| *p.borrow_mut() = None);
    NODE_TOKENS.with(|m| m.borrow_mut().clear());
}

// ---------------------------------------------------------------------
// JS-backed NodeDefinition
// ---------------------------------------------------------------------

/// Build a [`NodeDefinition`] whose component closure dispatches into
/// JSC. The closure captures a shared handle to the engine
/// (`Rc<RefCell<JscEngine>>`), the isolate, and the per-component
/// function handle returned by `eval_component`.
///
/// On every evaluation:
/// 1. [`ComponentEvalGuard`] resets the JS-side hook index + stamps
///    the current node id.
/// 2. The per-instance `ctx` is built from [`NodeContext`].
/// 3. JSC's `call_function(component, [ctx])` runs the body — the
///    body's hook calls (e.g. `useState`) route through host
///    callbacks registered on the engine.
/// 4. The returned manifest object is converted to
///    [`PermissionManifest`].
/// 5. The guard drops on closure exit.
pub(crate) fn build_js_node_definition(
    name: String,
    engine: Rc<RefCell<JscEngine>>,
    iso: IsolateHandle,
    fn_handle: FunctionHandle,
) -> NodeDefinition {
    NodeDefinition::new(name, move |ctx: &NodeContext| -> PermissionManifest {
        let _guard = ComponentEvalGuard::new(&ctx.id);
        let host_ctx = build_host_ctx(ctx);
        let result = engine
            .borrow_mut()
            .call_function(iso, fn_handle, &[host_ctx]);
        match result {
            Ok(manifest) => host_value_to_manifest(manifest),
            Err(e) => {
                eprintln!("[graphreact-cli] JSC component invocation failed: {e}");
                PermissionManifest::default()
            }
        }
    })
}

/// **M15 Phase 8.** Compile `source` as a fresh component body on the
/// existing isolate and return a [`NodeDefinition`] that dispatches to
/// it. Reuses the same `engine` plus `iso` so per-isolate state — host
/// callback registrations, the JS-side `__hostCallbacks` token
/// registry, the Rust-side `MEMO_REGISTRY` cache (keyed on `(node_id,
/// hook_pos)`) and the `SIGNAL_REGISTRY` slots — survives the swap.
///
/// Hot-reload's [`crate::hotreload_adapter::JscReloadAdapter`] calls
/// this from `EngineAdapter::build_definition`; the orchestrator then
/// hands the result to `Scheduler::replace_definition`. Compatible
/// reloads preserve `useState` because the new closure registers its
/// hooks in the same order against the same per-`(node_id, hook_pos)`
/// slot table that `core_use_state` resolves to.
///
/// Known leak: each call mints a fresh JSC `FunctionHandle`. The
/// previous handle stays `JSValueProtect`'d in the isolate's function
/// table — bounded by reload count, parallel in shape to the
/// `__hostCallbacks` token leak documented in M12.3a-followon-D.
/// Hardenable in M-Prod.
pub fn build_jsc_node_definition_from_source(
    engine: &Rc<RefCell<JscEngine>>,
    iso: IsolateHandle,
    component: &str,
    source: &str,
) -> Result<NodeDefinition, EngineError> {
    let fn_handle = engine.borrow_mut().eval_component(iso, component, source)?;
    Ok(build_js_node_definition(
        component.to_string(),
        Rc::clone(engine),
        iso,
        fn_handle,
    ))
}

/// Build the per-instance JSC `ctx` from a [`NodeContext`]. Mirrors
/// `runtime::build_node_ctx` but reads from the lifecycle-supplied
/// context (id, props, neighbors) rather than the IR node directly.
fn build_host_ctx(ctx: &NodeContext) -> HostValue {
    let mut props_pairs: Vec<(String, HostValue)> = Vec::new();
    for (k, v) in &ctx.props {
        // Hide IR-injected bookkeeping keys; phase 3a's runtime path
        // strips these and we mirror that for parity.
        if matches!(k.as_str(), "component" | "_depth" | "_parentPath") {
            continue;
        }
        props_pairs.push((k.clone(), HostValue::from_json(v.clone())));
    }

    let neighbors: Vec<HostValue> = ctx
        .neighbors
        .iter()
        .cloned()
        .map(HostValue::String)
        .collect();

    HostValue::Object(vec![
        ("id".into(), HostValue::String(ctx.id.clone())),
        ("props".into(), HostValue::Object(props_pairs)),
        ("neighbors".into(), HostValue::Array(neighbors)),
        // Stub directory + mutation handles. Phase 3c+ will populate
        // these as host callbacks routed through GraphDirectory and
        // the scheduler's pending mutation queue.
        (
            "directory".into(),
            HostValue::Object(vec![("_stub".into(), HostValue::Bool(true))]),
        ),
    ])
}

/// Convert a JS-returned manifest object into a graphreact-core
/// [`PermissionManifest`]. Only `accepts` and `emits` are populated
/// in phase 3b — channel policies and per-neighbor overrides land in
/// phase 3c+ alongside the corresponding hook wiring.
fn host_value_to_manifest(hv: HostValue) -> PermissionManifest {
    let HostValue::Object(pairs) = hv else {
        return PermissionManifest::default();
    };
    let mut accepts: Vec<String> = Vec::new();
    let mut emits: Vec<String> = Vec::new();
    let mut accepts_external: Vec<String> = Vec::new();
    let mut accepts_external_call: Vec<graphreact_core::ExternalCall> = Vec::new();
    for (k, v) in pairs {
        match (k.as_str(), v) {
            ("accepts", HostValue::Array(arr)) => {
                accepts = arr
                    .into_iter()
                    .filter_map(|x| {
                        if let HostValue::String(s) = x {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .collect();
            }
            ("emits", HostValue::Array(arr)) => {
                emits = arr
                    .into_iter()
                    .filter_map(|x| {
                        if let HostValue::String(s) = x {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .collect();
            }
            // **M-Ingress.** Optional component-declared list of message
            // types accepted from external HTTP injection (POST
            // /v1/nodes/:id/messages). Default-deny semantics — absent
            // or empty means the node refuses every external type.
            ("accepts_external", HostValue::Array(arr)) => {
                accepts_external = arr
                    .into_iter()
                    .filter_map(|x| {
                        if let HostValue::String(s) = x {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .collect();
            }
            // **M-Ingress phase 2.** Optional component-declared list of
            // call-shaped external acceptance — `[{ request_type, reply_type }, ...]`.
            // Powers `POST /v1/nodes/:id/call` with auto-correlation:
            // the runtime tags the first outgoing emit whose `type`
            // matches a `reply_type` here with the inbound `correlation_id`.
            // Entries missing either field are silently skipped (the
            // pair is meaningless without both halves).
            ("accepts_external_call", HostValue::Array(arr)) => {
                accepts_external_call = arr
                    .into_iter()
                    .filter_map(|entry| {
                        let HostValue::Object(pairs) = entry else {
                            return None;
                        };
                        let mut request_type: Option<String> = None;
                        let mut reply_type: Option<String> = None;
                        for (k, v) in pairs {
                            match (k.as_str(), v) {
                                ("request_type", HostValue::String(s)) => {
                                    request_type = Some(s);
                                }
                                ("reply_type", HostValue::String(s)) => {
                                    reply_type = Some(s);
                                }
                                _ => {}
                            }
                        }
                        match (request_type, reply_type) {
                            (Some(rq), Some(rp)) => Some(graphreact_core::ExternalCall {
                                request_type: rq,
                                reply_type: rp,
                            }),
                            _ => None,
                        }
                    })
                    .collect();
            }
            _ => {}
        }
    }
    let mut manifest = PermissionManifest::new(accepts, emits);
    manifest.accepts_external = accepts_external;
    manifest.accepts_external_call = accepts_external_call;
    manifest
}

// ---------------------------------------------------------------------
// JS runtime — Scheduler + JscEngine + IR mount
// ---------------------------------------------------------------------

/// The phase-3b composition: a [`Scheduler`] driving JS components on
/// a [`JscEngine`]. Returned by [`mount_with_scheduler`]; consumers
/// drive it via [`JsRuntime::process_batch`] and observe via the
/// signal registry.
pub struct JsRuntime {
    pub engine: Rc<RefCell<JscEngine>>,
    pub iso: IsolateHandle,
    pub scheduler: Scheduler,
    pub mounted: Vec<String>,
    pub graph_id: String,
    /// Total node count from the parsed IR (includes nodes that may
    /// not have been registered with the scheduler if their component
    /// definition was missing).
    pub n_nodes: usize,
    /// Total edge count from the parsed IR.
    pub n_edges: usize,
    /// Total definition (component) count from the parsed IR.
    pub n_definitions: usize,
    /// Live directory handle (cloned from Scheduler) installed via
    /// TLS by `process_batch` for `host_directory_*` callbacks.
    /// Phase 3e-δ.
    pub directory: Rc<RefCell<GraphDirectory>>,
    /// Static graph snapshot captured at mount time. Read by
    /// `host_graph_*` accessors (useGraphQuery's `__graphQueryProxy`).
    /// Stale on dynamic spawnChild/compose mutations — acceptable for
    /// canonical fixtures' read-only patterns. Phase 3e-δ.
    pub graph_snapshot: Rc<GraphSnapshot>,
    /// IDs of nodes the scheduler removed since the last
    /// `process_batch`. Populated by a topology listener installed in
    /// `mount_with_scheduler`; drained at the end of every
    /// `process_batch` and used to dispatch `__releaseTokens` for each
    /// removed node. M12.3a-followon-D.
    pub(crate) pending_unmounts: Arc<parking_lot::Mutex<Vec<String>>>,
    /// Keep the topology subscription alive — drop unsubscribes. Held
    /// here so the listener (which pushes into `pending_unmounts`)
    /// fires for the lifetime of the runtime. M12.3a-followon-D.
    _topology_subscription: Subscription,
}

impl JsRuntime {
    /// Drive the scheduler one process_batch round. Installs the
    /// engine + directory + graph-snapshot TLS for the duration so
    /// deferred-fire host callbacks (effects, message handlers,
    /// directory accessors) can read what they need without
    /// capturing `Rc` directly.
    ///
    /// After the batch settles, drains `pending_unmounts` and sweeps
    /// the JS-side `__hostCallbacks` entries for each removed node via
    /// `__releaseTokens`. The sweep runs while the engine guard is
    /// still installed so `release_tokens_for_node` can re-enter JSC.
    /// M12.3a-followon-D.
    pub fn process_batch(&mut self) -> Result<(), graphreact_core::RuntimeError> {
        let _engine_guard = EngineGuard::with_directory(
            Rc::clone(&self.engine),
            self.iso,
            Rc::clone(&self.directory),
            Rc::clone(&self.graph_snapshot),
        );
        let result = self.scheduler.process_batch();
        // Sweep tokens for nodes removed during the batch. The
        // topology listener pushed their IDs into `pending_unmounts`
        // synchronously when the graph mutation fired. Drain into a
        // local first so the lock isn't held during JSC re-entry.
        let drained: Vec<String> = std::mem::take(&mut *self.pending_unmounts.lock());
        for id in drained {
            release_tokens_for_node(&self.engine, self.iso, &id);
        }
        result
    }

    /// Async sibling of [`Self::process_batch`]. **M12.3b-phase1** /
    /// **M12.3b-phase1.5.** Drives the scheduler via
    /// `process_batch_async` so `useEffectAsync` effects fire (sync
    /// `useEffect` continues to run too — both paths process the sync
    /// slot identically). Same TLS install pattern as the sync variant;
    /// same post-batch `__hostCallbacks` token sweep.
    ///
    /// **phase1.5**: post-batch microtask drain. The `host_effect_async`
    /// closure's `select!`-driven inner drain handles in-flight
    /// settlements; this trailing drain catches anything queued by
    /// host callbacks (`host_emit`, `host_message`, topology dispatch)
    /// that resolved during the batch but didn't naturally re-enter
    /// JSC again before returning.
    ///
    /// Must be called from a `tokio::task::LocalSet` (or other
    /// single-threaded executor) because `JsRuntime` is `!Send` (the
    /// engine is `Rc<RefCell<...>>`).
    pub async fn process_batch_async(&mut self) -> Result<(), graphreact_core::RuntimeError> {
        let _engine_guard = EngineGuard::with_directory(
            Rc::clone(&self.engine),
            self.iso,
            Rc::clone(&self.directory),
            Rc::clone(&self.graph_snapshot),
        );
        let result = self.scheduler.process_batch_async().await;
        // Defensive drain — any microtasks left in the queue from
        // host-callback-driven resolutions that didn't re-enter JSC
        // are flushed here. No-op if the queue is empty.
        let _ = drain_microtasks_static(self.iso);
        let drained: Vec<String> = std::mem::take(&mut *self.pending_unmounts.lock());
        for id in drained {
            release_tokens_for_node(&self.engine, self.iso, &id);
        }
        result
    }

    /// Tear down — disposes the isolate and shuts the engine. Idempotent
    /// in the sense that it can be called once; subsequent calls error.
    pub fn shutdown(&mut self) -> Result<(), EngineError> {
        let mut e = self.engine.borrow_mut();
        e.dispose_isolate(self.iso)?;
        e.shutdown()?;
        Ok(())
    }
}

/// Phase-3b mount: parses the `.grx` file, builds a
/// [`CommunicationGraph`] from the IR, instantiates a [`Scheduler`],
/// registers each node with a JS-backed [`NodeDefinition`], and
/// returns the runtime ready for `process_batch`.
///
/// The caller is responsible for driving the scheduler. This function
/// mounts but does NOT run the first batch — that's a consumer choice
/// (tests want fine-grained control; the CLI typically runs once
/// then stops).
pub fn mount_with_scheduler(path: &std::path::Path) -> Result<JsRuntime, RuntimeError> {
    let resolved = resolve_multi_file(path)?;

    // Concatenate definitions from root + imports (same shape as
    // dry_mount_jsc).
    let mut definitions: Vec<(String, String, String, graphreact_grx_parser::ast::ComponentLang)> =
        Vec::new();
    for d in &resolved.root.definitions {
        definitions.push((
            resolved.root_path.display().to_string(),
            d.component.clone(),
            d.code.clone(),
            d.lang,
        ));
    }
    for imp in &resolved.imports {
        for d in &imp.file.definitions {
            definitions.push((
                imp.path.display().to_string(),
                d.component.clone(),
                d.code.clone(),
                d.lang,
            ));
        }
    }

    let graph_block = resolved
        .root
        .graph
        .as_ref()
        .ok_or_else(|| RuntimeError::NoGraph {
            path: resolved.root_path.display().to_string(),
        })?;

    let ir = GrxIr {
        graph_id: graph_block.id.clone(),
        graph_loc: graph_block.loc,
        nodes: graphreact_grx_parser::ir::build_ir(&GrxFile {
            imports: resolved.root.imports.clone(),
            graph: Some(graph_block.clone()),
            definitions: resolved.root.definitions.clone(),
        })
        .nodes,
        edges: graphreact_grx_parser::ir::build_ir(&GrxFile {
            imports: resolved.root.imports.clone(),
            graph: Some(graph_block.clone()),
            definitions: resolved.root.definitions.clone(),
        })
        .edges,
        definitions: definitions
            .iter()
            .map(|(_, comp, code, lang)| graphreact_grx_parser::ir::IrDefinition {
                component: comp.clone(),
                code: code.clone(),
                lang: *lang,
                source_loc: SourceLoc::default(),
            })
            .collect(),
        imports: resolved
            .imports
            .iter()
            .map(|i| i.path.display().to_string())
            .collect(),
    };

    // Build the CommunicationGraph from IR.
    let mut graph = CommunicationGraph::new();
    for node in &ir.nodes {
        let mut attrs: Attrs = Attrs::new();
        for (k, v) in &node.attrs {
            attrs.insert(k.clone(), v.clone());
        }
        graph.add_node(node.id.clone(), attrs);
    }
    for edge in &ir.edges {
        let mut attrs: Attrs = Attrs::new();
        for (k, v) in &edge.attrs {
            attrs.insert(k.clone(), v.clone());
        }
        graph.add_edge(edge.source.clone(), edge.target.clone(), None, attrs);
    }

    // Construct + initialise the engine.
    let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
    let iso = {
        let mut e = engine_rc.borrow_mut();
        e.initialize()?;
        e.create_isolate(IsolateOptions::default())?
    };

    // Register host callbacks BEFORE eval_component so the bodies can
    // reference them via the prelude stubs.
    {
        let mut e = engine_rc.borrow_mut();
        e.register_callback(iso, "host_state", build_host_state_callback())?;
        e.register_callback(iso, "host_state_set", build_host_state_set_callback())?;
        e.register_callback(iso, "host_effect", build_host_effect_callback())?;
        // M12.3b-phase1: useEffectAsync — real impl via JS Promise →
        // Rust Future bridge (oneshot channel keyed by completion id).
        e.register_callback(
            iso,
            "host_effect_async",
            build_host_effect_async_callback(),
        )?;
        e.register_callback(
            iso,
            "host_effect_async_complete",
            build_host_effect_async_complete_callback(),
        )?;
        e.register_callback(iso, "host_emit", build_host_emit_callback())?;
        e.register_callback(iso, "host_message", build_host_message_callback())?;
        e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())?;
        e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())?;
        e.register_callback(iso, "host_neighbors", build_host_neighbors_callback())?;
        e.register_callback(iso, "host_topology", build_host_topology_callback())?;
        e.register_callback(iso, "host_accept_edge", build_host_accept_edge_callback())?;
        e.register_callback(iso, "host_spawn_child", build_host_spawn_child_callback())?;
        e.register_callback(iso, "host_unmount_self", build_host_unmount_self_callback())?;
        e.register_callback(iso, "host_compose", build_host_compose_callback())?;
        // 3e-δ: directory finders.
        e.register_callback(
            iso,
            "host_directory_find_by_id",
            build_host_directory_find_by_id_callback(),
        )?;
        e.register_callback(
            iso,
            "host_directory_find_by_role",
            build_host_directory_find_by_role_callback(),
        )?;
        e.register_callback(
            iso,
            "host_directory_find_by_depth",
            build_host_directory_find_by_depth_callback(),
        )?;
        e.register_callback(
            iso,
            "host_directory_find_in_subtree",
            build_host_directory_find_in_subtree_callback(),
        )?;
        e.register_callback(
            iso,
            "host_directory_is_ancestor",
            build_host_directory_is_ancestor_callback(),
        )?;
        // 3e-δ: graph-query proxy backing accessors.
        e.register_callback(iso, "host_graph_neighbors", build_host_graph_neighbors_callback())?;
        e.register_callback(iso, "host_graph_has_node", build_host_graph_has_node_callback())?;
        e.register_callback(iso, "host_graph_has_edge", build_host_graph_has_edge_callback())?;
        e.register_callback(iso, "host_graph_node_count", build_host_graph_node_count_callback())?;
        // M12.3a-followon-B: graph-algorithm accessors on __graphQueryProxy.
        e.register_callback(
            iso,
            "host_graph_shortest_path",
            build_host_graph_shortest_path_callback(),
        )?;
        e.register_callback(
            iso,
            "host_graph_all_paths",
            build_host_graph_all_paths_callback(),
        )?;
        e.register_callback(
            iso,
            "host_graph_a_star_path",
            build_host_graph_a_star_path_callback(),
        )?;
        e.register_callback(iso, "host_graph_bfs", build_host_graph_bfs_callback())?;
        e.register_callback(iso, "host_graph_dfs", build_host_graph_dfs_callback())?;
        e.register_callback(
            iso,
            "host_graph_connected_components",
            build_host_graph_connected_components_callback(),
        )?;
        e.register_callback(
            iso,
            "host_graph_cycle_detection",
            build_host_graph_cycle_detection_callback(),
        )?;
        // 3e-ε: useSource + timer/delay factories.
        e.register_callback(iso, "host_use_source", build_host_use_source_callback())?;
        // 3e-ζ: supervision.
        e.register_callback(iso, "host_monitor", build_host_monitor_callback())?;
        e.register_callback(iso, "host_demonitor", build_host_demonitor_callback())?;
        e.register_callback(iso, "host_link", build_host_link_callback())?;
        e.register_callback(iso, "host_unlink", build_host_unlink_callback())?;
        e.register_callback(iso, "host_group_join", build_host_group_join_callback())?;
        e.register_callback(iso, "host_group_leave", build_host_group_leave_callback())?;
        // Remaining no-op stubs — host_send/host_log/host_dispatch/
        // host_observe are reserved namespaces.
        e.register_callback(iso, "host_send", Box::new(|_| Ok(HostValue::Null)))?;
        e.register_callback(iso, "host_log", Box::new(|_| Ok(HostValue::Null)))?;
        e.register_callback(iso, "host_dispatch", Box::new(|_| Ok(HostValue::Null)))?;
        e.register_callback(iso, "host_observe", Box::new(|_| Ok(HostValue::Null)))?;
        // M12.3a-followon-D: per-node `__hostCallbacks` token tracking
        // for unmount sweep. Must be registered for live mounts; the
        // dry-mount path uses the stub variant (see runtime.rs).
        e.register_callback(iso, "host_track_token", build_host_track_token_callback())?;
        // M12.3a-followon-E: Tier 2 (policy-derived reachability).
        // The 9 host_tier2_* callbacks back the `g.tier2(msgType)`
        // sub-proxy on `__graphQueryProxy`. Direction emerges per
        // message type from `PermissionManifest.emits/accepts` pairs.
        e.register_callback(
            iso,
            "host_tier2_node_count",
            build_host_tier2_node_count_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier2_has_node",
            build_host_tier2_has_node_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier2_neighbors",
            build_host_tier2_neighbors_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier2_has_edge",
            build_host_tier2_has_edge_callback(),
        )?;
        e.register_callback(iso, "host_tier2_bfs", build_host_tier2_bfs_callback())?;
        e.register_callback(iso, "host_tier2_dfs", build_host_tier2_dfs_callback())?;
        e.register_callback(
            iso,
            "host_tier2_shortest_path",
            build_host_tier2_shortest_path_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier2_topological_sort",
            build_host_tier2_topological_sort_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier2_a_star_path",
            build_host_tier2_a_star_path_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier2_all_paths",
            build_host_tier2_all_paths_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier2_connected_components",
            build_host_tier2_connected_components_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier2_cycle_detection",
            build_host_tier2_cycle_detection_callback(),
        )?;
        // M12.3a-followon-F: Tier 3 (negotiation-aware planning).
        // The 12 host_tier3_* callbacks back the `g.tier3(msgType)`
        // sub-proxy on `__graphQueryProxy`. Tier 3 = Tier 2 ∪ {(s, u)
        // where target u's edge-accept predicate accepts (s, msgType)}.
        // Predicates are installed via `useEdgeAcceptHandler(fn)` from
        // a node body and consulted at query time in pure dry-run
        // mode (no AcceptEdge mutation triggered).
        e.register_callback(
            iso,
            "host_edge_accept_handler",
            build_host_edge_accept_handler_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier3_node_count",
            build_host_tier3_node_count_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier3_has_node",
            build_host_tier3_has_node_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier3_neighbors",
            build_host_tier3_neighbors_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier3_has_edge",
            build_host_tier3_has_edge_callback(),
        )?;
        e.register_callback(iso, "host_tier3_bfs", build_host_tier3_bfs_callback())?;
        e.register_callback(iso, "host_tier3_dfs", build_host_tier3_dfs_callback())?;
        e.register_callback(
            iso,
            "host_tier3_shortest_path",
            build_host_tier3_shortest_path_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier3_topological_sort",
            build_host_tier3_topological_sort_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier3_a_star_path",
            build_host_tier3_a_star_path_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier3_all_paths",
            build_host_tier3_all_paths_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier3_connected_components",
            build_host_tier3_connected_components_callback(),
        )?;
        e.register_callback(
            iso,
            "host_tier3_cycle_detection",
            build_host_tier3_cycle_detection_callback(),
        )?;
    }

    // Eval each definition as a re-callable component function.
    let mut component_handles: HashMap<String, FunctionHandle> = HashMap::new();
    for (origin, component, source, lang) in &definitions {
        // The JSC scheduler-mount path handles JS bodies only. A
        // non-JS lang here is a fixture-runtime mismatch — the user
        // likely wants `--runtime=python` or has a stale fixture.
        if *lang != graphreact_grx_parser::ast::ComponentLang::Js {
            return Err(RuntimeError::EngineMisroute {
                component: component.clone(),
                lang: lang.as_attr_str().to_string(),
                engine: "jsc".to_string(),
            });
        }
        let module_name = format!("{origin}::{component}");
        let handle = engine_rc.borrow_mut().eval_component(iso, &module_name, source)?;
        component_handles.insert(component.clone(), handle);
    }

    // Create scheduler bound to the freshly-mutated graph.
    let mut scheduler = Scheduler::new(graph);

    // Register nodes. Component name resolution mirrors phase 3a's
    // attrs-then-tag fallback.
    let mut mounted: Vec<String> = Vec::new();
    for node in &ir.nodes {
        let component_name = component_name_for_node(node);
        let Some(&fn_handle) = component_handles.get(&component_name) else {
            continue;
        };
        let definition = build_js_node_definition(
            component_name,
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut props: Attrs = Attrs::new();
        for (k, v) in &node.attrs {
            props.insert(k.clone(), v.clone());
        }
        let height: u32 = node.depth as u32;
        let _ = scheduler.register_node(definition, node.id.clone(), props, height)?;
        mounted.push(node.id.clone());
    }

    let n_nodes = ir.nodes.len();
    let n_edges = ir.edges.len();
    let n_definitions = ir.definitions.len();
    let directory = Rc::clone(&scheduler.directory);
    let graph_snapshot = Rc::new(GraphSnapshot::from_live(
        Rc::clone(&scheduler.graph),
        Rc::clone(&scheduler.manifests),
        Rc::clone(&scheduler.edge_accept_predicates),
    ));

    // M12.3a-followon-D: subscribe to NodeRemoved events so the runtime
    // can sweep `__hostCallbacks` token entries on unmount. The
    // listener pushes IDs into a thread-safe queue (it fires
    // synchronously from inside graph mutations on the scheduler's
    // thread); `process_batch` drains the queue after the batch
    // settles and dispatches `__releaseTokens` for each entry.
    //
    // **M15 Phase 9**: the same listener also clears per-node
    // `SIGNAL_BY_HOOK_POS` plus `SIGNAL_REGISTRY` plus `MEMO_REGISTRY`
    // entries inline (no JSC re-entry needed) so a subsequent
    // re-mount of the same node id (incompat-reload's drain plus
    // remount path) doesn't pick up stale `Signal` handles whose
    // underlying `GenerationalBox` was dropped on unmount. Without
    // this sweep the new mount's `useState` would panic with
    // `ValueDroppedError` on first read.
    let pending_unmounts: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let pending_unmounts_for_listener = Arc::clone(&pending_unmounts);
    let topology_subscription = scheduler
        .graph
        .borrow()
        .on_topology_change(move |event: &TopologyEvent| {
            if let TopologyEvent::NodeRemoved { node_id } = event {
                clear_node_local_state(node_id);
                pending_unmounts_for_listener.lock().push(node_id.clone());
            }
        });

    Ok(JsRuntime {
        engine: engine_rc,
        iso,
        scheduler,
        mounted,
        graph_id: ir.graph_id,
        n_nodes,
        n_edges,
        n_definitions,
        directory,
        graph_snapshot,
        pending_unmounts,
        _topology_subscription: topology_subscription,
    })
}

fn component_name_for_node(node: &IrNode) -> String {
    node.attrs
        .get("component")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| node.component.clone())
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    /// JSC's process-global state demands serialised tests. Forwards
    /// to the crate-wide `super::engine_lock` so JSC-touching tests
    /// in sibling modules (e.g. `hotreload_adapter::tests`) share the
    /// same mutex.
    fn engine_lock() -> MutexGuard<'static, ()> {
        super::engine_lock()
    }

    /// Phase 3b: `useState` preserves value across renders.
    ///
    /// Strategy: build a single-node graph, mount it, run two
    /// process_batch rounds. Between them, externally call `.set(v)`
    /// on the registered Signal. The component body reports the
    /// observed `count` via `host_observe` so the test can collect
    /// successive renders.
    #[test]
    fn phase3b_use_state_preserves_value_across_renders() {
        let _g = engine_lock();
        reset_global_state();

        // Build a one-node graph by hand (skips the IR + .grx parser).
        let mut graph = CommunicationGraph::new();
        graph.add_node("counter-1".to_string(), Attrs::new());

        // Set up a JSC engine + register host callbacks. Use a custom
        // host_observe that pushes into a shared Mutex<Vec<u64>>.
        let observations: std::sync::Arc<parking_lot::Mutex<Vec<u64>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::Number(n)) = args.first() {
                        observations_for_cb.lock().push(*n as u64);
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        let body = "\
            const [count, setCount] = useState(0);\n\
            host_observe(count);\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Counter", body)
            .expect("eval");

        // Set up scheduler; register the node with our JS-backed
        // definition. Default SchedulerHandle (height=0) since
        // there's no hierarchy in this test.
        let definition = build_js_node_definition(
            "Counter".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "counter-1".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // First batch: useState(0) → host_state mints id, signal value=0.
        scheduler.process_batch().expect("first batch");
        {
            let observed = observations.lock();
            assert_eq!(observed.as_slice(), &[0_u64], "first render observed 0");
        }

        // Find the signal id we minted (only one component, only one
        // useState — id should be 1).
        let id = SIGNAL_BY_HOOK_POS
            .with(|map| map.borrow().get(&("counter-1".to_string(), 0)).copied())
            .expect("signal id should be registered after first render");

        // Externally write to the signal — marks the node dirty via
        // graphreact-core's enqueue_self closure.
        set_signal(id, Value::Number(serde_json::Number::from(7)));

        // Second batch: re-render with preserved state. useState
        // should return the same Signal — current value is now 7.
        scheduler.process_batch().expect("second batch");
        {
            let observed = observations.lock();
            assert_eq!(
                observed.as_slice(),
                &[0_u64, 7_u64],
                "second render observed 7 (state preserved)"
            );
        }

        // Tear down.
        {
            let mut e = engine_rc.borrow_mut();
            e.dispose_isolate(iso).expect("dispose");
            e.shutdown().expect("shutdown");
        }
    }

    /// Phase 3b: a JS setter call (via host_state_set) marks the
    /// owning node dirty and triggers re-evaluation.
    #[test]
    fn phase3b_js_setter_triggers_rerender() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("c-2".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<u64>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::Number(n)) = args.first() {
                        observations_for_cb.lock().push(*n as u64);
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body that calls the setter the FIRST time it sees count==0.
        // The setter call dispatches host_state_set — which updates
        // the signal AND marks the node dirty. Next batch round
        // re-renders with the new value.
        let body = "\
            const [count, setCount] = useState(0);\n\
            host_observe(count);\n\
            if (count === 0) { setCount(42); }\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Setter", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Setter".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "c-2".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // Single process_batch — the cascade should run twice
        // internally because the first render's setCount(42) marks
        // the node dirty, and the batch loop re-evaluates until
        // quiescent.
        scheduler.process_batch().expect("batch");

        let observed = observations.lock();
        assert_eq!(
            observed.as_slice(),
            &[0_u64, 42_u64],
            "first render saw 0, JS setter triggered re-render with 42"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3c: `useEffect(fn, [])` with empty deps fires once after
    /// the first render's body returns; does not re-fire on
    /// subsequent renders that reuse the same deps. The effect
    /// callback runs from `flush_effects`, AFTER the body's
    /// `call_function` returned, which is why TLS-based engine
    /// access (rather than `Rc` capture) is the design.
    #[test]
    fn phase3c_use_effect_fires_once_with_empty_deps() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("eff-1".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_effect", build_host_effect_callback())
                .expect("host_effect");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: useEffect logs once on mount, then state changes
        // shouldn't re-fire it (empty deps).
        let body = "\
            const [count, setCount] = useState(0);\n\
            useEffect(function() { host_observe('effect:' + count); }, []);\n\
            host_observe('render:' + count);\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "EffectOnce", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "EffectOnce".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "eff-1".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // First batch: render fires, effect fires from flush_effects.
        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("first batch");
        }
        {
            let observed = observations.lock();
            assert_eq!(
                observed.as_slice(),
                &["render:0".to_string(), "effect:0".to_string()],
                "first batch: render then effect"
            );
        }

        // Externally toggle the signal to force a re-render.
        let id = SIGNAL_BY_HOOK_POS
            .with(|map| map.borrow().get(&("eff-1".to_string(), 0)).copied())
            .expect("state id registered");
        set_signal(id, Value::Number(serde_json::Number::from(99)));

        // Second batch: re-render but effect's deps (`[]`) didn't
        // change, so the effect doesn't fire again.
        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("second batch");
        }
        {
            let observed = observations.lock();
            assert_eq!(
                observed.as_slice(),
                &[
                    "render:0".to_string(),
                    "effect:0".to_string(),
                    "render:99".to_string(),
                ],
                "second batch: render only, no re-fire of effect"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3c: `useEffect(fn, [count])` re-fires when `count`
    /// changes; cleanup from the previous run executes before the
    /// new run. Mirrors React's commit-then-cleanup-then-effect
    /// ordering — graphreact-core's `flush_effects` runs the prior
    /// cleanup before re-invoking the effect.
    #[test]
    fn phase3c_use_effect_re_fires_on_dep_change_with_cleanup() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("eff-2".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_effect", build_host_effect_callback())
                .expect("host_effect");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: effect with deps=[count]. Returns a cleanup function.
        let body = "\
            const [count, setCount] = useState(0);\n\
            useEffect(function() {\n\
              host_observe('effect:' + count);\n\
              return function() { host_observe('cleanup:' + count); };\n\
            }, [count]);\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "EffectDeps", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "EffectDeps".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "eff-2".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // First batch: render → effect fires (no prior cleanup).
        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("first batch");
        }
        {
            let observed = observations.lock();
            assert_eq!(
                observed.as_slice(),
                &["effect:0".to_string()],
                "first batch: effect:0"
            );
        }

        // Change count: effect deps change → on re-render, prior
        // cleanup fires before the new effect.
        let id = SIGNAL_BY_HOOK_POS
            .with(|map| map.borrow().get(&("eff-2".to_string(), 0)).copied())
            .expect("state id registered");
        set_signal(id, Value::Number(serde_json::Number::from(1)));

        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("second batch");
        }
        {
            let observed = observations.lock();
            assert_eq!(
                observed.as_slice(),
                &[
                    "effect:0".to_string(),
                    "cleanup:0".to_string(),
                    "effect:1".to_string(),
                ],
                "second batch: cleanup of prior, then new effect"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d: `useEmit()(type, payload)` from a body's effect
    /// reaches a neighbor's `useMessage(type, handler)`. Tests the
    /// minimal end-to-end flow: emit pushes onto the scheduler queue,
    /// `deliver()` resolves the channel policy, the handler fires.
    #[test]
    fn phase3d_use_emit_dispatches_to_neighbor() {
        let _g = engine_lock();
        reset_global_state();

        // A → B (one direction is enough; emit reaches neighbors).
        let mut graph = CommunicationGraph::new();
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_effect", build_host_effect_callback())
                .expect("host_effect");
            e.register_callback(iso, "host_emit", build_host_emit_callback())
                .expect("host_emit");
            e.register_callback(iso, "host_message", build_host_message_callback())
                .expect("host_message");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        let body_a = "\
            const emit = useEmit();\n\
            useEffect(function() { emit('ping', { hello: 'world' }); }, []);\n\
            return { accepts: [], emits: ['ping'] };\n\
        ";
        let body_b = "\
            useMessage('ping', function() { host_observe('b-received'); });\n\
            return { accepts: ['ping'], emits: [] };\n\
        ";

        let fn_a = engine_rc
            .borrow_mut()
            .eval_component(iso, "A", body_a)
            .expect("eval a");
        let fn_b = engine_rc
            .borrow_mut()
            .eval_component(iso, "B", body_b)
            .expect("eval b");

        let def_a = build_js_node_definition("A".into(), Rc::clone(&engine_rc), iso, fn_a);
        let def_b = build_js_node_definition("B".into(), Rc::clone(&engine_rc), iso, fn_b);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_a, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register a");
        scheduler
            .register_node(def_b, "b".to_string(), Attrs::new(), 0_u32)
            .expect("register b");

        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert_eq!(
                observed.as_slice(),
                &["b-received".to_string()],
                "B's useMessage handler observed exactly one delivery from A's emit"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d: the JS-side `msg` handed to a `useMessage` handler
    /// has the `{from, type, payload, ttl}` shape mirroring NAPI
    /// (graphreact-napi/src/hooks.rs::invoke_message_handler). Asserts
    /// nested payload survives the JSON FFI round-trip and `ttl` is a
    /// number type.
    #[test]
    fn phase3d_use_message_handler_receives_marshalled_message() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("sender".to_string(), Attrs::new());
        graph.add_node("recv".to_string(), Attrs::new());
        graph.add_edge("sender".to_string(), "recv".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_effect", build_host_effect_callback())
                .expect("host_effect");
            e.register_callback(iso, "host_emit", build_host_emit_callback())
                .expect("host_emit");
            e.register_callback(iso, "host_message", build_host_message_callback())
                .expect("host_message");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        let body_sender = "\
            const emit = useEmit();\n\
            useEffect(function() {\n\
              emit('greet', { greeting: 'hi', count: 7, nested: { ok: true } });\n\
            }, []);\n\
            return { accepts: [], emits: ['greet'] };\n\
        ";
        let body_recv = "\
            useMessage('greet', function(msg) {\n\
              host_observe('from=' + msg.from);\n\
              host_observe('type=' + msg.type);\n\
              host_observe('payload.greeting=' + msg.payload.greeting);\n\
              host_observe('payload.count=' + msg.payload.count);\n\
              host_observe('payload.nested.ok=' + msg.payload.nested.ok);\n\
              host_observe('ttl-typeof=' + (typeof msg.ttl));\n\
            });\n\
            return { accepts: ['greet'], emits: [] };\n\
        ";

        let fn_s = engine_rc
            .borrow_mut()
            .eval_component(iso, "Sender", body_sender)
            .expect("eval sender");
        let fn_r = engine_rc
            .borrow_mut()
            .eval_component(iso, "Recv", body_recv)
            .expect("eval recv");

        let def_s = build_js_node_definition("Sender".into(), Rc::clone(&engine_rc), iso, fn_s);
        let def_r = build_js_node_definition("Recv".into(), Rc::clone(&engine_rc), iso, fn_r);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_s, "sender".to_string(), Attrs::new(), 0_u32)
            .expect("register sender");
        scheduler
            .register_node(def_r, "recv".to_string(), Attrs::new(), 0_u32)
            .expect("register recv");

        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert_eq!(
                observed.as_slice(),
                &[
                    "from=sender".to_string(),
                    "type=greet".to_string(),
                    "payload.greeting=hi".to_string(),
                    "payload.count=7".to_string(),
                    "payload.nested.ok=true".to_string(),
                    "ttl-typeof=number".to_string(),
                ],
                "useMessage handler received {{from, type, payload, ttl}} matching NAPI shape"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-α: `useMemo(fn, deps)` returns the cached value when
    /// `deps` is unchanged across renders. The factory runs on the
    /// first render (deps go from "unset" to set) and on every
    /// subsequent render where deps differ. host_observe records
    /// every factory invocation so we can count them.
    #[test]
    fn phase3e_alpha_use_memo_caches_when_deps_unchanged() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("memo-1".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: useMemo with constant deps `[42]` — factory should fire
        // exactly once across multiple renders. useState lets us drive
        // re-renders without changing deps.
        let body = "\
            const [_count, setCount] = useState(0);\n\
            const memoed = useMemo(function() {\n\
              host_observe('factory-ran');\n\
              return 'computed-value';\n\
            }, [42]);\n\
            host_observe('memo:' + memoed);\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "MemoCacher", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "MemoCacher".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "memo-1".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // First render — factory runs once.
        scheduler.process_batch().expect("first batch");

        // Force a re-render WITHOUT changing memo deps. Find the useState
        // signal id and bump it.
        let id = SIGNAL_BY_HOOK_POS
            .with(|map| map.borrow().get(&("memo-1".to_string(), 0)).copied())
            .expect("state id registered");
        set_signal(id, Value::Number(serde_json::Number::from(1)));
        scheduler.process_batch().expect("second batch");

        // Force a third render the same way.
        set_signal(id, Value::Number(serde_json::Number::from(2)));
        scheduler.process_batch().expect("third batch");

        let observed = observations.lock();
        let factory_runs = observed.iter().filter(|s| *s == "factory-ran").count();
        let memo_outputs = observed.iter().filter(|s| s.starts_with("memo:")).count();
        assert_eq!(
            factory_runs, 1,
            "factory ran exactly once across 3 renders; observations: {observed:?}"
        );
        assert_eq!(
            memo_outputs, 3,
            "memoed value appeared on every render (3 times); observations: {observed:?}"
        );

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-α: `useMemo(fn, deps)` re-runs the factory when deps
    /// change. Drives this by parameterising deps off a useState
    /// counter; bumping the counter changes the deps and the factory
    /// must fire again with the new value.
    #[test]
    fn phase3e_alpha_use_memo_recomputes_on_dep_change() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("memo-2".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: useMemo's deps include the state counter. Each setCount
        // changes deps → factory runs again with the new value.
        let body = "\
            const [count, _setCount] = useState(0);\n\
            const doubled = useMemo(function() {\n\
              host_observe('factory:' + count);\n\
              return count * 2;\n\
            }, [count]);\n\
            host_observe('memo:' + doubled);\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "MemoDeps", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "MemoDeps".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "memo-2".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        scheduler.process_batch().expect("first batch");
        let id = SIGNAL_BY_HOOK_POS
            .with(|map| map.borrow().get(&("memo-2".to_string(), 0)).copied())
            .expect("state id");
        set_signal(id, Value::Number(serde_json::Number::from(5)));
        scheduler.process_batch().expect("second batch");
        set_signal(id, Value::Number(serde_json::Number::from(7)));
        scheduler.process_batch().expect("third batch");

        let observed = observations.lock();
        let factory_runs: Vec<&String> = observed
            .iter()
            .filter(|s| s.starts_with("factory:"))
            .collect();
        let memo_outputs: Vec<&String> = observed
            .iter()
            .filter(|s| s.starts_with("memo:"))
            .collect();
        // Factory fires on every render because deps (`[count]`) changed
        // each time. Render 1 (deps unset → 0), render 2 (0 → 5),
        // render 3 (5 → 7). Memoed value is `count * 2`.
        assert_eq!(
            factory_runs,
            vec![
                &"factory:0".to_string(),
                &"factory:5".to_string(),
                &"factory:7".to_string(),
            ],
            "factory fired with the correct deps each render; observations: {observed:?}"
        );
        assert_eq!(
            memo_outputs,
            vec![
                &"memo:0".to_string(),
                &"memo:10".to_string(),
                &"memo:14".to_string(),
            ],
            "memoed values reflect the count at each render; observations: {observed:?}"
        );

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-α: `useCallback(fn, deps)` returns the same function
    /// reference across renders when `deps` is unchanged. JS-side
    /// identity check via `===`.
    #[test]
    fn phase3e_alpha_use_callback_identity_stable_across_renders() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("cb-1".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: useCallback with constant deps. Stash the returned
        // function on globalThis between renders and compare identity
        // on the next render. Use `===` to verify identical reference.
        let body = "\
            const [_count, _setCount] = useState(0);\n\
            const cb = useCallback(function() {}, [42]);\n\
            if (typeof globalThis.__prevCb === 'undefined') {\n\
              host_observe('first-render');\n\
            } else {\n\
              host_observe(globalThis.__prevCb === cb ? 'identity-stable' : 'identity-changed');\n\
            }\n\
            globalThis.__prevCb = cb;\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "CbStable", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "CbStable".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "cb-1".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        scheduler.process_batch().expect("first batch");
        let id = SIGNAL_BY_HOOK_POS
            .with(|map| map.borrow().get(&("cb-1".to_string(), 0)).copied())
            .expect("state id");
        set_signal(id, Value::Number(serde_json::Number::from(1)));
        scheduler.process_batch().expect("second batch");
        set_signal(id, Value::Number(serde_json::Number::from(2)));
        scheduler.process_batch().expect("third batch");

        let observed = observations.lock();
        assert_eq!(
            observed.as_slice(),
            &[
                "first-render".to_string(),
                "identity-stable".to_string(),
                "identity-stable".to_string(),
            ],
            "useCallback returned the same function reference across 3 renders; observations: {observed:?}"
        );

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-α: `useReducer(reducer, initial)` composes over
    /// `useState`; dispatching an action updates state. Pure-JS in
    /// the prelude — no host callback needed beyond useState.
    #[test]
    fn phase3e_alpha_use_reducer_dispatch_updates_state() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("rdc-1".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<u64>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::Number(n)) = args.first() {
                        observations_for_cb.lock().push(*n as u64);
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: classic reducer counter pattern. On first render
        // (count===0), dispatch 'increment' twice to drive state to 2.
        let body = "\
            const reducer = function(s, action) { return action === 'increment' ? s + 1 : s; };\n\
            const pair = useReducer(reducer, 0);\n\
            const count = pair[0];\n\
            const dispatch = pair[1];\n\
            host_observe(count);\n\
            if (count === 0) { dispatch('increment'); }\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Reducer", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Reducer".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "rdc-1".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // process_batch drives the cascade: first render observes 0
        // and dispatches increment → re-render observes 1 (no further
        // dispatch since count !== 0).
        scheduler.process_batch().expect("batch");

        let observed = observations.lock();
        assert_eq!(
            observed.as_slice(),
            &[0_u64, 1_u64],
            "reducer composed over useState; dispatch incremented count; observations: {observed:?}"
        );

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-β: `useNeighbors()` returns the active node's
    /// neighbor IDs — read straight from HOOK_CTX.neighbors. The
    /// optional JS-side filter narrows the list.
    #[test]
    fn phase3e_beta_use_neighbors_returns_active_node_neighbors() {
        let _g = engine_lock();
        reset_global_state();

        // Star graph: hub → {a, b, c}. hub's neighbors are [a, b, c].
        let mut graph = CommunicationGraph::new();
        graph.add_node("hub".to_string(), Attrs::new());
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());
        graph.add_node("c".to_string(), Attrs::new());
        graph.add_edge("hub".to_string(), "a".to_string(), None, Attrs::new());
        graph.add_edge("hub".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("hub".to_string(), "c".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_neighbors", build_host_neighbors_callback())
                .expect("host_neighbors");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: emit a host_observe per neighbor, then again with a
        // JS-side filter that drops 'b'.
        let body = "\
            var all = useNeighbors();\n\
            for (var i = 0; i < all.length; i++) {\n\
              host_observe('all:' + all[i]);\n\
            }\n\
            var filtered = useNeighbors(function(id) { return id !== 'b'; });\n\
            for (var j = 0; j < filtered.length; j++) {\n\
              host_observe('filtered:' + filtered[j]);\n\
            }\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Hub", body)
            .expect("eval");

        // Define the hub component on JSC.
        let definition = build_js_node_definition(
            "Hub".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        // Only register the hub node — the other 3 are pure topology
        // (no scheduler entry needed for this read-only test).
        scheduler
            .register_node(definition, "hub".to_string(), Attrs::new(), 0_u32)
            .expect("register hub");

        scheduler.process_batch().expect("batch");

        let observed = observations.lock();
        // useNeighbors() should yield {a, b, c} in some order — assert
        // each is present, and 'all:' has 3 entries while 'filtered:'
        // has 2 (no 'b').
        let all_count = observed.iter().filter(|s| s.starts_with("all:")).count();
        let filt_count = observed.iter().filter(|s| s.starts_with("filtered:")).count();
        assert_eq!(all_count, 3, "useNeighbors() returned 3 neighbors; observed: {observed:?}");
        assert_eq!(filt_count, 2, "filter dropped 'b'; observed: {observed:?}");
        assert!(
            observed.iter().any(|s| s == "all:a")
                && observed.iter().any(|s| s == "all:b")
                && observed.iter().any(|s| s == "all:c"),
            "all neighbors present unfiltered: {observed:?}"
        );
        assert!(
            !observed.iter().any(|s| s == "filtered:b"),
            "'b' filtered out: {observed:?}"
        );

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-β: `useTopology(handler, filter?)` registers a JS
    /// handler that fires on graph mutations. We trigger a mutation
    /// post-mount by adding a node directly to the underlying graph,
    /// which dispatches a `NodeAdded` event through
    /// `dispatch_topology_event` on the next process_batch.
    #[test]
    fn phase3e_beta_use_topology_handler_fires_on_node_addition() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("watcher".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_topology", build_host_topology_callback())
                .expect("host_topology");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: register useTopology with no filter. handler observes
        // the event's kind + nodeId.
        let body = "\
            useTopology(function(event) {\n\
              if (event.kind === 'node-added') {\n\
                host_observe('saw:' + event.nodeId);\n\
              }\n\
            });\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Watcher", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Watcher".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "watcher".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // Initial mount registers the topology handler.
        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("first batch");
        }

        // Add a new node directly to the underlying graph — the
        // scheduler's topology listener (`on_topology_change`) catches
        // the event into `pending_topology`, which is dispatched on
        // the next `process_batch`.
        scheduler.graph.borrow_mut().add_node("newcomer".to_string(), Attrs::new());

        // Process the event.
        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("second batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "saw:newcomer"),
                "useTopology handler observed the NodeAdded event for 'newcomer'; observed: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-β: `useAcceptEdge()` returns a function that
    /// enqueues a Phase-9 AcceptEdge mutation; the next batch round
    /// applies it and the active node's neighbors snapshot reflects
    /// the new edge.
    #[test]
    fn phase3e_beta_use_accept_edge_creates_edge() {
        let _g = engine_lock();
        reset_global_state();

        // Two disconnected nodes: 'host' and 'guest'.
        let mut graph = CommunicationGraph::new();
        graph.add_node("host".to_string(), Attrs::new());
        graph.add_node("guest".to_string(), Attrs::new());
        // No edge initially.

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_neighbors", build_host_neighbors_callback())
                .expect("host_neighbors");
            e.register_callback(iso, "host_accept_edge", build_host_accept_edge_callback())
                .expect("host_accept_edge");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: on first render, accept edge to 'guest'. On subsequent
        // renders, observe whether 'guest' is now in our neighbors.
        let body = "\
            const [counter, setCounter] = useState(0);\n\
            var nbrs = useNeighbors();\n\
            host_observe('render:' + counter + ':nbrs=' + nbrs.length);\n\
            if (counter === 0) {\n\
              var acceptEdge = useAcceptEdge();\n\
              acceptEdge('guest');\n\
              setCounter(1);\n\
            }\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Host", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Host".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "host".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // process_batch — first render observes 0 neighbors, accepts
        // edge, dirty-marks self via setCounter(1). The pending
        // mutation is drained at the start of the next cascade round;
        // re-render observes 1 neighbor.
        scheduler.process_batch().expect("batch");

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "render:0:nbrs=0"),
            "first render saw 0 neighbors; observed: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "render:1:nbrs=1"),
            "after accept-edge mutation drained, second render saw 1 neighbor; observed: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-γ: `useSpawnChild()` returns a function that, when
    /// invoked with a spec containing a JS component body, registers
    /// the body via the `__hostCallbacks` token registry, hands a
    /// `ComposeChildSpec` to `core_use_spawn_child`, and the
    /// scheduler materialises the child node on the next cascade
    /// round. The child's body executes on JSC like any other node.
    #[test]
    fn phase3e_gamma_use_spawn_child_creates_dynamic_node() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("parent".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_spawn_child", build_host_spawn_child_callback())
                .expect("host_spawn_child");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: on first render, spawn a child whose body just
        // observes its own id. State counter triggers a re-render
        // (so we can verify the child mounted on the second round).
        let body = "\
            const [count, setCount] = useState(0);\n\
            host_observe('parent:' + count);\n\
            if (count === 0) {\n\
              var spawn = useSpawnChild();\n\
              spawn({\n\
                id: 'child-1',\n\
                component: function(ctx) {\n\
                  host_observe('child:' + ctx.id);\n\
                  return { accepts: [], emits: [] };\n\
                },\n\
                props: { label: 'first' },\n\
                connectTo: ['parent']\n\
              });\n\
              setCount(1);\n\
            }\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Parent", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Parent".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "parent".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // process_batch — first render observes parent:0, queues
        // SpawnChild('child-1') + setCount(1). The cascade drains
        // the mutation (creates child-1) and re-evaluates parent.
        // Then evaluates child-1 once. observations include both
        // parent renders + the child's body.
        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "parent:0"),
            "parent's first render observed; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "parent:1"),
            "parent re-rendered after setCount; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "child:child-1"),
            "spawned child's body executed once with the right ctx.id; observations: {observed:?}"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-γ: `useUnmountSelf()` returns a function that
    /// schedules an `UnmountNode` mutation for the active node. After
    /// the cascade drains, the node is gone — subsequent renders
    /// don't fire (and a new mount of the same id would start fresh).
    #[test]
    fn phase3e_gamma_use_unmount_self_removes_node() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("ephemeral".to_string(), Attrs::new());
        graph.add_node("witness".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(
                iso,
                "host_unmount_self",
                build_host_unmount_self_callback(),
            )
            .expect("host_unmount_self");
            e.register_callback(iso, "host_topology", build_host_topology_callback())
                .expect("host_topology");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // 'ephemeral' unmounts itself on the first render. 'witness'
        // observes the NodeRemoved event via useTopology.
        let ephemeral_body = "\
            const [count, _setCount] = useState(0);\n\
            host_observe('ephemeral-render:' + count);\n\
            if (count === 0) {\n\
              var unmount = useUnmountSelf();\n\
              unmount();\n\
            }\n\
            return { accepts: [], emits: [] };\n\
        ";
        let witness_body = "\
            useTopology(function(event) {\n\
              if (event.kind === 'node-removed') {\n\
                host_observe('witness:removed:' + event.nodeId);\n\
              }\n\
            });\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_eph = engine_rc
            .borrow_mut()
            .eval_component(iso, "Ephemeral", ephemeral_body)
            .expect("eval ephemeral");
        let fn_wit = engine_rc
            .borrow_mut()
            .eval_component(iso, "Witness", witness_body)
            .expect("eval witness");

        let def_eph = build_js_node_definition(
            "Ephemeral".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_eph,
        );
        let def_wit = build_js_node_definition(
            "Witness".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_wit,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_eph, "ephemeral".to_string(), Attrs::new(), 0_u32)
            .expect("register ephemeral");
        scheduler
            .register_node(def_wit, "witness".to_string(), Attrs::new(), 0_u32)
            .expect("register witness");

        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "ephemeral-render:0"),
            "ephemeral's first render fired; observations: {observed:?}"
        );
        // After the unmount mutation drains, ephemeral's body should
        // not re-render (no observation of 'ephemeral-render:1').
        assert!(
            !observed.iter().any(|s| s == "ephemeral-render:1"),
            "ephemeral did NOT re-render after unmount; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "witness:removed:ephemeral"),
            "witness observed the NodeRemoved event for ephemeral; observations: {observed:?}"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-γ: `useCompose()` declares the active node's
    /// composed-children list. Specs that aren't yet registered are
    /// spawned (same as useSpawnChild for each one); already-registered
    /// IDs are kept. Tests the multi-spec dispatch.
    #[test]
    fn phase3e_gamma_use_compose_spawns_multiple_children() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("composer".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_compose", build_host_compose_callback())
                .expect("host_compose");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: first render declares 3 children via useCompose.
        let body = "\
            const [count, setCount] = useState(0);\n\
            host_observe('composer:' + count);\n\
            if (count === 0) {\n\
              var compose = useCompose();\n\
              var makeChild = function(label) {\n\
                return function(ctx) {\n\
                  host_observe('child:' + label + ':' + ctx.id);\n\
                  return { accepts: [], emits: [] };\n\
                };\n\
              };\n\
              compose([\n\
                { id: 'c-a', component: makeChild('a'), connectTo: ['composer'] },\n\
                { id: 'c-b', component: makeChild('b'), connectTo: ['composer'] },\n\
                { id: 'c-c', component: makeChild('c'), connectTo: ['composer'] }\n\
              ]);\n\
              setCount(1);\n\
            }\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Composer", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Composer".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "composer".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        for label in &["a", "b", "c"] {
            let want = format!("child:{label}:c-{label}");
            assert!(
                observed.iter().any(|s| s == &want),
                "composed child '{label}' rendered with the right ctx.id; observations: {observed:?}"
            );
        }

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-δ: `ctx.directory.findById/findByRole/findByDepth/
    /// isAncestor` query the live `GraphDirectory` via TLS-installed
    /// host callbacks. Tests populate directory entries explicitly
    /// (rather than via `register_node`) so the role/depth/path
    /// fields are testable without requiring the full scheduler
    /// hierarchy machinery.
    #[test]
    fn phase3e_delta_ctx_directory_finders_query_live_directory() {
        use graphreact_core::DirectoryEntry;

        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("watcher".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(
                iso,
                "host_directory_find_by_id",
                build_host_directory_find_by_id_callback(),
            )
            .expect("host_directory_find_by_id");
            e.register_callback(
                iso,
                "host_directory_find_by_role",
                build_host_directory_find_by_role_callback(),
            )
            .expect("host_directory_find_by_role");
            e.register_callback(
                iso,
                "host_directory_find_by_depth",
                build_host_directory_find_by_depth_callback(),
            )
            .expect("host_directory_find_by_depth");
            e.register_callback(
                iso,
                "host_directory_is_ancestor",
                build_host_directory_is_ancestor_callback(),
            )
            .expect("host_directory_is_ancestor");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: query the directory and observe each result.
        let body = "\
            var entry = ctx.directory.findById('alice');\n\
            host_observe('alice-role:' + (entry === null ? 'NULL' : entry.role));\n\
            host_observe('alice-depth:' + (entry === null ? -1 : entry.depth));\n\
            var workers = ctx.directory.findByRole('Worker');\n\
            host_observe('workers-count:' + workers.length);\n\
            for (var i = 0; i < workers.length; i++) {\n\
              host_observe('worker:' + workers[i]);\n\
            }\n\
            var depth1 = ctx.directory.findByDepth(1);\n\
            host_observe('depth-1-count:' + depth1.length);\n\
            host_observe('alice-ancestor-of-bob:' + ctx.directory.isAncestor('alice', 'bob'));\n\
            host_observe('zombie-found:' + (ctx.directory.findById('nonexistent') === null ? 'no' : 'yes'));\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Watcher", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Watcher".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "watcher".to_string(), Attrs::new(), 0_u32)
            .expect("register watcher");

        // Populate directory directly with custom roles + depths +
        // ancestry paths so we can test all five finders.
        {
            let mut dir = scheduler.directory.borrow_mut();
            dir.add_entry(DirectoryEntry {
                id: "alice".to_string(),
                role: "Foreman".to_string(),
                depth: 0,
                path: vec!["alice".to_string()],
            });
            dir.add_entry(DirectoryEntry {
                id: "bob".to_string(),
                role: "Worker".to_string(),
                depth: 1,
                path: vec!["alice".to_string(), "bob".to_string()],
            });
            dir.add_entry(DirectoryEntry {
                id: "carol".to_string(),
                role: "Worker".to_string(),
                depth: 1,
                path: vec!["alice".to_string(), "carol".to_string()],
            });
        }

        let snapshot = Rc::new(GraphSnapshot::from_live(Rc::clone(&scheduler.graph), Rc::clone(&scheduler.manifests), Rc::clone(&scheduler.edge_accept_predicates)));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "alice-role:Foreman"),
            "findById('alice') returned the entry with role=Foreman; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "alice-depth:0"),
            "alice's depth marshalled correctly; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "workers-count:2"),
            "findByRole('Worker') returned 2 entries; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "worker:bob"),
            "bob is a Worker; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "worker:carol"),
            "carol is a Worker; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "depth-1-count:2"),
            "findByDepth(1) returned 2 entries; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "alice-ancestor-of-bob:true"),
            "isAncestor('alice', 'bob') is true; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "zombie-found:no"),
            "findById('nonexistent') is null; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-δ: `useGraphQuery(queryFn, deps)` exposes a
    /// `__graphQueryProxy` to the query function with `neighbors`,
    /// `hasNode`, `hasEdge`, `nodeCount` accessors backed by a static
    /// graph snapshot captured at mount time. Wraps useMemo, so
    /// deps-unchanged renders return the cached query result.
    #[test]
    fn phase3e_delta_use_graph_query_returns_proxy_results() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("hub".to_string(), Attrs::new());
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());
        graph.add_edge("hub".to_string(), "a".to_string(), None, Attrs::new());
        graph.add_edge("hub".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(
                iso,
                "host_graph_neighbors",
                build_host_graph_neighbors_callback(),
            )
            .expect("host_graph_neighbors");
            e.register_callback(iso, "host_graph_has_node", build_host_graph_has_node_callback())
                .expect("host_graph_has_node");
            e.register_callback(iso, "host_graph_has_edge", build_host_graph_has_edge_callback())
                .expect("host_graph_has_edge");
            e.register_callback(
                iso,
                "host_graph_node_count",
                build_host_graph_node_count_callback(),
            )
            .expect("host_graph_node_count");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: useGraphQuery returns {hubNeighborCount, hasA, hasZombie,
        // edgeAB, edgeAC, totalNodes} in one call.
        let body = "\
            var q = useGraphQuery(function(g) {\n\
              return {\n\
                hubNeighborCount: g.neighbors('hub').length,\n\
                hasA: g.hasNode('a'),\n\
                hasZombie: g.hasNode('zombie'),\n\
                edgeAB: g.hasEdge('a', 'b'),\n\
                edgeAC: g.hasEdge('a', 'c'),\n\
                totalNodes: g.nodeCount()\n\
              };\n\
            }, [42]);\n\
            host_observe('hubNbrs:' + q.hubNeighborCount);\n\
            host_observe('hasA:' + q.hasA);\n\
            host_observe('hasZombie:' + q.hasZombie);\n\
            host_observe('edgeAB:' + q.edgeAB);\n\
            host_observe('edgeAC:' + q.edgeAC);\n\
            host_observe('totalNodes:' + q.totalNodes);\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Query", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Query".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "hub".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        let snapshot = Rc::new(GraphSnapshot::from_live(Rc::clone(&scheduler.graph), Rc::clone(&scheduler.manifests), Rc::clone(&scheduler.edge_accept_predicates)));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        // hub→a + hub→b + a→b. Three add_edge calls; the graph treats
        // edges as undirected so 'hub' has 2 neighbors and 'a' has 2.
        assert!(
            observed.iter().any(|s| s == "hubNbrs:2"),
            "hub has 2 neighbors via the snapshot; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "hasA:true"),
            "hasNode('a') = true; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "hasZombie:false"),
            "hasNode('zombie') = false; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "edgeAB:true"),
            "hasEdge('a','b') = true; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "edgeAC:false"),
            "hasEdge('a','c') = false (no such edge); observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "totalNodes:3"),
            "nodeCount() = 3; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-ε: `useSource(timerSource(N))` JS surface decodes a
    /// descriptor and dispatches it to `host_use_source`. We test
    /// the wiring with a custom `host_use_source` stub that captures
    /// the descriptor — production source-firing (which requires a
    /// tokio runtime + async sleep to observe) is exercised by
    /// graphreact-core's existing source tests; this test verifies
    /// the JS surface marshals correctly.
    #[test]
    fn phase3e_epsilon_use_source_dispatches_descriptor_to_host() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("ticker".to_string(), Attrs::new());

        let captured: std::sync::Arc<parking_lot::Mutex<Vec<(String, u64)>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let captured_clone = std::sync::Arc::clone(&captured);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            // Custom host_use_source that captures the descriptor for
            // assertion. Replaces the production callback; we're
            // testing the JS marshalling path, not the core_use_source
            // integration.
            e.register_callback(
                iso,
                "host_use_source",
                Box::new(move |args| {
                    let HostValue::Object(pairs) = args.first().cloned().unwrap_or(HostValue::Null)
                    else {
                        return Ok(HostValue::Null);
                    };
                    let mut kind = String::new();
                    let mut value: u64 = 0;
                    for (k, v) in pairs {
                        match (k.as_str(), v) {
                            ("kind", HostValue::String(s)) => kind = s,
                            ("intervalMs" | "delayMs", HostValue::Number(n)) => value = n as u64,
                            _ => {}
                        }
                    }
                    captured_clone.lock().push((kind, value));
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_use_source");
            iso
        };

        // Body: register both timer and delay sources via useSource.
        let body = "\
            useSource(function() { return timerSource(50); }, []);\n\
            useSource(function() { return delaySource(120); }, []);\n\
            return { accepts: ['tick', 'timeout'], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Ticker", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Ticker".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "ticker".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        scheduler.process_batch().expect("batch");

        let captured_descriptors = captured.lock();
        assert_eq!(
            captured_descriptors.as_slice(),
            &[
                ("timer".to_string(), 50_u64),
                ("delay".to_string(), 120_u64),
            ],
            "useSource dispatched both descriptors in order; captured: {captured_descriptors:?}"
        );

        drop(captured_descriptors);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-ζ: `useMonitor(targetId)` registers a monitor; when
    /// the target unmounts, the active node receives a `__DOWN__`
    /// system message. Body uses `useMessage('__DOWN__', handler)`
    /// to observe.
    #[test]
    fn phase3e_zeta_use_monitor_delivers_down_on_unmount() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("observer".to_string(), Attrs::new());
        graph.add_node("target".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_message", build_host_message_callback())
                .expect("host_message");
            e.register_callback(iso, "host_monitor", build_host_monitor_callback())
                .expect("host_monitor");
            e.register_callback(
                iso,
                "host_unmount_self",
                build_host_unmount_self_callback(),
            )
            .expect("host_unmount_self");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // observer: monitor target + register __DOWN__ handler.
        let obs_body = "\
            const [_count, _setCount] = useState(0);\n\
            var mref = useMonitor('target');\n\
            host_observe('mref:' + (mref === null ? 'null' : 'set'));\n\
            useMessage('__DOWN__', function(msg) {\n\
              host_observe('down:' + msg.payload.target + ':' + msg.payload.reason);\n\
            });\n\
            return { accepts: ['__DOWN__'], emits: [] };\n\
        ";
        // target: unmounts itself on first render.
        let tgt_body = "\
            const [count, _setCount] = useState(0);\n\
            if (count === 0) {\n\
              var unmount = useUnmountSelf();\n\
              unmount();\n\
            }\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_obs = engine_rc
            .borrow_mut()
            .eval_component(iso, "Observer", obs_body)
            .expect("eval observer");
        let fn_tgt = engine_rc
            .borrow_mut()
            .eval_component(iso, "Target", tgt_body)
            .expect("eval target");

        let def_obs = build_js_node_definition(
            "Observer".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_obs,
        );
        let def_tgt = build_js_node_definition(
            "Target".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_tgt,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_obs, "observer".to_string(), Attrs::new(), 0_u32)
            .expect("register observer");
        scheduler
            .register_node(def_tgt, "target".to_string(), Attrs::new(), 0_u32)
            .expect("register target");

        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "mref:set"),
            "useMonitor returned a non-null monitor ref; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "down:target:normal"),
            "observer received __DOWN__ message after target unmounted; observations: {observed:?}"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-ζ: `useLink(targetId)` returns true on success;
    /// `useGroupJoin(group)` registers the active node in the group.
    /// Tests the wiring without requiring the full link/group event
    /// flows.
    #[test]
    fn phase3e_zeta_use_link_and_group_join_register_cleanly() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_link", build_host_link_callback())
                .expect("host_link");
            e.register_callback(iso, "host_unlink", build_host_unlink_callback())
                .expect("host_unlink");
            e.register_callback(iso, "host_group_join", build_host_group_join_callback())
                .expect("host_group_join");
            e.register_callback(iso, "host_group_leave", build_host_group_leave_callback())
                .expect("host_group_leave");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: link to b, join group, observe results.
        let body = "\
            var linked = useLink('b');\n\
            host_observe('linked:' + linked);\n\
            useGroupJoin('workers');\n\
            host_observe('joined-workers');\n\
            useUnlink('b');\n\
            host_observe('unlinked');\n\
            useGroupLeave('workers');\n\
            host_observe('left-workers');\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Linker", body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Linker".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        // 'b' needs a NodeDefinition registered with the scheduler
        // for `useLink('b')` to find it via id_to_key. A noop body
        // suffices.
        let noop_body = "return { accepts: [], emits: [] };\n";
        let fn_b = engine_rc
            .borrow_mut()
            .eval_component(iso, "B", noop_body)
            .expect("eval b");
        let def_b = build_js_node_definition(
            "B".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_b,
        );

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register a");
        scheduler
            .register_node(def_b, "b".to_string(), Attrs::new(), 0_u32)
            .expect("register b");

        scheduler.process_batch().expect("batch");

        let observed = observations.lock();
        assert_eq!(
            observed.as_slice(),
            &[
                "linked:true".to_string(),
                "joined-workers".to_string(),
                "unlinked".to_string(),
                "left-workers".to_string(),
            ],
            "supervision hooks all returned cleanly; observations: {observed:?}"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d: ping-pong bounce — both nodes emit on dep change,
    /// receive via `useMessage`, and increment state. State change
    /// drives `useEffect` deps to re-fire, which emits the next
    /// bounce. Bounded by `bounces < 3` guards on both sides for
    /// deterministic termination.
    #[test]
    fn phase3d_ping_pong_bounces_through_jsc() {
        let _g = engine_lock();
        reset_global_state();

        // Bidirectional ping ↔ pong. Both edges are needed because
        // `make_emit` resolves targets from `graph.neighbors(from)`,
        // which is direction-aware.
        let mut graph = CommunicationGraph::new();
        graph.add_node("ping".to_string(), Attrs::new());
        graph.add_node("pong".to_string(), Attrs::new());
        graph.add_edge("ping".to_string(), "pong".to_string(), None, Attrs::new());
        graph.add_edge("pong".to_string(), "ping".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_effect", build_host_effect_callback())
                .expect("host_effect");
            e.register_callback(iso, "host_emit", build_host_emit_callback())
                .expect("host_emit");
            e.register_callback(iso, "host_message", build_host_message_callback())
                .expect("host_message");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Ping emits 'ping' on initial render and on each subsequent
        // increment; bounded by `bounces < 3`.
        let body_ping = "\
            const [bounces, setBounces] = useState(0);\n\
            const emit = useEmit();\n\
            useMessage('pong', function() {\n\
              host_observe('ping-recv:' + bounces);\n\
              if (bounces < 3) setBounces(bounces + 1);\n\
            });\n\
            useEffect(function() {\n\
              if (bounces < 3) {\n\
                host_observe('ping-emit:' + bounces);\n\
                emit('ping', { n: bounces });\n\
              }\n\
            }, [bounces]);\n\
            return { accepts: ['pong'], emits: ['ping'] };\n\
        ";
        // Pong only emits AFTER it has received at least one ping
        // (`bounces > 0`), so the bounce starts with ping's first
        // emission.
        let body_pong = "\
            const [bounces, setBounces] = useState(0);\n\
            const emit = useEmit();\n\
            useMessage('ping', function() {\n\
              host_observe('pong-recv:' + bounces);\n\
              if (bounces < 3) setBounces(bounces + 1);\n\
            });\n\
            useEffect(function() {\n\
              if (bounces > 0 && bounces <= 3) {\n\
                host_observe('pong-emit:' + bounces);\n\
                emit('pong', { n: bounces });\n\
              }\n\
            }, [bounces]);\n\
            return { accepts: ['ping'], emits: ['pong'] };\n\
        ";

        let fn_ping = engine_rc
            .borrow_mut()
            .eval_component(iso, "Ping", body_ping)
            .expect("eval ping");
        let fn_pong = engine_rc
            .borrow_mut()
            .eval_component(iso, "Pong", body_pong)
            .expect("eval pong");

        let def_ping = build_js_node_definition("Ping".into(), Rc::clone(&engine_rc), iso, fn_ping);
        let def_pong = build_js_node_definition("Pong".into(), Rc::clone(&engine_rc), iso, fn_pong);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_ping, "ping".to_string(), Attrs::new(), 0_u32)
            .expect("register ping");
        scheduler
            .register_node(def_pong, "pong".to_string(), Attrs::new(), 0_u32)
            .expect("register pong");

        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            let ping_emits: usize = observed
                .iter()
                .filter(|s| s.starts_with("ping-emit:"))
                .count();
            let ping_recvs: usize = observed
                .iter()
                .filter(|s| s.starts_with("ping-recv:"))
                .count();
            let pong_emits: usize = observed
                .iter()
                .filter(|s| s.starts_with("pong-emit:"))
                .count();
            let pong_recvs: usize = observed
                .iter()
                .filter(|s| s.starts_with("pong-recv:"))
                .count();

            assert!(
                ping_emits >= 2,
                "ping emitted at least twice; observed: {:?}",
                observed.as_slice()
            );
            assert!(
                pong_emits >= 2,
                "pong emitted at least twice; observed: {:?}",
                observed.as_slice()
            );
            assert!(
                ping_recvs >= 2,
                "ping received at least twice; observed: {:?}",
                observed.as_slice()
            );
            assert!(
                pong_recvs >= 2,
                "pong received at least twice; observed: {:?}",
                observed.as_slice()
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-A: `useSource(httpSource({port}))` and
    /// `useSource(watchSource({path}))` decode the nested `opts` field
    /// and dispatch the right `(kind, value)` to the host. Wiring test
    /// only — uses a custom `host_use_source` capture callback,
    /// matching the pattern from `phase3e_epsilon_use_source_dispatches_descriptor_to_host`.
    #[test]
    fn followon_a_use_source_decodes_http_and_watch_descriptors() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("server".to_string(), Attrs::new());

        // Capture (kind, port-as-u64, path) tuples. Port stored as u64
        // for uniform shape; path empty when the descriptor is http.
        let captured: std::sync::Arc<parking_lot::Mutex<Vec<(String, u64, String)>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let captured_clone = std::sync::Arc::clone(&captured);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(
                iso,
                "host_use_source",
                Box::new(move |args| {
                    let HostValue::Object(pairs) =
                        args.first().cloned().unwrap_or(HostValue::Null)
                    else {
                        return Ok(HostValue::Null);
                    };
                    let mut kind = String::new();
                    let mut port: u64 = 0;
                    let mut path = String::new();
                    for (k, v) in pairs {
                        match (k.as_str(), v) {
                            ("kind", HostValue::String(s)) => kind = s,
                            ("opts", HostValue::Object(opts_pairs)) => {
                                for (ok, ov) in opts_pairs {
                                    match (ok.as_str(), ov) {
                                        ("port", HostValue::Number(n)) => port = n as u64,
                                        ("path", HostValue::String(s)) => path = s,
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    captured_clone.lock().push((kind, port, path));
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_use_source");
            iso
        };

        let body = "\
            useSource(function() { return httpSource({ port: 8080 }); }, []);\n\
            useSource(function() { return watchSource({ path: '/tmp/grx-followon-a' }); }, []);\n\
            return { accepts: ['request', 'change'], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Server", body)
            .expect("eval");

        let definition =
            build_js_node_definition("Server".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "server".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        scheduler.process_batch().expect("batch");

        let descriptors = captured.lock();
        assert_eq!(
            descriptors.as_slice(),
            &[
                ("http".to_string(), 8080_u64, String::new()),
                ("watch".to_string(), 0_u64, "/tmp/grx-followon-a".to_string()),
            ],
            "useSource decoded both http+watch opts; captured: {descriptors:?}"
        );

        drop(descriptors);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-A: `useSource(watchSource({path}))` end-to-end —
    /// production `host_use_source` decoder builds a real
    /// [`crate::sources::watch_source`], `start()` registers a notify
    /// watcher, file write under the watched path delivers a `change`
    /// message that the body's `useMessage('change', ...)` handler
    /// observes.
    ///
    /// Polls process_batch in a 5s window because notify (FSEvents on
    /// macOS) has variable latency. Per [`feedback_timing_tests`]:
    /// generous margins for OS-event-driven assertions.
    #[test]
    fn followon_a_watch_source_fires_change_events_e2e() {
        let _g = engine_lock();
        reset_global_state();

        let tmpdir = tempfile::tempdir().expect("tempdir");
        let watch_path = tmpdir.path().to_string_lossy().to_string();

        let mut graph = CommunicationGraph::new();
        graph.add_node("watcher".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_message", build_host_message_callback())
                .expect("host_message");
            e.register_callback(iso, "host_use_source", build_host_use_source_callback())
                .expect("host_use_source");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // String-escape the path in case macOS gives us /var/folders/...
        // (no single-quote risk under tempfile, but defensive anyway).
        let escaped_path = watch_path.replace('\\', r"\\").replace('\'', r"\'");
        let body = format!(
            "\
            const [_count, _setCount] = useState(0);\n\
            useSource(function() {{ return watchSource({{ path: '{escaped_path}' }}); }}, []);\n\
            useMessage('change', function(msg) {{\n\
              host_observe('change');\n\
            }});\n\
            return {{ accepts: ['change'], emits: [] }};\n\
            ",
        );
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Watcher", &body)
            .expect("eval");

        let definition = build_js_node_definition(
            "Watcher".into(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "watcher".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // Initial batch — registers the source, calls start(), notify
        // begins watching the tempdir.
        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("first batch");
        }

        // Give notify a beat to install its watch before we touch the
        // file. FSEvents on macOS sometimes drops events arriving in
        // the same kqueue tick as the watch installation.
        std::thread::sleep(std::time::Duration::from_millis(200));
        std::fs::write(tmpdir.path().join("hello.txt"), b"hello")
            .expect("write to watched dir");

        // Poll for the change event. 5s budget covers FSEvents'
        // ~50-100ms latency with comfortable margin.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut observed_change = false;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
            {
                let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
                scheduler.process_batch().expect("poll batch");
            }
            if observations.lock().iter().any(|s| s == "change") {
                observed_change = true;
                break;
            }
        }

        let observed = observations.lock();
        assert!(
            observed_change,
            "watchSource fired a 'change' message after file write; observations: {observed:?}"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-B: `__graphQueryProxy` exposes path-finding +
    /// traversal accessors (`shortestPath`, `allPaths`, `aStarPath`,
    /// `bfs`, `dfs`) backed by graphreact-algorithms. Test graph is a
    /// chain `a-b-c-d` with a side branch `c-e` so traversal results
    /// are predictable.
    #[test]
    fn followon_b_use_graph_query_path_algorithms() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["a", "b", "c", "d", "e"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("b".to_string(), "c".to_string(), None, Attrs::new());
        graph.add_edge("c".to_string(), "d".to_string(), None, Attrs::new());
        graph.add_edge("c".to_string(), "e".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(
                iso,
                "host_graph_shortest_path",
                build_host_graph_shortest_path_callback(),
            )
            .expect("host_graph_shortest_path");
            e.register_callback(
                iso,
                "host_graph_all_paths",
                build_host_graph_all_paths_callback(),
            )
            .expect("host_graph_all_paths");
            e.register_callback(
                iso,
                "host_graph_a_star_path",
                build_host_graph_a_star_path_callback(),
            )
            .expect("host_graph_a_star_path");
            e.register_callback(iso, "host_graph_bfs", build_host_graph_bfs_callback())
                .expect("host_graph_bfs");
            e.register_callback(iso, "host_graph_dfs", build_host_graph_dfs_callback())
                .expect("host_graph_dfs");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: query each algorithm + observe a normalised string.
        // Paths are joined with '>' so the assertions stay readable.
        let body = "\
            var q = useGraphQuery(function(g) {\n\
              return {\n\
                sp: g.shortestPath('a', 'd'),\n\
                spMissing: g.shortestPath('a', 'zzz'),\n\
                ap: g.allPaths('a', 'e'),\n\
                star: g.aStarPath('a', 'e'),\n\
                bfsAll: g.bfs('a'),\n\
                bfsBounded: g.bfs('a', 1),\n\
                dfsAll: g.dfs('a')\n\
              };\n\
            }, [42]);\n\
            host_observe('sp:' + (q.sp === null ? 'null' : q.sp.join('>')));\n\
            host_observe('spMissing:' + (q.spMissing === null ? 'null' : q.spMissing.join('>')));\n\
            host_observe('ap-count:' + q.ap.length);\n\
            if (q.ap.length > 0) {\n\
              host_observe('ap-first:' + q.ap[0].join('>'));\n\
            }\n\
            host_observe('star:' + (q.star === null ? 'null' : q.star.join('>')));\n\
            host_observe('bfs-len:' + q.bfsAll.length);\n\
            host_observe('bfs-bounded:' + q.bfsBounded.join(','));\n\
            host_observe('dfs-len:' + q.dfsAll.length);\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Query", body)
            .expect("eval");

        let definition =
            build_js_node_definition("Query".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        let snapshot = Rc::new(GraphSnapshot::from_live(Rc::clone(&scheduler.graph), Rc::clone(&scheduler.manifests), Rc::clone(&scheduler.edge_accept_predicates)));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "sp:a>b>c>d"),
            "shortestPath(a,d) = a>b>c>d; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "spMissing:null"),
            "shortestPath(a,zzz) = null (missing target); observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "ap-count:1"),
            "allPaths(a,e) on this acyclic shape = 1 simple path; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "ap-first:a>b>c>e"),
            "allPaths(a,e)[0] = a>b>c>e; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "star:a>b>c>e"),
            "aStarPath(a,e) without heuristic == BFS shortest path; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "bfs-len:5"),
            "bfs(a) visits all 5 nodes in the connected component; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "bfs-bounded:a,b"),
            "bfs(a, 1) = [a, b] (depth-1 frontier); observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "dfs-len:5"),
            "dfs(a) visits all 5 nodes; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-B: `connectedComponents()` and `cycleDetection()`
    /// over a graph with two disjoint components (one cyclic, one
    /// acyclic singleton).
    #[test]
    fn followon_b_use_graph_query_components_and_cycles() {
        let _g = engine_lock();
        reset_global_state();

        // Component 1: triangle x-y-z (cyclic).
        // Component 2: isolated node w.
        let mut graph = CommunicationGraph::new();
        for id in ["x", "y", "z", "w"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("x".to_string(), "y".to_string(), None, Attrs::new());
        graph.add_edge("y".to_string(), "z".to_string(), None, Attrs::new());
        graph.add_edge("z".to_string(), "x".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(
                iso,
                "host_graph_connected_components",
                build_host_graph_connected_components_callback(),
            )
            .expect("host_graph_connected_components");
            e.register_callback(
                iso,
                "host_graph_cycle_detection",
                build_host_graph_cycle_detection_callback(),
            )
            .expect("host_graph_cycle_detection");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        let body = "\
            var q = useGraphQuery(function(g) {\n\
              return {\n\
                comps: g.connectedComponents(),\n\
                cycle: g.cycleDetection()\n\
              };\n\
            }, [42]);\n\
            host_observe('comp-count:' + q.comps.length);\n\
            // Each component is a sorted Vec of node IDs.\n\
            for (var i = 0; i < q.comps.length; i++) {\n\
              host_observe('comp:' + q.comps[i].join(','));\n\
            }\n\
            host_observe('hasCycle:' + q.cycle.hasCycle);\n\
            host_observe('cycle-len:' + (q.cycle.cycle === null ? -1 : q.cycle.cycle.length));\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "CC", body)
            .expect("eval");

        let definition =
            build_js_node_definition("CC".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "x".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        let snapshot = Rc::new(GraphSnapshot::from_live(Rc::clone(&scheduler.graph), Rc::clone(&scheduler.manifests), Rc::clone(&scheduler.edge_accept_predicates)));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "comp-count:2"),
            "two connected components; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "comp:w"),
            "isolated node w forms its own component; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "comp:x,y,z"),
            "x/y/z form one (sorted) component; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "hasCycle:true"),
            "triangle x-y-z is detected as a cycle; observations: {observed:?}"
        );
        assert!(
            observed
                .iter()
                .any(|s| s.starts_with("cycle-len:") && !s.ends_with(":-1") && !s.ends_with(":0")),
            "cycle edge list non-empty; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-C: `__graphQueryProxy` reads the LIVE
    /// `Scheduler::graph` (now `Rc<RefCell<_>>`) at call time, so a
    /// node spawned via `useSpawnChild` becomes visible to a
    /// subsequent `useGraphQuery` in the same `process_batch` cascade.
    /// Pre-followon-C the snapshot was rebuilt at mount time, so the
    /// post-spawn nodeCount stayed at 1 — a correctness bug for any
    /// system that mutates topology and then queries it. This test is
    /// the regression pin for the live-handle refactor.
    #[test]
    fn followon_c_use_graph_query_sees_dynamic_spawn_child() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("parent".to_string(), Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(iso, "host_spawn_child", build_host_spawn_child_callback())
                .expect("host_spawn_child");
            e.register_callback(
                iso,
                "host_graph_node_count",
                build_host_graph_node_count_callback(),
            )
            .expect("host_graph_node_count");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Body: read nodeCount via useGraphQuery on every render
        // (deps include `count` so memoisation re-evaluates after the
        // setCount flips it). On first render, spawn a child; on
        // second render, observe the post-spawn nodeCount. With
        // followon-C's live handle the second observation sees 2;
        // pre-followon-C it would have stayed at 1.
        let body = "\
            const [count, setCount] = useState(0);\n\
            var q = useGraphQuery(function(g) {\n\
              return { n: g.nodeCount() };\n\
            }, [count]);\n\
            host_observe('phase:' + count + ':nodeCount=' + q.n);\n\
            if (count === 0) {\n\
              var spawn = useSpawnChild();\n\
              spawn({\n\
                id: 'newcomer',\n\
                component: function(ctx) {\n\
                  host_observe('child:' + ctx.id);\n\
                  return { accepts: [], emits: [] };\n\
                },\n\
                props: {},\n\
                connectTo: ['parent']\n\
              });\n\
              setCount(1);\n\
            }\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Parent", body)
            .expect("eval");

        let definition =
            build_js_node_definition("Parent".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "parent".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        let snapshot = Rc::new(GraphSnapshot::from_live(Rc::clone(&scheduler.graph), Rc::clone(&scheduler.manifests), Rc::clone(&scheduler.edge_accept_predicates)));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "phase:0:nodeCount=1"),
            "first render observes only the parent (1 node); observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "child:newcomer"),
            "spawned child mounted with the right id; observations: {observed:?}"
        );
        // The acceptance bar: post-spawn render sees the live graph.
        // Pre-followon-C this was "phase:1:nodeCount=1" because the
        // snapshot was frozen at mount.
        assert!(
            observed.iter().any(|s| s == "phase:1:nodeCount=2"),
            "second render sees the spawned child via live __graphQueryProxy; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-D: every JS-side `__registerCallback` mint is
    /// reported through `host_track_token`, recorded under
    /// CURRENT_NODE_ID in `NODE_TOKENS`. On node unmount the runtime
    /// drains the bucket and dispatches `__releaseTokens([...])` to
    /// delete the entries from the JS-side `__hostCallbacks` map. This
    /// test exercises the full mechanism end-to-end:
    /// (1) mount a node whose body registers tokens via useEffect +
    ///     useMessage + useCallback (three body-time tokens minimum);
    /// (2) verify `NODE_TOKENS` records ≥3 under the node's id;
    /// (3) subscribe to topology events and run an unmount via
    ///     `scheduler.remove_node_at_runtime`;
    /// (4) drain the captured `NodeRemoved` events and call the
    ///     module's sweep helper for each one;
    /// (5) verify `NODE_TOKENS` is empty.
    #[test]
    fn followon_d_unmount_sweeps_host_callbacks_tokens() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("leaky".to_string(), Attrs::new());

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_state", build_host_state_callback())
                .expect("host_state");
            e.register_callback(iso, "host_state_set", build_host_state_set_callback())
                .expect("host_state_set");
            e.register_callback(iso, "host_effect", build_host_effect_callback())
                .expect("host_effect");
            e.register_callback(iso, "host_message", build_host_message_callback())
                .expect("host_message");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            // The piece under test.
            e.register_callback(
                iso,
                "host_track_token",
                build_host_track_token_callback(),
            )
            .expect("host_track_token");
            iso
        };

        // Body: registers three body-time tokens per render —
        // useEffect (effect fn token), useMessage (handler token),
        // useCallback (cached fn token via __hostCallbacks). One render
        // is sufficient; we want to confirm tokens land in NODE_TOKENS
        // and then disappear after the sweep.
        let body = "\
            useEffect(function() {}, []);\n\
            useMessage('ping', function(_msg) {});\n\
            var _cb = useCallback(function() {}, []);\n\
            return { accepts: ['ping'], emits: [] };\n\
        ";
        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "Leaky", body)
            .expect("eval");

        let definition =
            build_js_node_definition("Leaky".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "leaky".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // Subscribe to topology events so we can capture NodeRemoved
        // and feed it into the sweep — mirrors what
        // `mount_with_scheduler` wires for the live runtime.
        let removed: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let removed_for_listener = std::sync::Arc::clone(&removed);
        let _sub = scheduler
            .graph
            .borrow()
            .on_topology_change(move |event: &TopologyEvent| {
                if let TopologyEvent::NodeRemoved { node_id } = event {
                    removed_for_listener.lock().push(node_id.clone());
                }
            });

        {
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        // After mount, the three tokens should be tracked under "leaky".
        let tracked_after_mount = tracked_token_count();
        assert!(
            tracked_after_mount >= 3,
            "expected ≥3 body-time tokens tracked under 'leaky' (useEffect/useMessage/useCallback); got {tracked_after_mount}"
        );

        // Drive the unmount. `remove_node_at_runtime` calls into the
        // graph's `remove_node`, firing the topology listener.
        scheduler
            .remove_node_at_runtime("leaky")
            .expect("remove_node_at_runtime");

        // Mimic the JsRuntime sweep step: drain captured NodeRemoved
        // ids and call the helper for each.
        let drained: Vec<String> = std::mem::take(&mut *removed.lock());
        assert!(
            drained.iter().any(|id| id == "leaky"),
            "topology listener captured the NodeRemoved for 'leaky'; captured: {drained:?}"
        );
        for id in &drained {
            // Engine guard required so the helper can re-enter JSC to
            // dispatch __releaseTokens.
            let _engine_guard = EngineGuard::new(Rc::clone(&engine_rc), iso);
            release_tokens_for_node(&engine_rc, iso, id);
        }

        let tracked_after_sweep = tracked_token_count();
        assert_eq!(
            tracked_after_sweep, 0,
            "after sweep, NODE_TOKENS should be empty; got {tracked_after_sweep}"
        );

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-E: Tier 2 BFS over policy-derived directed
    /// reachability. Tier 2 edges require BOTH a real Tier 1 edge AND
    /// matching emit/accept manifest types (per the followon-E
    /// shipping semantics — direct-messaging shortcuts are deferred
    /// to a phase-2 opt-in flag). This test demonstrates that Tier 2
    /// prunes Tier 1's adjacency by manifest compatibility: the chain
    /// producer→worker→sink (each link has both an edge AND an
    /// emit/accept type match for 'process') is reachable, while a
    /// bystander 'observer' (Tier 1-adjacent to producer but with no
    /// 'process' acceptance) is correctly excluded from Tier 2 even
    /// though Tier 1 BFS would include it.
    #[test]
    fn followon_e_tier2_bfs_finds_policy_reachable_peers() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["producer", "worker", "sink", "observer"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        // Tier 1 chain: producer↔worker↔sink. Bystander edge:
        // producer↔observer. Observer's manifest has no 'process'
        // overlap, so Tier 2('process') excludes it.
        graph.add_edge("producer".to_string(), "worker".to_string(), None, Attrs::new());
        graph.add_edge("worker".to_string(), "sink".to_string(), None, Attrs::new());
        graph.add_edge(
            "producer".to_string(),
            "observer".to_string(),
            None,
            Attrs::new(),
        );

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(iso, "host_tier2_bfs", build_host_tier2_bfs_callback())
                .expect("host_tier2_bfs");
            e.register_callback(
                iso,
                "host_tier2_node_count",
                build_host_tier2_node_count_callback(),
            )
            .expect("host_tier2_node_count");
            e.register_callback(
                iso,
                "host_tier2_neighbors",
                build_host_tier2_neighbors_callback(),
            )
            .expect("host_tier2_neighbors");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // A separate "querier" node observes Tier 2 BFS from 'producer'.
        // Querier renders at the highest height so all participants'
        // manifests are populated by the time it queries. Note: a
        // node's own manifest isn't in the snapshot during its own
        // render (it's installed AFTER evaluate_node returns), so
        // querier sees N-1 manifests where N = registered count. The
        // query target ('producer') is one of those N-1.
        graph.add_node("querier".to_string(), Attrs::new());

        let producer_body = "\
            return { accepts: [], emits: ['process'] };\n\
        ";
        let worker_body = "\
            return { accepts: ['process'], emits: ['process'] };\n\
        ";
        let sink_body = "\
            return { accepts: ['process'], emits: [] };\n\
        ";
        // Bystander — Tier 1-adjacent to producer but doesn't accept
        // 'process'. Tier 2 BFS for 'process' must skip it.
        let observer_body = "\
            return { accepts: ['gossip'], emits: [] };\n\
        ";
        let querier_body = "\
            host_observe('querier:t2-nodeCount=' + __makeTier2Proxy('process').nodeCount());\n\
            var bfs = __makeTier2Proxy('process').bfs('producer');\n\
            host_observe('querier:t2-bfs=' + bfs.join('>'));\n\
            // Cross-check: observer is reachable in Tier 1 from \n\
            // producer (real edge), but NOT in Tier 2 for 'process' \n\
            // (no manifest overlap).\n\
            host_observe('querier:t2-hasEdge(producer,observer)=' + __makeTier2Proxy('process').hasEdge('producer', 'observer'));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_p = engine_rc
            .borrow_mut()
            .eval_component(iso, "Producer", producer_body)
            .expect("eval producer");
        let fn_w = engine_rc
            .borrow_mut()
            .eval_component(iso, "Worker", worker_body)
            .expect("eval worker");
        let fn_s = engine_rc
            .borrow_mut()
            .eval_component(iso, "Sink", sink_body)
            .expect("eval sink");
        let fn_o = engine_rc
            .borrow_mut()
            .eval_component(iso, "Observer", observer_body)
            .expect("eval observer");
        let fn_q = engine_rc
            .borrow_mut()
            .eval_component(iso, "Querier", querier_body)
            .expect("eval querier");

        let def_p = build_js_node_definition("Producer".into(), Rc::clone(&engine_rc), iso, fn_p);
        let def_w = build_js_node_definition("Worker".into(), Rc::clone(&engine_rc), iso, fn_w);
        let def_s = build_js_node_definition("Sink".into(), Rc::clone(&engine_rc), iso, fn_s);
        let def_o = build_js_node_definition("Observer".into(), Rc::clone(&engine_rc), iso, fn_o);
        let def_q = build_js_node_definition("Querier".into(), Rc::clone(&engine_rc), iso, fn_q);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_p, "producer".to_string(), Attrs::new(), 0_u32)
            .expect("register producer");
        scheduler
            .register_node(def_w, "worker".to_string(), Attrs::new(), 0_u32)
            .expect("register worker");
        scheduler
            .register_node(def_s, "sink".to_string(), Attrs::new(), 0_u32)
            .expect("register sink");
        scheduler
            .register_node(def_o, "observer".to_string(), Attrs::new(), 0_u32)
            .expect("register observer");
        scheduler
            .register_node(def_q, "querier".to_string(), Attrs::new(), 1_u32)
            .expect("register querier");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        // querier sees 4 manifests — producer, worker, sink, observer.
        // (Its OWN manifest isn't in the snapshot during its own
        // render — see "Manifest snapshot timing" doc comment on
        // Scheduler::manifests for the full lifecycle contract.)
        assert!(
            observed.iter().any(|s| s == "querier:t2-nodeCount=4"),
            "Tier 2 nodeCount sees the 4 manifests indexed before querier ran; observations: {observed:?}"
        );
        // Tier 2 directed graph for 'process':
        //   producer (emits 'process') → its Tier 1 neighbors that
        //     accept 'process': worker. observer is a Tier 1 neighbor
        //     but accepts 'gossip' not 'process' → excluded.
        //   worker (emits + accepts 'process') → its Tier 1 neighbors
        //     that accept 'process': sink. (producer accepts nothing
        //     so excluded.)
        //   sink, observer don't emit 'process' → no out edges.
        // BFS from producer: producer → worker → sink.
        assert!(
            observed
                .iter()
                .any(|s| s == "querier:t2-bfs=producer>worker>sink"),
            "Tier 2 BFS from producer follows the 'process'-typed Tier 1 chain (producer → \
             worker → sink) and excludes the bystander observer; observations: {observed:?}"
        );
        // Cross-check: Tier 1 has producer↔observer, but Tier 2 for
        // 'process' should report no edge (observer doesn't accept
        // 'process').
        assert!(
            observed
                .iter()
                .any(|s| s == "querier:t2-hasEdge(producer,observer)=false"),
            "Tier 2 'process' edge producer→observer must be false (observer accepts only \
             'gossip'); observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-E: `g.tier2(T).topologicalSort()` returns a
    /// directed Kahn's order over the policy-derived graph for T.
    /// Closes followon-B's `topologicalSort` deferral. Three-stage
    /// pipeline: producer emits 'tick', mid emits+accepts 'tick',
    /// terminal accepts 'tick'. Topo order must put producer first
    /// and terminal last.
    #[test]
    fn followon_e_tier2_topological_sort_directed_pipeline() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["producer", "mid", "terminal"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        // Tier 1 chain — Tier 2 needs the underlying edges for the
        // policy filter to apply.
        graph.add_edge("producer".to_string(), "mid".to_string(), None, Attrs::new());
        graph.add_edge("mid".to_string(), "terminal".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(
                iso,
                "host_tier2_topological_sort",
                build_host_tier2_topological_sort_callback(),
            )
            .expect("host_tier2_topological_sort");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // Same querier-renders-last pattern as the BFS test.
        graph.add_node("querier".to_string(), Attrs::new());

        let producer_body = "\
            return { accepts: [], emits: ['tick'] };\n\
        ";
        let mid_body = "\
            return { accepts: ['tick'], emits: ['tick'] };\n\
        ";
        let terminal_body = "\
            return { accepts: ['tick'], emits: [] };\n\
        ";
        let querier_body = "\
            var topo = __makeTier2Proxy('tick').topologicalSort();\n\
            host_observe('topo=' + (topo === null ? 'null' : topo.join('>')));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_p = engine_rc
            .borrow_mut()
            .eval_component(iso, "Producer", producer_body)
            .expect("eval producer");
        let fn_m = engine_rc
            .borrow_mut()
            .eval_component(iso, "Mid", mid_body)
            .expect("eval mid");
        let fn_t = engine_rc
            .borrow_mut()
            .eval_component(iso, "Terminal", terminal_body)
            .expect("eval terminal");
        let fn_q = engine_rc
            .borrow_mut()
            .eval_component(iso, "Querier", querier_body)
            .expect("eval querier");

        let def_p = build_js_node_definition("Producer".into(), Rc::clone(&engine_rc), iso, fn_p);
        let def_m = build_js_node_definition("Mid".into(), Rc::clone(&engine_rc), iso, fn_m);
        let def_t = build_js_node_definition("Terminal".into(), Rc::clone(&engine_rc), iso, fn_t);
        let def_q = build_js_node_definition("Querier".into(), Rc::clone(&engine_rc), iso, fn_q);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_p, "producer".to_string(), Attrs::new(), 0_u32)
            .expect("register producer");
        scheduler
            .register_node(def_m, "mid".to_string(), Attrs::new(), 0_u32)
            .expect("register mid");
        scheduler
            .register_node(def_t, "terminal".to_string(), Attrs::new(), 0_u32)
            .expect("register terminal");
        scheduler
            .register_node(def_q, "querier".to_string(), Attrs::new(), 1_u32)
            .expect("register querier");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        // Tier 2 directed graph for 'tick' at query time. Querier
        // queries from its own body, so querier's own manifest isn't
        // in the snapshot yet — the indexed manifests are
        // {producer, mid, terminal}. Tier 1 edges: producer↔mid,
        // mid↔terminal. Tier 2 edges (per-edge filter):
        //   producer (emits) → its Tier 1 neighbors that accept 'tick': mid.
        //   mid (emits + accepts) → its Tier 1 neighbors that accept 'tick': terminal.
        //   terminal: no out.
        // Kahn's:
        //   in_deg: mid=1, producer=0, terminal=1.
        //   sources sorted: producer.
        //   pop producer → mid:0 (push).
        //   pop mid → terminal:0 (push).
        //   pop terminal.
        // Expected: producer > mid > terminal.
        assert!(
            observed
                .iter()
                .any(|s| s == "topo=producer>mid>terminal"),
            "Tier 2 directed topo sort over the policy-filtered chain; observations: \
             {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-E: Cost-from-policy A* with the 'compliance'
    /// heuristic prefers paths through compliant nodes / encrypted
    /// edges over equivalent-length non-compliant paths. Diamond
    /// shape: a → {b, c} → d. b is compliant; c isn't (`compliant:
    /// false` node attr). Tier 2 BFS would pick either path
    /// arbitrarily; A* with `compliance` must pick a → b → d.
    #[test]
    fn followon_e_tier2_a_star_compliance_heuristic_picks_compliant_path() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("a".to_string(), Attrs::new());
        // Compliant intermediate.
        let mut b_attrs = Attrs::new();
        b_attrs.insert("compliant".to_string(), serde_json::json!(true));
        graph.add_node("b".to_string(), b_attrs);
        // Non-compliant intermediate — penalised by the heuristic.
        let mut c_attrs = Attrs::new();
        c_attrs.insert("compliant".to_string(), serde_json::json!(false));
        graph.add_node("c".to_string(), c_attrs);
        graph.add_node("d".to_string(), Attrs::new());
        // Real edges only along the diamond — Tier 2 emit/accept
        // manifests will mirror the same shape.
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("a".to_string(), "c".to_string(), None, Attrs::new());
        graph.add_edge("b".to_string(), "d".to_string(), None, Attrs::new());
        graph.add_edge("c".to_string(), "d".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(
                iso,
                "host_tier2_a_star_path",
                build_host_tier2_a_star_path_callback(),
            )
            .expect("host_tier2_a_star_path");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // a/b/c/d compose the diamond. A separate querier (height 1)
        // runs the A* query so all 4 manifests are populated by then.
        graph.add_node("querier".to_string(), Attrs::new());

        let a_body = "\
            return { accepts: [], emits: ['route'] };\n\
        ";
        let bc_body = "\
            return { accepts: ['route'], emits: ['route'] };\n\
        ";
        let d_body = "\
            return { accepts: ['route'], emits: [] };\n\
        ";
        let querier_body = "\
            var path = __makeTier2Proxy('route').aStarPath('a', 'd', { heuristic: 'compliance' });\n\
            host_observe('aStar:compliance=' + (path === null ? 'null' : path.join('>')));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_a = engine_rc
            .borrow_mut()
            .eval_component(iso, "A", a_body)
            .expect("eval a");
        let fn_b = engine_rc
            .borrow_mut()
            .eval_component(iso, "B", bc_body)
            .expect("eval b");
        let fn_c = engine_rc
            .borrow_mut()
            .eval_component(iso, "C", bc_body)
            .expect("eval c");
        let fn_d = engine_rc
            .borrow_mut()
            .eval_component(iso, "D", d_body)
            .expect("eval d");
        let fn_q = engine_rc
            .borrow_mut()
            .eval_component(iso, "Querier", querier_body)
            .expect("eval querier");

        let def_a = build_js_node_definition("A".into(), Rc::clone(&engine_rc), iso, fn_a);
        let def_b = build_js_node_definition("B".into(), Rc::clone(&engine_rc), iso, fn_b);
        let def_c = build_js_node_definition("C".into(), Rc::clone(&engine_rc), iso, fn_c);
        let def_d = build_js_node_definition("D".into(), Rc::clone(&engine_rc), iso, fn_d);
        let def_q = build_js_node_definition("Querier".into(), Rc::clone(&engine_rc), iso, fn_q);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_a, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register a");
        scheduler
            .register_node(def_b, "b".to_string(), Attrs::new(), 0_u32)
            .expect("register b");
        scheduler
            .register_node(def_c, "c".to_string(), Attrs::new(), 0_u32)
            .expect("register c");
        scheduler
            .register_node(def_d, "d".to_string(), Attrs::new(), 0_u32)
            .expect("register d");
        scheduler
            .register_node(def_q, "querier".to_string(), Attrs::new(), 1_u32)
            .expect("register querier");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed
                .iter()
                .any(|s| s == "aStar:compliance=a>b>d"),
            "compliance heuristic prefers compliant b over non-compliant c; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-E (full): directed `allPaths` + `connectedComponents`
    /// (Tarjan's SCC) + directed `cycleDetection`. Three nodes in a
    /// directed cycle a→b→c→a (via 'cmd' message type with bidirectional
    /// emit/accept manifests + bidirectional Tier 1 edges); SCC must
    /// return them as a single component, allPaths from a to c must
    /// surface both the direct hop (a→b→c) and the wrap-around
    /// (a→c via the b→c→...→a return chain — which doesn't yield a
    /// simple path, so just the direct path), and cycleDetection must
    /// return hasCycle=true.
    #[test]
    fn followon_e_tier2_directed_long_tail_accessors() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["a", "b", "c"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        // Triangle: a-b, b-c, c-a (Tier 1 undirected; Tier 2
        // direction emerges from the symmetric emit/accept manifests).
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("b".to_string(), "c".to_string(), None, Attrs::new());
        graph.add_edge("c".to_string(), "a".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(
                iso,
                "host_tier2_all_paths",
                build_host_tier2_all_paths_callback(),
            )
            .expect("host_tier2_all_paths");
            e.register_callback(
                iso,
                "host_tier2_connected_components",
                build_host_tier2_connected_components_callback(),
            )
            .expect("host_tier2_connected_components");
            e.register_callback(
                iso,
                "host_tier2_cycle_detection",
                build_host_tier2_cycle_detection_callback(),
            )
            .expect("host_tier2_cycle_detection");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        // a/b/c are full bidirectional participants for 'cmd'.
        graph.add_node("querier".to_string(), Attrs::new());
        let triangle_body = "\
            return { accepts: ['cmd'], emits: ['cmd'] };\n\
        ";
        let querier_body = "\
            var t2 = __makeTier2Proxy('cmd');\n\
            host_observe('ap-count:' + t2.allPaths('a', 'c').length);\n\
            // First simple path from a to c — direct hop a>b>c.\n\
            var aps = t2.allPaths('a', 'c');\n\
            if (aps.length > 0) host_observe('ap-first:' + aps[0].join('>'));\n\
            var sccs = t2.connectedComponents();\n\
            host_observe('scc-count:' + sccs.length);\n\
            if (sccs.length > 0) host_observe('scc-first:' + sccs[0].join(','));\n\
            var cd = t2.cycleDetection();\n\
            host_observe('hasCycle:' + cd.hasCycle);\n\
            host_observe('cycle-len:' + (cd.cycle === null ? -1 : cd.cycle.length));\n\
            return { accepts: [], emits: [] };\n\
        ";
        let fn_a = engine_rc
            .borrow_mut()
            .eval_component(iso, "A", triangle_body)
            .expect("eval a");
        let fn_b = engine_rc
            .borrow_mut()
            .eval_component(iso, "B", triangle_body)
            .expect("eval b");
        let fn_c = engine_rc
            .borrow_mut()
            .eval_component(iso, "C", triangle_body)
            .expect("eval c");
        let fn_q = engine_rc
            .borrow_mut()
            .eval_component(iso, "Querier", querier_body)
            .expect("eval querier");

        let def_a = build_js_node_definition("A".into(), Rc::clone(&engine_rc), iso, fn_a);
        let def_b = build_js_node_definition("B".into(), Rc::clone(&engine_rc), iso, fn_b);
        let def_c = build_js_node_definition("C".into(), Rc::clone(&engine_rc), iso, fn_c);
        let def_q = build_js_node_definition("Querier".into(), Rc::clone(&engine_rc), iso, fn_q);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_a, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register a");
        scheduler
            .register_node(def_b, "b".to_string(), Attrs::new(), 0_u32)
            .expect("register b");
        scheduler
            .register_node(def_c, "c".to_string(), Attrs::new(), 0_u32)
            .expect("register c");
        scheduler
            .register_node(def_q, "querier".to_string(), Attrs::new(), 1_u32)
            .expect("register querier");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        // Tier 2 directed graph for 'cmd' (excluding querier — it
        // doesn't participate). Each edge is bidirectional in Tier 1
        // and policy permits both directions: a↔b, b↔c, c↔a → SIX
        // directed Tier 2 edges. allPaths from a to c (simple paths):
        //   - a → b → c
        //   - a → c (1-hop via direct edge — `c` accepts 'cmd' and
        //     `a` emits 'cmd', and a↔c is a Tier 1 edge)
        // So 2 simple paths total (sorted by length: [a, c] then [a, b, c]).
        assert!(
            observed.iter().any(|s| s == "ap-count:2"),
            "Tier 2 allPaths(a, c) returns 2 simple directed paths; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "ap-first:a>c"),
            "Shortest path first (sorted by length); observations: {observed:?}"
        );
        // SCC: a/b/c form a single strongly connected component
        // (every node reachable from every other via directed paths).
        // Querier isn't in the snapshot during its own render
        // (manifest-snapshot timing contract — see Scheduler::manifests
        // doc), so SCC iterates only the 3 triangle nodes.
        assert!(
            observed.iter().any(|s| s == "scc-count:1"),
            "1 SCC over the {{a,b,c}} triangle; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "scc-first:a,b,c"),
            "Triangle SCC sorted = a,b,c; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "hasCycle:true"),
            "Triangle a→b→c→a forms a directed cycle; observations: {observed:?}"
        );
        assert!(
            observed
                .iter()
                .any(|s| s.starts_with("cycle-len:") && !s.ends_with(":-1") && !s.ends_with(":0")),
            "Cycle edge list non-empty; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-E (full): `trust-attenuated` heuristic prefers
    /// high-trust edges. Two parallel paths a→b→d and a→c→d; b's
    /// edges have high trust (1.0), c's have low trust (0.1). A* with
    /// `trust-attenuated` picks the b path (cost ~2) over the c path
    /// (cost ~20).
    #[test]
    fn followon_e_tier2_a_star_trust_attenuated_prefers_high_trust_path() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["a", "b", "c", "d", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        // High-trust path: a-b-d.
        let mut high = Attrs::new();
        high.insert("trust".to_string(), serde_json::json!(1.0));
        graph.add_edge("a".to_string(), "b".to_string(), None, high.clone());
        graph.add_edge("b".to_string(), "d".to_string(), None, high);
        // Low-trust path: a-c-d.
        let mut low = Attrs::new();
        low.insert("trust".to_string(), serde_json::json!(0.1));
        graph.add_edge("a".to_string(), "c".to_string(), None, low.clone());
        graph.add_edge("c".to_string(), "d".to_string(), None, low);

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
                .expect("host_memo_check");
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
                .expect("host_memo_store");
            e.register_callback(
                iso,
                "host_tier2_a_star_path",
                build_host_tier2_a_star_path_callback(),
            )
            .expect("host_tier2_a_star_path");
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("host_observe");
            iso
        };

        let a_body = "return { accepts: [], emits: ['vouch'] };\n";
        let bc_body = "return { accepts: ['vouch'], emits: ['vouch'] };\n";
        let d_body = "return { accepts: ['vouch'], emits: [] };\n";
        let querier_body = "\
            var path = __makeTier2Proxy('vouch').aStarPath('a', 'd', { heuristic: 'trust-attenuated' });\n\
            host_observe('aStar:trust=' + (path === null ? 'null' : path.join('>')));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_a = engine_rc.borrow_mut().eval_component(iso, "A", a_body).unwrap();
        let fn_b = engine_rc.borrow_mut().eval_component(iso, "B", bc_body).unwrap();
        let fn_c = engine_rc.borrow_mut().eval_component(iso, "C", bc_body).unwrap();
        let fn_d = engine_rc.borrow_mut().eval_component(iso, "D", d_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Querier", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("A".into(), Rc::clone(&engine_rc), iso, fn_a),
            "a".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("B".into(), Rc::clone(&engine_rc), iso, fn_b),
            "b".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("C".into(), Rc::clone(&engine_rc), iso, fn_c),
            "c".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("D".into(), Rc::clone(&engine_rc), iso, fn_d),
            "d".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Querier".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        // High-trust path: a-b-d, cost = 1/1 + 1/1 = 2. Low-trust:
        // a-c-d, cost = 1/0.1 + 1/0.1 = 20. A* picks the cheaper one.
        assert!(
            observed.iter().any(|s| s == "aStar:trust=a>b>d"),
            "trust-attenuated heuristic picks the high-trust path; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-E (full): `prerequisite-distance` heuristic
    /// prefers low-difficulty intermediates. Two paths a→easy→target
    /// and a→hard→target. easy.difficulty=1, hard.difficulty=10. A*
    /// picks the easy path.
    #[test]
    fn followon_e_tier2_a_star_prerequisite_distance_prefers_easy_path() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("start".to_string(), Attrs::new());
        let mut easy_attrs = Attrs::new();
        easy_attrs.insert("difficulty".to_string(), serde_json::json!(1.0));
        graph.add_node("easy".to_string(), easy_attrs);
        let mut hard_attrs = Attrs::new();
        hard_attrs.insert("difficulty".to_string(), serde_json::json!(10.0));
        graph.add_node("hard".to_string(), hard_attrs);
        graph.add_node("target".to_string(), Attrs::new());
        graph.add_node("querier".to_string(), Attrs::new());
        graph.add_edge("start".to_string(), "easy".to_string(), None, Attrs::new());
        graph.add_edge("start".to_string(), "hard".to_string(), None, Attrs::new());
        graph.add_edge("easy".to_string(), "target".to_string(), None, Attrs::new());
        graph.add_edge("hard".to_string(), "target".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback()).unwrap();
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback()).unwrap();
            e.register_callback(
                iso,
                "host_tier2_a_star_path",
                build_host_tier2_a_star_path_callback(),
            ).unwrap();
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            ).unwrap();
            iso
        };

        let start_body = "return { accepts: [], emits: ['learn'] };\n";
        let middle_body = "return { accepts: ['learn'], emits: ['learn'] };\n";
        let target_body = "return { accepts: ['learn'], emits: [] };\n";
        let querier_body = "\
            var path = __makeTier2Proxy('learn').aStarPath('start', 'target', { heuristic: 'prerequisite-distance' });\n\
            host_observe('aStar:prereq=' + (path === null ? 'null' : path.join('>')));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_s = engine_rc.borrow_mut().eval_component(iso, "Start", start_body).unwrap();
        let fn_e = engine_rc.borrow_mut().eval_component(iso, "Easy", middle_body).unwrap();
        let fn_h = engine_rc.borrow_mut().eval_component(iso, "Hard", middle_body).unwrap();
        let fn_t = engine_rc.borrow_mut().eval_component(iso, "Target", target_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Querier", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("Start".into(), Rc::clone(&engine_rc), iso, fn_s),
            "start".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Easy".into(), Rc::clone(&engine_rc), iso, fn_e),
            "easy".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Hard".into(), Rc::clone(&engine_rc), iso, fn_h),
            "hard".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Target".into(), Rc::clone(&engine_rc), iso, fn_t),
            "target".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Querier".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        // Easy path cost: (1 + difficulty(easy)) + (1 + difficulty(target))
        //                 = (1 + 1) + (1 + 0) = 3.
        // Hard path cost: (1 + 10) + (1 + 0) = 12.
        assert!(
            observed.iter().any(|s| s == "aStar:prereq=start>easy>target"),
            "prerequisite-distance heuristic picks easy intermediate; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// M12.3a-followon-E (full): `g.tier2(T, {includeDirectoryShortcuts:
    /// true})` adds edges for every emit/accept pair regardless of
    /// Tier 1 edge presence — the canonical "Tier 2 = directory-augmented
    /// reachability" semantic. Test: producer + sink with NO Tier 1
    /// edge between them. Structural Tier 2 reports unreachable;
    /// directory-shortcut Tier 2 reports 1-hop reachable.
    #[test]
    fn followon_e_tier2_directory_shortcuts_flag_enables_edge_free_reachability() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("producer".to_string(), Attrs::new());
        graph.add_node("sink".to_string(), Attrs::new());
        graph.add_node("querier".to_string(), Attrs::new());
        // INTENTIONALLY no edge between producer and sink. They share
        // matching emit/accept manifests for 'broadcast' but cannot
        // talk via Tier 1 / structural Tier 2. Directory-shortcuts
        // flag enables direct-messaging-driven reachability.

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.register_callback(iso, "host_memo_check", build_host_memo_check_callback()).unwrap();
            e.register_callback(iso, "host_memo_store", build_host_memo_store_callback()).unwrap();
            e.register_callback(iso, "host_tier2_has_edge", build_host_tier2_has_edge_callback()).unwrap();
            e.register_callback(iso, "host_tier2_neighbors", build_host_tier2_neighbors_callback()).unwrap();
            e.register_callback(
                iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            ).unwrap();
            iso
        };

        let producer_body = "return { accepts: [], emits: ['broadcast'] };\n";
        let sink_body = "return { accepts: ['broadcast'], emits: [] };\n";
        let querier_body = "\
            // Structural Tier 2: producer and sink share no Tier 1 edge → no Tier 2 edge.\n\
            var structural = __makeTier2Proxy('broadcast');\n\
            host_observe('struct:hasEdge(producer,sink)=' + structural.hasEdge('producer', 'sink'));\n\
            host_observe('struct:nbrs(producer)=' + structural.neighbors('producer').join(','));\n\
            // Directory-shortcut flavor: every emitter ↔ every acceptor.\n\
            var ds = __makeTier2Proxy('broadcast', { includeDirectoryShortcuts: true });\n\
            host_observe('ds:hasEdge(producer,sink)=' + ds.hasEdge('producer', 'sink'));\n\
            host_observe('ds:nbrs(producer)=' + ds.neighbors('producer').join(','));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_p = engine_rc.borrow_mut().eval_component(iso, "Producer", producer_body).unwrap();
        let fn_s = engine_rc.borrow_mut().eval_component(iso, "Sink", sink_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Querier", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("Producer".into(), Rc::clone(&engine_rc), iso, fn_p),
            "producer".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Sink".into(), Rc::clone(&engine_rc), iso, fn_s),
            "sink".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Querier".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        // Structural Tier 2: no Tier 1 edge → no Tier 2 edge.
        assert!(
            observed.iter().any(|s| s == "struct:hasEdge(producer,sink)=false"),
            "structural Tier 2 reports no edge without Tier 1 backing; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "struct:nbrs(producer)="),
            "structural Tier 2 returns empty neighbors; observations: {observed:?}"
        );
        // Directory-shortcut Tier 2: emit/accept pair → 1-hop edge.
        assert!(
            observed.iter().any(|s| s == "ds:hasEdge(producer,sink)=true"),
            "directory-shortcut Tier 2 reports edge from emit/accept pair; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "ds:nbrs(producer)=sink"),
            "directory-shortcut Tier 2 returns sink as a neighbor; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // ---------------------------------------------------------------------
    // M12.3a-followon-F: Tier 3 (negotiation-aware planning) acceptance
    // ---------------------------------------------------------------------
    //
    // Substrate tests (4):
    //   * default-accept preserved when no predicate is installed
    //     (Tier 3 == Tier 2)
    //   * predicate accepts → pair appears in Tier 3
    //   * predicate rejects → pair excluded from Tier 3
    //   * `useAcceptEdge` Phase 9 mutation path consults the predicate
    //
    // Application-class worked examples (5 — one per Tier 3 user from the
    // application-space framing):
    //   * Self-Healing #1 — compliance heuristic, predicate vetoes
    //     compliance-incompatible failover
    //   * Adaptive Education #6 — neighbors enumeration, predicate
    //     vetoes prerequisite mismatches
    //   * Generative Worlds #4 — BFS, peace-treaty regions veto war
    //     propagation
    //   * Biological Pathways #7 — allPaths, off-target receptor vetoes
    //   * Reputation #8 — trust-attenuated A*, vouch-targets veto
    //     low-reputation links

    /// Helper: register the full Tier 2 + Tier 3 callback set on an
    /// isolate. All nine acceptance tests use the same registration
    /// pattern — pull it out so each test focuses on its own scenario.
    fn register_followon_f_callbacks(
        e: &mut JscEngine,
        iso: IsolateHandle,
        observe_callback: BoxCallback,
    ) {
        e.register_callback(iso, "host_memo_check", build_host_memo_check_callback())
            .expect("host_memo_check");
        e.register_callback(iso, "host_memo_store", build_host_memo_store_callback())
            .expect("host_memo_store");
        e.register_callback(iso, "host_track_token", build_host_track_token_callback())
            .expect("host_track_token");
        e.register_callback(
            iso,
            "host_edge_accept_handler",
            build_host_edge_accept_handler_callback(),
        )
        .expect("host_edge_accept_handler");
        e.register_callback(iso, "host_accept_edge", build_host_accept_edge_callback())
            .expect("host_accept_edge");
        e.register_callback(
            iso,
            "host_tier2_neighbors",
            build_host_tier2_neighbors_callback(),
        )
        .expect("host_tier2_neighbors");
        e.register_callback(
            iso,
            "host_tier2_has_edge",
            build_host_tier2_has_edge_callback(),
        )
        .expect("host_tier2_has_edge");
        e.register_callback(
            iso,
            "host_tier3_neighbors",
            build_host_tier3_neighbors_callback(),
        )
        .expect("host_tier3_neighbors");
        e.register_callback(
            iso,
            "host_tier3_has_edge",
            build_host_tier3_has_edge_callback(),
        )
        .expect("host_tier3_has_edge");
        e.register_callback(iso, "host_tier3_bfs", build_host_tier3_bfs_callback())
            .expect("host_tier3_bfs");
        e.register_callback(iso, "host_tier3_dfs", build_host_tier3_dfs_callback())
            .expect("host_tier3_dfs");
        e.register_callback(
            iso,
            "host_tier3_shortest_path",
            build_host_tier3_shortest_path_callback(),
        )
        .expect("host_tier3_shortest_path");
        e.register_callback(
            iso,
            "host_tier3_a_star_path",
            build_host_tier3_a_star_path_callback(),
        )
        .expect("host_tier3_a_star_path");
        e.register_callback(
            iso,
            "host_tier3_all_paths",
            build_host_tier3_all_paths_callback(),
        )
        .expect("host_tier3_all_paths");
        e.register_callback(iso, "host_observe", observe_callback)
            .expect("host_observe");
    }

    /// Substrate #1: when no node calls `useEdgeAcceptHandler`, Tier 3
    /// degrades to Tier 2 — every (s, u) emit/accept-compatible pair
    /// appears in Tier 3 because the default predicate is "accept".
    #[test]
    fn followon_f_use_edge_accept_handler_default_accept_when_no_predicate() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["src", "dst", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("src".to_string(), "dst".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            register_followon_f_callbacks(
                &mut e,
                iso,
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            );
            iso
        };

        let src_body = "return { accepts: [], emits: ['ping'] };\n";
        let dst_body = "return { accepts: ['ping'], emits: [] };\n";
        let querier_body = "\
            var t2 = __makeTier2Proxy('ping');\n\
            var t3 = __makeTier3Proxy('ping');\n\
            host_observe('t2:hasEdge=' + t2.hasEdge('src','dst'));\n\
            host_observe('t3:hasEdge=' + t3.hasEdge('src','dst'));\n\
            host_observe('t2:nbrs=' + t2.neighbors('src').join(','));\n\
            host_observe('t3:nbrs=' + t3.neighbors('src').join(','));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_s = engine_rc.borrow_mut().eval_component(iso, "Src", src_body).unwrap();
        let fn_d = engine_rc.borrow_mut().eval_component(iso, "Dst", dst_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Q", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("Src".into(), Rc::clone(&engine_rc), iso, fn_s),
            "src".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Dst".into(), Rc::clone(&engine_rc), iso, fn_d),
            "dst".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Q".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _g = EngineGuard::with_directory(
                Rc::clone(&engine_rc), iso,
                Rc::clone(&scheduler.directory), snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        for expected in [
            "t2:hasEdge=true", "t3:hasEdge=true",
            "t2:nbrs=dst", "t3:nbrs=dst",
        ] {
            assert!(
                observed.iter().any(|s| s == expected),
                "expected {expected:?}; observations: {observed:?}"
            );
        }
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Substrate #2: predicate returns true → Tier 3 includes the
    /// (s, u) pair (matches the Tier 2 baseline).
    #[test]
    fn followon_f_tier3_query_includes_predicate_accepted_pair() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["alice", "bob", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("alice".to_string(), "bob".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            register_followon_f_callbacks(
                &mut e, iso,
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            );
            iso
        };

        let alice_body = "return { accepts: [], emits: ['friend-request'] };\n";
        // Bob always-accepts. Tier 3 should include alice→bob.
        let bob_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return true; });\n\
            return { accepts: ['friend-request'], emits: [] };\n\
        ";
        let querier_body = "\
            var t3 = __makeTier3Proxy('friend-request');\n\
            host_observe('t3:hasEdge=' + t3.hasEdge('alice','bob'));\n\
            host_observe('t3:nbrs=' + t3.neighbors('alice').join(','));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_a = engine_rc.borrow_mut().eval_component(iso, "Alice", alice_body).unwrap();
        let fn_b = engine_rc.borrow_mut().eval_component(iso, "Bob", bob_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Q", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("Alice".into(), Rc::clone(&engine_rc), iso, fn_a),
            "alice".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Bob".into(), Rc::clone(&engine_rc), iso, fn_b),
            "bob".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Q".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _g = EngineGuard::with_directory(
                Rc::clone(&engine_rc), iso,
                Rc::clone(&scheduler.directory), snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(observed.iter().any(|s| s == "t3:hasEdge=true"),
            "predicate-accepted pair appears in Tier 3; observations: {observed:?}");
        assert!(observed.iter().any(|s| s == "t3:nbrs=bob"),
            "alice's tier 3 neighbors include bob; observations: {observed:?}");
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Substrate #3: predicate returns false → Tier 3 excludes the
    /// (s, u) pair even when Tier 2 would include it.
    #[test]
    fn followon_f_tier3_query_excludes_predicate_rejected_pair() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["alice", "bob", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("alice".to_string(), "bob".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            register_followon_f_callbacks(
                &mut e, iso,
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            );
            iso
        };

        let alice_body = "return { accepts: [], emits: ['friend-request'] };\n";
        // Bob refuses every requester for this msgType — Tier 3 must
        // exclude alice→bob even though Tier 2 includes it.
        let bob_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return false; });\n\
            return { accepts: ['friend-request'], emits: [] };\n\
        ";
        let querier_body = "\
            var t2 = __makeTier2Proxy('friend-request');\n\
            var t3 = __makeTier3Proxy('friend-request');\n\
            host_observe('t2:hasEdge=' + t2.hasEdge('alice','bob'));\n\
            host_observe('t3:hasEdge=' + t3.hasEdge('alice','bob'));\n\
            host_observe('t3:nbrs(alice)=' + t3.neighbors('alice').join(','));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_a = engine_rc.borrow_mut().eval_component(iso, "Alice", alice_body).unwrap();
        let fn_b = engine_rc.borrow_mut().eval_component(iso, "Bob", bob_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Q", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("Alice".into(), Rc::clone(&engine_rc), iso, fn_a),
            "alice".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Bob".into(), Rc::clone(&engine_rc), iso, fn_b),
            "bob".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Q".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _g = EngineGuard::with_directory(
                Rc::clone(&engine_rc), iso,
                Rc::clone(&scheduler.directory), snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(observed.iter().any(|s| s == "t2:hasEdge=true"),
            "Tier 2 baseline includes alice→bob (manifest match + Tier 1 edge); observations: {observed:?}");
        assert!(observed.iter().any(|s| s == "t3:hasEdge=false"),
            "Tier 3 excludes alice→bob because bob's predicate vetoes; observations: {observed:?}");
        assert!(observed.iter().any(|s| s == "t3:nbrs(alice)="),
            "alice has no Tier 3 neighbors after veto; observations: {observed:?}");
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Substrate #4: `useAcceptEdge` Phase 9 mutation path also
    /// consults the predicate. Source enqueues an AcceptEdge to a
    /// non-existent Tier 1 edge; target's predicate rejects → no edge
    /// added. Then a second target with no predicate accepts the
    /// equivalent mutation — sanity check that the veto is selective.
    #[test]
    fn followon_f_use_accept_edge_consults_predicate() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["src", "veto-target", "open-target"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        // src is NOT initially connected to either target. After
        // process_batch, the predicate vetoes src→veto-target and
        // allows src→open-target.

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            register_followon_f_callbacks(
                &mut e, iso,
                Box::new(|_args| Ok(HostValue::Null)),
            );
            iso
        };

        let src_body = "\
            var accept = useAcceptEdge();\n\
            useEffect(function() {\n\
                accept('veto-target');\n\
                accept('open-target');\n\
            }, []);\n\
            return { accepts: [], emits: [] };\n\
        ";
        let veto_target_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return false; });\n\
            return { accepts: [], emits: [] };\n\
        ";
        let open_target_body = "\
            return { accepts: [], emits: [] };\n\
        ";

        // Source needs use_effect support to defer the accept calls.
        engine_rc.borrow_mut().register_callback(
            iso, "host_state", build_host_state_callback()
        ).unwrap();
        engine_rc.borrow_mut().register_callback(
            iso, "host_state_set", build_host_state_set_callback()
        ).unwrap();
        engine_rc.borrow_mut().register_callback(
            iso, "host_effect", build_host_effect_callback()
        ).unwrap();

        let fn_s = engine_rc.borrow_mut().eval_component(iso, "Src", src_body).unwrap();
        let fn_v = engine_rc.borrow_mut().eval_component(iso, "Veto", veto_target_body).unwrap();
        let fn_o = engine_rc.borrow_mut().eval_component(iso, "Open", open_target_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        // Targets at height 0 (their predicates need to be installed
        // BEFORE drain_pending_mutations sees the AcceptEdge from src).
        // src at height 1 so its useEffect fires after both targets'
        // bodies have synced their predicates into the shared index.
        scheduler.register_node(
            build_js_node_definition("Veto".into(), Rc::clone(&engine_rc), iso, fn_v),
            "veto-target".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Open".into(), Rc::clone(&engine_rc), iso, fn_o),
            "open-target".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Src".into(), Rc::clone(&engine_rc), iso, fn_s),
            "src".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _g = EngineGuard::with_directory(
                Rc::clone(&engine_rc), iso,
                Rc::clone(&scheduler.directory), snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let g = scheduler.graph.borrow();
        assert!(
            !g.has_edge("src", "veto-target", None),
            "veto-target's predicate vetoed the AcceptEdge mutation"
        );
        assert!(
            g.has_edge("src", "open-target", None),
            "open-target has no predicate → default-accept; edge added"
        );
        drop(g);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Application class #1 (Self-Healing Infrastructure / Thymus):
    /// `g.tier3('write').aStarPath(primary, target, {heuristic:
    /// 'compliance'})` picks the compliance-preferred path. Failover
    /// targets that can't meet compliance veto via predicate.
    #[test]
    #[allow(non_snake_case)]
    fn followon_f_self_healing_compliance_aStarPath_with_predicate() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["primary", "compliant-failover", "noncompliant-failover", "querier"] {
            let mut attrs = Attrs::new();
            // compliant-failover marked compliant; noncompliant left default-true at attr level
            // but its predicate vetoes — different mechanism for the same rejection effect.
            if id == "compliant-failover" {
                attrs.insert("compliant".to_string(), serde_json::Value::Bool(true));
            }
            graph.add_node(id.to_string(), attrs);
        }
        // Tier 1: primary connects to BOTH failover candidates.
        graph.add_edge("primary".to_string(), "compliant-failover".to_string(), None, Attrs::new());
        graph.add_edge("primary".to_string(), "noncompliant-failover".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            register_followon_f_callbacks(
                &mut e, iso,
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            );
            iso
        };

        let primary_body = "return { accepts: [], emits: ['write'] };\n";
        // Compliant failover: predicate accepts.
        let compliant_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return true; });\n\
            return { accepts: ['write'], emits: [] };\n\
        ";
        // Noncompliant failover: predicate rejects (e.g., sees the
        // primary doesn't meet some compliance precondition).
        let noncompliant_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return false; });\n\
            return { accepts: ['write'], emits: [] };\n\
        ";
        let querier_body = "\
            var path = __makeTier3Proxy('write')\n\
                .aStarPath('primary', 'compliant-failover', { heuristic: 'compliance' });\n\
            host_observe('compliant-path=' + (path === null ? 'null' : path.join('>')));\n\
            var blocked = __makeTier3Proxy('write')\n\
                .aStarPath('primary', 'noncompliant-failover');\n\
            host_observe('noncompliant-path=' + (blocked === null ? 'null' : blocked.join('>')));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_p = engine_rc.borrow_mut().eval_component(iso, "P", primary_body).unwrap();
        let fn_c = engine_rc.borrow_mut().eval_component(iso, "C", compliant_body).unwrap();
        let fn_n = engine_rc.borrow_mut().eval_component(iso, "N", noncompliant_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Q", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("P".into(), Rc::clone(&engine_rc), iso, fn_p),
            "primary".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("C".into(), Rc::clone(&engine_rc), iso, fn_c),
            "compliant-failover".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_n),
            "noncompliant-failover".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Q".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _g = EngineGuard::with_directory(
                Rc::clone(&engine_rc), iso,
                Rc::clone(&scheduler.directory), snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "compliant-path=primary>compliant-failover"),
            "compliance-aware Tier 3 finds the compliant failover; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "noncompliant-path=null"),
            "noncompliant failover unreachable in Tier 3 — predicate vetoed; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Application class #6 (Adaptive Education): `g.tier3('compose')
    /// .neighbors(current-concept)` enumerates concepts willing to
    /// bridge from `current-concept`. Concepts whose prerequisites
    /// aren't met by the student veto via predicate.
    #[test]
    fn followon_f_adaptive_education_neighbors_with_predicate() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["fractions", "decimals", "calculus", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("fractions".to_string(), "decimals".to_string(), None, Attrs::new());
        graph.add_edge("fractions".to_string(), "calculus".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            register_followon_f_callbacks(
                &mut e, iso,
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            );
            iso
        };

        let fractions_body = "return { accepts: [], emits: ['compose'] };\n";
        // Decimals: prerequisites met (always accepts).
        let decimals_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return true; });\n\
            return { accepts: ['compose'], emits: [] };\n\
        ";
        // Calculus: prerequisites NOT met (predicate vetoes — student
        // hasn't demonstrated mastery of intermediate algebra).
        let calculus_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return false; });\n\
            return { accepts: ['compose'], emits: [] };\n\
        ";
        let querier_body = "\
            var nbrs = __makeTier3Proxy('compose').neighbors('fractions');\n\
            host_observe('bridgeable=' + nbrs.join(','));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_f = engine_rc.borrow_mut().eval_component(iso, "F", fractions_body).unwrap();
        let fn_d = engine_rc.borrow_mut().eval_component(iso, "D", decimals_body).unwrap();
        let fn_c = engine_rc.borrow_mut().eval_component(iso, "C", calculus_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Q", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("F".into(), Rc::clone(&engine_rc), iso, fn_f),
            "fractions".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("D".into(), Rc::clone(&engine_rc), iso, fn_d),
            "decimals".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("C".into(), Rc::clone(&engine_rc), iso, fn_c),
            "calculus".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Q".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _g = EngineGuard::with_directory(
                Rc::clone(&engine_rc), iso,
                Rc::clone(&scheduler.directory), snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        // Only decimals appears — calculus was vetoed.
        assert!(
            observed.iter().any(|s| s == "bridgeable=decimals"),
            "Tier 3 neighbors include decimals (predicate accepted) but exclude calculus (predicate vetoed); observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Application class #4 (Generative World Building):
    /// `g.tier3('war').bfs('invader-region')` returns regions reachable
    /// by war propagation. Peace-treaty regions veto via predicate.
    #[test]
    fn followon_f_generative_worlds_bfs_with_peace_treaty_vetoes() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["invader", "open-region", "peace-region", "frontier", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        // Tier 1: invader→open-region→frontier; invader→peace-region.
        // Without the treaty, war message would BFS to all 4 regions.
        // Peace-region's predicate vetoes incoming war from invader.
        graph.add_edge("invader".to_string(), "open-region".to_string(), None, Attrs::new());
        graph.add_edge("invader".to_string(), "peace-region".to_string(), None, Attrs::new());
        graph.add_edge("open-region".to_string(), "frontier".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            register_followon_f_callbacks(
                &mut e, iso,
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            );
            iso
        };

        let invader_body = "return { accepts: [], emits: ['war'] };\n";
        let open_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return true; });\n\
            return { accepts: ['war'], emits: ['war'] };\n\
        ";
        let peace_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return false; });\n\
            return { accepts: ['war'], emits: [] };\n\
        ";
        let frontier_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return true; });\n\
            return { accepts: ['war'], emits: [] };\n\
        ";
        let querier_body = "\
            var bfs = __makeTier3Proxy('war').bfs('invader');\n\
            host_observe('reachable=' + bfs.join(','));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_i = engine_rc.borrow_mut().eval_component(iso, "I", invader_body).unwrap();
        let fn_o = engine_rc.borrow_mut().eval_component(iso, "O", open_body).unwrap();
        let fn_p = engine_rc.borrow_mut().eval_component(iso, "P", peace_body).unwrap();
        let fn_fr = engine_rc.borrow_mut().eval_component(iso, "F", frontier_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Q", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("I".into(), Rc::clone(&engine_rc), iso, fn_i),
            "invader".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("O".into(), Rc::clone(&engine_rc), iso, fn_o),
            "open-region".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("P".into(), Rc::clone(&engine_rc), iso, fn_p),
            "peace-region".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("F".into(), Rc::clone(&engine_rc), iso, fn_fr),
            "frontier".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Q".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _g = EngineGuard::with_directory(
                Rc::clone(&engine_rc), iso,
                Rc::clone(&scheduler.directory), snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        // BFS visits: invader → open-region → frontier. Peace-region
        // is excluded (predicate vetoed). Expected order is
        // discovery-order — invader,open-region,frontier — but to
        // tolerate sort-order variation we just check membership.
        let bfs_line = observed.iter()
            .find(|s| s.starts_with("reachable="))
            .expect("reachable observation present");
        let visited: Vec<&str> = bfs_line.trim_start_matches("reachable=").split(',').collect();
        assert!(visited.contains(&"invader"), "{bfs_line:?}");
        assert!(visited.contains(&"open-region"), "{bfs_line:?}");
        assert!(visited.contains(&"frontier"), "{bfs_line:?}");
        assert!(!visited.contains(&"peace-region"),
            "peace-region must be vetoed by predicate; got {bfs_line:?}");
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Application class #7 (Biological Pathway Simulation):
    /// `g.tier3('binding').allPaths(drug, downstream-effector)`.
    /// Off-target receptors veto via predicate, leaving only the
    /// on-target signaling chain.
    #[test]
    #[allow(non_snake_case)]
    fn followon_f_biological_pathway_allPaths_with_off_target_vetoes() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["drug", "on-target-receptor", "off-target-receptor", "kinase", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        // Both receptors are Tier 1-adjacent to drug AND to kinase.
        // Tier 2 finds two paths: drug→on-target→kinase AND
        // drug→off-target→kinase. Tier 3 with off-target's
        // veto leaves only the on-target chain.
        graph.add_edge("drug".to_string(), "on-target-receptor".to_string(), None, Attrs::new());
        graph.add_edge("drug".to_string(), "off-target-receptor".to_string(), None, Attrs::new());
        graph.add_edge("on-target-receptor".to_string(), "kinase".to_string(), None, Attrs::new());
        graph.add_edge("off-target-receptor".to_string(), "kinase".to_string(), None, Attrs::new());

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            register_followon_f_callbacks(
                &mut e, iso,
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            );
            iso
        };

        let drug_body = "return { accepts: [], emits: ['binding'] };\n";
        // On-target receptor: predicate accepts (specific to this drug).
        let on_target_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return true; });\n\
            return { accepts: ['binding'], emits: ['binding'] };\n\
        ";
        // Off-target receptor: predicate rejects (specificity mismatch).
        let off_target_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return false; });\n\
            return { accepts: ['binding'], emits: ['binding'] };\n\
        ";
        let kinase_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return true; });\n\
            return { accepts: ['binding'], emits: [] };\n\
        ";
        let querier_body = "\
            var paths = __makeTier3Proxy('binding').allPaths('drug', 'kinase');\n\
            host_observe('paths-count=' + paths.length);\n\
            for (var i = 0; i < paths.length; i++) {\n\
                host_observe('path:' + paths[i].join('>'));\n\
            }\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_d = engine_rc.borrow_mut().eval_component(iso, "D", drug_body).unwrap();
        let fn_on = engine_rc.borrow_mut().eval_component(iso, "On", on_target_body).unwrap();
        let fn_off = engine_rc.borrow_mut().eval_component(iso, "Off", off_target_body).unwrap();
        let fn_k = engine_rc.borrow_mut().eval_component(iso, "K", kinase_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Q", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("D".into(), Rc::clone(&engine_rc), iso, fn_d),
            "drug".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("On".into(), Rc::clone(&engine_rc), iso, fn_on),
            "on-target-receptor".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Off".into(), Rc::clone(&engine_rc), iso, fn_off),
            "off-target-receptor".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("K".into(), Rc::clone(&engine_rc), iso, fn_k),
            "kinase".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Q".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _g = EngineGuard::with_directory(
                Rc::clone(&engine_rc), iso,
                Rc::clone(&scheduler.directory), snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "paths-count=1"),
            "exactly 1 Tier 3 path drug→kinase (off-target chain vetoed); observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "path:drug>on-target-receptor>kinase"),
            "the surviving path is via on-target-receptor; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Application class #8 (Reputation):
    /// `g.tier3('vouch').aStarPath(alice, bob, {heuristic:
    /// 'trust-attenuated'})`. Vouch-targets that decline veto via
    /// predicate; the surviving high-trust chain is selected.
    #[test]
    #[allow(non_snake_case)]
    fn followon_f_reputation_aStarPath_trust_attenuated_with_vouch_vetoes() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["alice", "high-trust-mutual", "low-trust-mutual", "bob", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        // Two parallel vouch chains alice→mutual→bob. Tier 1 backs
        // both. Trust attrs differ; A* with trust-attenuated should
        // pick high-trust. low-trust-mutual ALSO declines vouches via
        // predicate — exercises both heuristic and predicate paths.
        let mut high_edge = Attrs::new();
        high_edge.insert("trust".to_string(), serde_json::json!(1.0));
        let mut low_edge = Attrs::new();
        low_edge.insert("trust".to_string(), serde_json::json!(0.1));
        graph.add_edge("alice".to_string(), "high-trust-mutual".to_string(), None, high_edge.clone());
        graph.add_edge("alice".to_string(), "low-trust-mutual".to_string(), None, low_edge.clone());
        graph.add_edge("high-trust-mutual".to_string(), "bob".to_string(), None, high_edge);
        graph.add_edge("low-trust-mutual".to_string(), "bob".to_string(), None, low_edge);

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            register_followon_f_callbacks(
                &mut e, iso,
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            );
            iso
        };

        let alice_body = "return { accepts: [], emits: ['vouch'] };\n";
        let high_mutual_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return true; });\n\
            return { accepts: ['vouch'], emits: ['vouch'] };\n\
        ";
        // Low-trust mutual declines vouches — predicate vetoes, so
        // Tier 3 has no path through it regardless of trust attr.
        let low_mutual_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return false; });\n\
            return { accepts: ['vouch'], emits: ['vouch'] };\n\
        ";
        let bob_body = "\
            useEdgeAcceptHandler(function(_src, _t) { return true; });\n\
            return { accepts: ['vouch'], emits: [] };\n\
        ";
        let querier_body = "\
            var path = __makeTier3Proxy('vouch')\n\
                .aStarPath('alice', 'bob', { heuristic: 'trust-attenuated' });\n\
            host_observe('vouch-path=' + (path === null ? 'null' : path.join('>')));\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_a = engine_rc.borrow_mut().eval_component(iso, "A", alice_body).unwrap();
        let fn_h = engine_rc.borrow_mut().eval_component(iso, "H", high_mutual_body).unwrap();
        let fn_l = engine_rc.borrow_mut().eval_component(iso, "L", low_mutual_body).unwrap();
        let fn_b = engine_rc.borrow_mut().eval_component(iso, "B", bob_body).unwrap();
        let fn_q = engine_rc.borrow_mut().eval_component(iso, "Q", querier_body).unwrap();

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(
            build_js_node_definition("A".into(), Rc::clone(&engine_rc), iso, fn_a),
            "alice".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("H".into(), Rc::clone(&engine_rc), iso, fn_h),
            "high-trust-mutual".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("L".into(), Rc::clone(&engine_rc), iso, fn_l),
            "low-trust-mutual".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("B".into(), Rc::clone(&engine_rc), iso, fn_b),
            "bob".to_string(), Attrs::new(), 0_u32,
        ).unwrap();
        scheduler.register_node(
            build_js_node_definition("Q".into(), Rc::clone(&engine_rc), iso, fn_q),
            "querier".to_string(), Attrs::new(), 1_u32,
        ).unwrap();

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _g = EngineGuard::with_directory(
                Rc::clone(&engine_rc), iso,
                Rc::clone(&scheduler.directory), snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "vouch-path=alice>high-trust-mutual>bob"),
            "trust-attenuated A* over Tier 3 picks the high-trust chain (low-trust mutual vetoed); observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // ---------------------------------------------------------------------
    // M12.3b-phase1: useEffectAsync (JS Promise → Rust Future bridge) tests
    // ---------------------------------------------------------------------
    //
    // Each test drives `Scheduler::process_batch_async()` from a tokio
    // LocalSet (current_thread runtime) since JsRuntime is `!Send`.
    // Synchronous Promise resolution chains complete within JSC's
    // auto-drain at the call boundary, so the oneshot Sender is signaled
    // before the future awaits — no manual microtask drain needed.

    use tokio::task::LocalSet;

    /// Helper: register the host callback set required by useEffectAsync
    /// tests. Mirrors `register_followon_f_callbacks`'s shape.
    fn register_use_effect_async_callbacks(
        e: &mut JscEngine,
        iso: IsolateHandle,
        observe_callback: BoxCallback,
    ) {
        e.register_callback(iso, "host_state", build_host_state_callback())
            .expect("host_state");
        e.register_callback(iso, "host_state_set", build_host_state_set_callback())
            .expect("host_state_set");
        e.register_callback(iso, "host_effect", build_host_effect_callback())
            .expect("host_effect");
        e.register_callback(
            iso,
            "host_effect_async",
            build_host_effect_async_callback(),
        )
        .expect("host_effect_async");
        e.register_callback(
            iso,
            "host_effect_async_complete",
            build_host_effect_async_complete_callback(),
        )
        .expect("host_effect_async_complete");
        e.register_callback(iso, "host_track_token", build_host_track_token_callback())
            .expect("host_track_token");
        e.register_callback(iso, "host_observe", observe_callback)
            .expect("host_observe");
    }

    /// Phase1 #1: useEffectAsync with a Promise that resolves to
    /// `undefined` (no cleanup) fires the body during process_batch_async.
    #[test]
    fn phase1b_use_effect_async_void_resolution_fires_body() {
        let _g = engine_lock();
        reset_global_state();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = LocalSet::new();

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        local.block_on(&runtime, async move {
            let mut graph = CommunicationGraph::new();
            graph.add_node("n".to_string(), Attrs::new());

            let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
            let iso = {
                let mut e = engine_rc.borrow_mut();
                e.initialize().expect("init");
                let iso = e.create_isolate(IsolateOptions::default()).expect("create");
                register_use_effect_async_callbacks(
                    &mut e,
                    iso,
                    Box::new(move |args| {
                        if let Some(HostValue::String(s)) = args.first() {
                            observations_for_cb.lock().push(s.clone());
                        }
                        Ok(HostValue::Null)
                    }),
                );
                iso
            };

            // Body fires once on first render; uses useEffectAsync with
            // an empty deps array so it doesn't re-fire on subsequent
            // render-triggers.
            let body = "\
                useEffectAsync(async function() {\n\
                    host_observe('async-effect-fired');\n\
                    return undefined;\n\
                }, []);\n\
                return { accepts: [], emits: [] };\n\
            ";

            let fn_handle = engine_rc
                .borrow_mut()
                .eval_component(iso, "N", body)
                .expect("eval");

            let definition =
                build_js_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
            let mut scheduler = Scheduler::new(graph);
            scheduler
                .register_node(definition, "n".to_string(), Attrs::new(), 0_u32)
                .expect("register");

            // EngineGuard install for the async batch — same pattern as
            // JsRuntime::process_batch_async.
            let snapshot = Rc::new(GraphSnapshot::from_live(
                Rc::clone(&scheduler.graph),
                Rc::clone(&scheduler.manifests),
                Rc::clone(&scheduler.edge_accept_predicates),
            ));
            {
                let _eg = EngineGuard::with_directory(
                    Rc::clone(&engine_rc),
                    iso,
                    Rc::clone(&scheduler.directory),
                    snapshot,
                );
                scheduler
                    .process_batch_async()
                    .await
                    .expect("process_batch_async");
            }

            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "async-effect-fired"),
                "async effect must fire during process_batch_async; observations: {observed:?}"
            );
            drop(observed);

            let mut e = engine_rc.borrow_mut();
            e.dispose_isolate(iso).expect("dispose");
            e.shutdown().expect("shutdown");
        });
    }

    /// Phase1 #2: useEffectAsync that resolves to a cleanup function.
    /// The cleanup runs on next deps-change re-render (here triggered by
    /// `setCount(1)` from the body's first render via useEffect-sync).
    #[test]
    fn phase1b_use_effect_async_cleanup_fires_on_deps_change() {
        let _g = engine_lock();
        reset_global_state();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = LocalSet::new();

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        local.block_on(&runtime, async move {
            let mut graph = CommunicationGraph::new();
            graph.add_node("n".to_string(), Attrs::new());

            let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
            let iso = {
                let mut e = engine_rc.borrow_mut();
                e.initialize().expect("init");
                let iso = e.create_isolate(IsolateOptions::default()).expect("create");
                register_use_effect_async_callbacks(
                    &mut e,
                    iso,
                    Box::new(move |args| {
                        if let Some(HostValue::String(s)) = args.first() {
                            observations_for_cb.lock().push(s.clone());
                        }
                        Ok(HostValue::Null)
                    }),
                );
                iso
            };

            // Body uses useEffectAsync with deps=[count]. First render
            // (count=0): async effect installs cleanup. setCount(1)
            // dirties the node; second render: deps differ, async
            // effect re-fires — but BEFORE re-firing, the previous
            // cleanup runs.
            let body = "\
                var [count, setCount] = useState(0);\n\
                host_observe('render:count=' + count);\n\
                useEffectAsync(async function() {\n\
                    host_observe('async-mount:count=' + count);\n\
                    return function() { host_observe('async-cleanup:count=' + count); };\n\
                }, [count]);\n\
                if (count === 0) setCount(1);\n\
                return { accepts: [], emits: [] };\n\
            ";

            let fn_handle = engine_rc
                .borrow_mut()
                .eval_component(iso, "N", body)
                .expect("eval");

            let definition =
                build_js_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
            let mut scheduler = Scheduler::new(graph);
            scheduler
                .register_node(definition, "n".to_string(), Attrs::new(), 0_u32)
                .expect("register");

            let snapshot = Rc::new(GraphSnapshot::from_live(
                Rc::clone(&scheduler.graph),
                Rc::clone(&scheduler.manifests),
                Rc::clone(&scheduler.edge_accept_predicates),
            ));
            {
                let _eg = EngineGuard::with_directory(
                    Rc::clone(&engine_rc),
                    iso,
                    Rc::clone(&scheduler.directory),
                    snapshot,
                );
                scheduler
                    .process_batch_async()
                    .await
                    .expect("process_batch_async");
            }

            let observed = observations.lock();
            // Both renders observed.
            assert!(
                observed.iter().any(|s| s == "render:count=0"),
                "first render observed; observations: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "render:count=1"),
                "second render observed (deps differ); observations: {observed:?}"
            );
            // Mount fired with count=0.
            assert!(
                observed.iter().any(|s| s == "async-mount:count=0"),
                "async mount fired with count=0; observations: {observed:?}"
            );
            // Cleanup fired (the prior async cleanup's closure captured
            // count=0; it runs before the new mount even though the
            // current render is at count=1).
            assert!(
                observed.iter().any(|s| s == "async-cleanup:count=0"),
                "async cleanup from prior deps fired; observations: {observed:?}"
            );
            // Mount fired again with count=1.
            assert!(
                observed.iter().any(|s| s == "async-mount:count=1"),
                "async mount re-fired with count=1; observations: {observed:?}"
            );
            drop(observed);

            let mut e = engine_rc.borrow_mut();
            e.dispose_isolate(iso).expect("dispose");
            e.shutdown().expect("shutdown");
        });
    }

    /// Phase1 #3: an async function that throws is caught — Rust logs
    /// the throw and the cleanup is None. Subsequent renders still work.
    #[test]
    fn phase1b_use_effect_async_rejection_handled_gracefully() {
        let _g = engine_lock();
        reset_global_state();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = LocalSet::new();

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        local.block_on(&runtime, async move {
            let mut graph = CommunicationGraph::new();
            graph.add_node("n".to_string(), Attrs::new());

            let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
            let iso = {
                let mut e = engine_rc.borrow_mut();
                e.initialize().expect("init");
                let iso = e.create_isolate(IsolateOptions::default()).expect("create");
                register_use_effect_async_callbacks(
                    &mut e,
                    iso,
                    Box::new(move |args| {
                        if let Some(HostValue::String(s)) = args.first() {
                            observations_for_cb.lock().push(s.clone());
                        }
                        Ok(HostValue::Null)
                    }),
                );
                iso
            };

            // Async fn throws. Body still completes its render; the
            // rejection is logged by Rust (eprintln) but doesn't
            // propagate as a panic.
            let body = "\
                host_observe('rendering');\n\
                useEffectAsync(async function() {\n\
                    throw new Error('async-boom');\n\
                }, []);\n\
                host_observe('returning-manifest');\n\
                return { accepts: [], emits: [] };\n\
            ";

            let fn_handle = engine_rc
                .borrow_mut()
                .eval_component(iso, "N", body)
                .expect("eval");

            let definition =
                build_js_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
            let mut scheduler = Scheduler::new(graph);
            scheduler
                .register_node(definition, "n".to_string(), Attrs::new(), 0_u32)
                .expect("register");

            let snapshot = Rc::new(GraphSnapshot::from_live(
                Rc::clone(&scheduler.graph),
                Rc::clone(&scheduler.manifests),
                Rc::clone(&scheduler.edge_accept_predicates),
            ));
            {
                let _eg = EngineGuard::with_directory(
                    Rc::clone(&engine_rc),
                    iso,
                    Rc::clone(&scheduler.directory),
                    snapshot,
                );
                // No panic, no error from process_batch_async.
                scheduler
                    .process_batch_async()
                    .await
                    .expect("process_batch_async");
            }

            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "rendering"),
                "render entered before async effect; observations: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "returning-manifest"),
                "render completed after queuing async effect; observations: {observed:?}"
            );
            drop(observed);

            let mut e = engine_rc.borrow_mut();
            e.dispose_isolate(iso).expect("dispose");
            e.shutdown().expect("shutdown");
        });
    }

    /// Phase1 #4: useEffectAsync coexists with sync useEffect — both
    /// fire during process_batch_async. Sync useEffect runs first (per
    /// hook order); async fires second.
    #[test]
    fn phase1b_use_effect_async_coexists_with_sync_use_effect() {
        let _g = engine_lock();
        reset_global_state();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = LocalSet::new();

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        local.block_on(&runtime, async move {
            let mut graph = CommunicationGraph::new();
            graph.add_node("n".to_string(), Attrs::new());

            let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
            let iso = {
                let mut e = engine_rc.borrow_mut();
                e.initialize().expect("init");
                let iso = e.create_isolate(IsolateOptions::default()).expect("create");
                register_use_effect_async_callbacks(
                    &mut e,
                    iso,
                    Box::new(move |args| {
                        if let Some(HostValue::String(s)) = args.first() {
                            observations_for_cb.lock().push(s.clone());
                        }
                        Ok(HostValue::Null)
                    }),
                );
                iso
            };

            let body = "\
                useEffect(function() {\n\
                    host_observe('sync-effect');\n\
                    return undefined;\n\
                }, []);\n\
                useEffectAsync(async function() {\n\
                    host_observe('async-effect');\n\
                    return undefined;\n\
                }, []);\n\
                return { accepts: [], emits: [] };\n\
            ";

            let fn_handle = engine_rc
                .borrow_mut()
                .eval_component(iso, "N", body)
                .expect("eval");

            let definition =
                build_js_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
            let mut scheduler = Scheduler::new(graph);
            scheduler
                .register_node(definition, "n".to_string(), Attrs::new(), 0_u32)
                .expect("register");

            let snapshot = Rc::new(GraphSnapshot::from_live(
                Rc::clone(&scheduler.graph),
                Rc::clone(&scheduler.manifests),
                Rc::clone(&scheduler.edge_accept_predicates),
            ));
            {
                let _eg = EngineGuard::with_directory(
                    Rc::clone(&engine_rc),
                    iso,
                    Rc::clone(&scheduler.directory),
                    snapshot,
                );
                scheduler
                    .process_batch_async()
                    .await
                    .expect("process_batch_async");
            }

            let observed = observations.lock();
            // Both effects fire. Order isn't strictly contractual at
            // the assertion level — just check both ran.
            assert!(
                observed.iter().any(|s| s == "sync-effect"),
                "sync effect fired; observations: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "async-effect"),
                "async effect fired; observations: {observed:?}"
            );
            drop(observed);

            let mut e = engine_rc.borrow_mut();
            e.dispose_isolate(iso).expect("dispose");
            e.shutdown().expect("shutdown");
        });
    }

    /// Phase1 #5: process_batch (sync) does NOT fire async effects.
    /// This pins the contract that async is opt-in via the async path.
    #[test]
    fn phase1b_use_effect_async_skipped_on_sync_path() {
        let _g = engine_lock();
        reset_global_state();

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        let mut graph = CommunicationGraph::new();
        graph.add_node("n".to_string(), Attrs::new());

        let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            register_use_effect_async_callbacks(
                &mut e,
                iso,
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            );
            iso
        };

        let body = "\
            useEffect(function() {\n\
                host_observe('sync-fired');\n\
            }, []);\n\
            useEffectAsync(async function() {\n\
                host_observe('async-fired');\n\
            }, []);\n\
            return { accepts: [], emits: [] };\n\
        ";

        let fn_handle = engine_rc
            .borrow_mut()
            .eval_component(iso, "N", body)
            .expect("eval");

        let definition =
            build_js_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "n".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _eg = EngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("process_batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "sync-fired"),
            "sync effect fires on sync path; observations: {observed:?}"
        );
        assert!(
            !observed.iter().any(|s| s == "async-fired"),
            "async effect must NOT fire on the sync path; observations: {observed:?}"
        );
        drop(observed);

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // ---------------------------------------------------------------------
    // M12.3b-phase1.5: drain_microtasks via macOS extern-C shim — tests
    // ---------------------------------------------------------------------
    //
    // Phase1.5 wires the real `JSC::VM::drainMicrotasks()` via the
    // C++ shim at `crates/graphreact-engine-jsc/src/drain_shim.cpp`.
    // Tests below verify (1) the shim is callable end-to-end without
    // panicking, (2) multi-microtask `Promise.then()` chains continue
    // to settle (regression check vs phase1's auto-drain reliance),
    // and (3) genuine async I/O — JS Promises resolved by tokio-spawned
    // host callbacks — settles via the combined auto-drain +
    // `select!`-loop manual drain in `host_effect_async`.

    /// Phase1.5 #1: `engine.drain_microtasks(iso)` is callable on a
    /// fresh isolate without panicking. Confirms the linker-resolved
    /// `_ZN3JSC2VM15drainMicrotasksEv` symbol actually fires at
    /// runtime (linkage success ≠ runtime success — Apple could have
    /// stripped or re-laid the symbol between SDK and runtime).
    #[test]
    fn phase1_5_drain_microtasks_callable_without_panic() {
        let _g = engine_lock();
        let mut e = JscEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        // Direct call: drain on an empty queue is a no-op but must
        // not panic / SIGABRT / corrupt VM state. Run several times
        // to surface any one-shot bug.
        for _ in 0..10 {
            e.drain_microtasks(iso).expect("drain");
        }

        // Static variant: same surface, used by JsRuntime in the
        // async batch path.
        for _ in 0..10 {
            graphreact_engine_jsc::drain_microtasks_static(iso).expect("drain_static");
        }

        // Sanity: VM is still usable after manual drains (no
        // partial-flush corruption).
        let m = e
            .eval_module(iso, "post-drain", "return { ok: 1 + 1 };")
            .expect("eval");
        assert!(m.0 > 0);

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase1.5 #2: regression check — a multi-step `await` chain
    /// inside `useEffectAsync` (3 chained Promise.resolve resolutions)
    /// settles within a single `process_batch_async`. Phase1's auto-
    /// drain was sufficient for this; phase1.5's manual drain must
    /// not interfere.
    #[test]
    fn phase1_5_use_effect_async_multi_microtask_chain() {
        let _g = engine_lock();
        reset_global_state();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = LocalSet::new();

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        local.block_on(&runtime, async move {
            let mut graph = CommunicationGraph::new();
            graph.add_node("n".to_string(), Attrs::new());

            let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
            let iso = {
                let mut e = engine_rc.borrow_mut();
                e.initialize().expect("init");
                let iso = e.create_isolate(IsolateOptions::default()).expect("create");
                register_use_effect_async_callbacks(
                    &mut e,
                    iso,
                    Box::new(move |args| {
                        if let Some(HostValue::String(s)) = args.first() {
                            observations_for_cb.lock().push(s.clone());
                        }
                        Ok(HostValue::Null)
                    }),
                );
                iso
            };

            // Body chains three `await Promise.resolve(...)` resolutions.
            // Each `await` queues a continuation as a microtask. Phase1's
            // auto-drain at the body's call boundary flushes them all;
            // phase1.5's `select!` drain is belt-and-suspenders.
            let body = "\
                useEffectAsync(async function() {\n\
                    var v = await Promise.resolve(1);\n\
                    v = await Promise.resolve(v + 1);\n\
                    v = await Promise.resolve(v + 1);\n\
                    host_observe('chain-settled:' + v);\n\
                    return undefined;\n\
                }, []);\n\
                return { accepts: [], emits: [] };\n\
            ";

            let fn_handle = engine_rc
                .borrow_mut()
                .eval_component(iso, "N", body)
                .expect("eval");

            let definition =
                build_js_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
            let mut scheduler = Scheduler::new(graph);
            scheduler
                .register_node(definition, "n".to_string(), Attrs::new(), 0_u32)
                .expect("register");

            let snapshot = Rc::new(GraphSnapshot::from_live(
                Rc::clone(&scheduler.graph),
                Rc::clone(&scheduler.manifests),
                Rc::clone(&scheduler.edge_accept_predicates),
            ));
            {
                let _eg = EngineGuard::with_directory(
                    Rc::clone(&engine_rc),
                    iso,
                    Rc::clone(&scheduler.directory),
                    snapshot,
                );
                scheduler
                    .process_batch_async()
                    .await
                    .expect("process_batch_async");
            }

            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "chain-settled:3"),
                "3-step await chain settled to 3; observations: {observed:?}"
            );
            drop(observed);

            let mut e = engine_rc.borrow_mut();
            e.dispose_isolate(iso).expect("dispose");
            e.shutdown().expect("shutdown");
        });
    }

    /// Phase1.5 #3: genuine async I/O — `useEffectAsync` awaits a JS
    /// `Promise` whose resolver is captured by a host callback that
    /// `tokio::task::spawn_local`s a deferred-yield task. The
    /// resolution happens AFTER the body's first `__invokeCallback`
    /// returns, so the auto-drain at that boundary cannot flush the
    /// `await` continuation — phase1.5's `select!`-loop drain in
    /// `host_effect_async` (combined with the auto-drain at the
    /// resolver's own call boundary) is what gets the chain to settle.
    ///
    /// Test design: a `host_async_resolve` callback registered for
    /// this test takes a resolver token, `tokio::task::spawn_local`s
    /// a yield-then-invoke chain, returns immediately. The JS body
    /// awaits a Promise wired to that resolver and returns the
    /// resolved value through `host_observe`.
    #[test]
    fn phase1_5_use_effect_async_genuine_async_via_deferred_host_resolve() {
        let _g = engine_lock();
        reset_global_state();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = LocalSet::new();

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);

        local.block_on(&runtime, async move {
            let mut graph = CommunicationGraph::new();
            graph.add_node("n".to_string(), Attrs::new());

            let engine_rc: Rc<RefCell<JscEngine>> = Rc::new(RefCell::new(JscEngine::new()));
            let iso = {
                let mut e = engine_rc.borrow_mut();
                e.initialize().expect("init");
                let iso = e.create_isolate(IsolateOptions::default()).expect("create");
                register_use_effect_async_callbacks(
                    &mut e,
                    iso,
                    Box::new(move |args| {
                        if let Some(HostValue::String(s)) = args.first() {
                            observations_for_cb.lock().push(s.clone());
                        }
                        Ok(HostValue::Null)
                    }),
                );

                // Register a host callback that defers resolution via
                // tokio::task::spawn_local — the real "genuine async"
                // path. The closure captures `iso` so its spawned task
                // can re-enter JSC via invoke_global_static.
                e.register_callback(
                    iso,
                    "host_async_resolve",
                    Box::new(move |args| {
                        let token = match args.first() {
                            Some(HostValue::Number(n)) => *n as u64,
                            _ => return Ok(HostValue::Null),
                        };
                        let value = args.get(1).cloned().unwrap_or(HostValue::Null);
                        // Defer the resolver invocation across two
                        // tokio yields — guarantees the JS-side `await`
                        // suspends and JSC's auto-drain at the original
                        // body call boundary cannot see the resolution.
                        tokio::task::spawn_local(async move {
                            tokio::task::yield_now().await;
                            tokio::task::yield_now().await;
                            let _ = invoke_global_static(
                                iso,
                                "__invokeCallback",
                                &[
                                    HostValue::Number(token as f64),
                                    HostValue::Array(vec![value]),
                                ],
                            );
                        });
                        Ok(HostValue::Null)
                    }),
                )
                .expect("host_async_resolve");

                iso
            };

            // Body awaits a Promise resolved by the deferred host
            // callback. Without phase1.5's drain integration, the
            // `host_effect_async` future would await its oneshot
            // forever (the resolver fires from a tokio task; the
            // continuation lands as a microtask that needs a drain
            // to run; the drain only happens via auto-drain at
            // re-entry — `select!`'s drain provides the tick).
            let body = "\
                useEffectAsync(async function() {\n\
                    var resolved = await new Promise(function(resolve) {\n\
                        host_async_resolve(__registerCallback(resolve), 'deferred-value');\n\
                    });\n\
                    host_observe('async-settled:' + resolved);\n\
                    return undefined;\n\
                }, []);\n\
                return { accepts: [], emits: [] };\n\
            ";

            let fn_handle = engine_rc
                .borrow_mut()
                .eval_component(iso, "N", body)
                .expect("eval");

            let definition =
                build_js_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
            let mut scheduler = Scheduler::new(graph);
            scheduler
                .register_node(definition, "n".to_string(), Attrs::new(), 0_u32)
                .expect("register");

            let snapshot = Rc::new(GraphSnapshot::from_live(
                Rc::clone(&scheduler.graph),
                Rc::clone(&scheduler.manifests),
                Rc::clone(&scheduler.edge_accept_predicates),
            ));
            {
                let _eg = EngineGuard::with_directory(
                    Rc::clone(&engine_rc),
                    iso,
                    Rc::clone(&scheduler.directory),
                    snapshot,
                );
                scheduler
                    .process_batch_async()
                    .await
                    .expect("process_batch_async");
            }

            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "async-settled:deferred-value"),
                "deferred host resolve settled the await; observations: {observed:?}"
            );
            drop(observed);

            let mut e = engine_rc.borrow_mut();
            e.dispose_isolate(iso).expect("dispose");
            e.shutdown().expect("shutdown");
        });
    }
}
