//! Hooks API — `use_state`, `use_effect`, `use_memo`, `use_message`, etc.
//!
//! Hooks are free functions called from inside a node component body. They
//! read thread-locals populated by [`crate::runtime::lifecycle::evaluate_node`]
//! and mutate the active [`NodeInstance`] through `Rc<RefCell<...>>` — no
//! `unsafe` required.
//!
//! ## Re-entrancy contract
//!
//! Hooks borrow the node's `RefCell` mutably for the duration of their own
//! call only. They MUST NOT call user code while holding the borrow —
//! every helper drops the `RefMut` before invoking any closure.

use generational_box::{GenerationalBox, UnsyncStorage};
use serde_json::Value;
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use crate::runtime::node_instance::{
    AsyncEffectCleanup, DurableStateCellInner, EffectCallback, EffectCleanup, EffectHookState,
    HookSlot, MemoHookState, MessageHookState, NodeInstance, SourceHookState, StateHookState,
    TopologyHookState,
};
use crate::runtime::sources::EventSource;
use crate::runtime::types::{
    DurableEffectRequest, DurableTimerCommand, EmitOptions, IncomingMessage, MessageHandler,
    TopologyFilterOptions, TopologyHandler,
};

// --- Thread-local context populated by the lifecycle ---

thread_local! {
    static CURRENT_NODE: RefCell<Option<Rc<RefCell<NodeInstance>>>> = const { RefCell::new(None) };
    static HOOK_INDEX: Cell<usize> = const { Cell::new(0) };
    static HOOK_CTX: RefCell<Option<HookCtx>> = const { RefCell::new(None) };
}

/// Side data the lifecycle injects for hooks to read.
pub struct HookCtx {
    #[allow(dead_code)]
    pub(crate) node_id: String,
    pub(crate) neighbors: Vec<String>,
    /// Re-enqueue the active node for re-evaluation.
    pub(crate) enqueue_self: std::rc::Rc<dyn Fn()>,
    /// Send a message from the active node.
    pub(crate) emit: std::rc::Rc<dyn Fn(&str, Value, EmitOptions)>,
    pub(crate) enqueue_durable_effect:
        std::rc::Rc<dyn Fn(DurableEffectRequest) -> Result<String, crate::RuntimeError>>,
    pub(crate) durable_timer:
        std::rc::Rc<dyn Fn(DurableTimerCommand) -> Result<Option<String>, crate::RuntimeError>>,
    /// Register a new event source under the active node.
    pub(crate) register_source: std::rc::Rc<dyn Fn(std::sync::Arc<dyn EventSource>)>,
    /// Unregister an event source by ID.
    pub(crate) unregister_source: std::rc::Rc<dyn Fn(&str)>,
    /// Register a monitor on a target. Takes a string target id;
    /// detects `name@<other>`-shaped cross-node targets and routes
    /// them through the dist crate's [`crate::runtime::scheduler::RemoteMonitorSink`]
    /// when one is installed. Returns `Some(MonitorRef)` for
    /// successful local resolution OR for routable remote
    /// targets, `None` otherwise. M14.5.
    pub(crate) register_monitor:
        std::rc::Rc<dyn Fn(&str) -> Option<crate::runtime::supervision::MonitorRef>>,
    /// Cancel a monitor by ref.
    pub(crate) cancel_monitor: std::rc::Rc<dyn Fn(crate::runtime::supervision::MonitorRef)>,
    /// Establish a bidirectional link between the active node and the
    /// target. Idempotent. Takes a string target id; routes
    /// `name@<other>`-shaped cross-node targets through the
    /// [`crate::runtime::scheduler::RemoteLinkSink`]. Returns
    /// `true` if the link is established (locally or via the dist
    /// sink); `false` if the local target is unknown or the
    /// remote sink is not installed. M14.5.
    pub(crate) register_link: std::rc::Rc<dyn Fn(&str) -> bool>,
    /// Remove the bidirectional link. String-shaped twin of
    /// [`Self::register_link`]; remote targets route through the
    /// [`crate::runtime::scheduler::RemoteUnlinkSink`].
    pub(crate) unregister_link: std::rc::Rc<dyn Fn(&str)>,
    /// Join a process group.
    pub(crate) group_join: std::rc::Rc<dyn Fn(&str)>,
    /// Leave a process group.
    pub(crate) group_leave: std::rc::Rc<dyn Fn(&str)>,
    /// Spawn a child node — deferred to the next `process_batch` round.
    pub(crate) spawn_child: std::rc::Rc<dyn Fn(crate::runtime::types::ComposeChildSpec)>,
    /// Unmount the active node — deferred to the next `process_batch` round.
    pub(crate) unmount_self: std::rc::Rc<dyn Fn()>,
    /// Declarative composition — set the parent's `composed_children`
    /// and spawn any missing children. Deferred.
    pub(crate) compose: std::rc::Rc<dyn Fn(Vec<crate::runtime::types::ComposeChildSpec>)>,
    /// Phase-9 edge negotiation — accept an incoming edge request from
    /// `target_id`, materialising a real bidirectional edge between the
    /// active node and the target. The TS reference implementation calls
    /// `graph.addEdge(active_id, target_id)` synchronously; mirrored here.
    pub(crate) accept_edge: std::rc::Rc<dyn Fn(&str)>,
    /// Install the active node's edge-accept predicate. Called by
    /// [`use_edge_accept_handler`]; the closure writes the predicate onto
    /// the active `NodeInstance.edge_accept_predicate` slot. M12.3a-followon-F.
    pub(crate) set_edge_accept_handler:
        std::rc::Rc<dyn Fn(crate::runtime::node_instance::EdgeAcceptPredicate)>,
}

/// Set the currently-evaluating node. Called by the lifecycle exclusively.
pub(crate) fn set_current_node(node: Option<Rc<RefCell<NodeInstance>>>) {
    CURRENT_NODE.with(|cell| *cell.borrow_mut() = node);
    HOOK_INDEX.with(|idx| idx.set(0));
}

/// Set the hook context. Called by the lifecycle around the component call.
pub(crate) fn set_hook_ctx(ctx: Option<HookCtx>) {
    HOOK_CTX.with(|cell| *cell.borrow_mut() = ctx);
}

fn current_node_rc() -> Rc<RefCell<NodeInstance>> {
    CURRENT_NODE
        .with(|cell| cell.borrow().clone())
        .expect("hook called outside a node component evaluation")
}

fn next_hook_index() -> usize {
    HOOK_INDEX.with(|idx| {
        let i = idx.get();
        idx.set(i + 1);
        i
    })
}

fn with_ctx<R>(f: impl FnOnce(&HookCtx) -> R) -> R {
    HOOK_CTX.with(|cell| {
        let borrow = cell.borrow();
        let ctx = borrow
            .as_ref()
            .expect("hook called outside a node component evaluation");
        f(ctx)
    })
}

// --- Signal<T> — Copy handle to a state cell ---

/// Reactive cell handle returned by [`use_state`] / [`use_reducer`].
///
/// `Copy` so it can be captured into closures, async tasks, and message
/// handlers without `.clone()`. Backed by a [`generational_box`] arena
/// owned by the node — when the node unmounts, all in-flight `Signal`s
/// become permanently invalidated.
pub struct Signal<T: 'static> {
    cell: GenerationalBox<StateCellInner, UnsyncStorage>,
    _marker: std::marker::PhantomData<fn(T) -> T>,
}

/// Inner state cell — type-erased value plus the enqueue closure that
/// triggers re-evaluation of the owning node when the value changes.
///
/// `!Send` overall because of the `Rc`-typed enqueue. The runtime is
/// single-threaded (M4 baseline); cross-thread signals land in M5 with
/// `SyncStorage` variants.
pub struct StateCellInner {
    pub(crate) value: Box<dyn Any>,
    pub(crate) enqueue: std::rc::Rc<dyn Fn()>,
}

impl<T: 'static> Signal<T> {
    /// Read the current value (cloning).
    pub fn get(&self) -> T
    where
        T: Clone,
    {
        let cell = self.cell.read();
        cell.value
            .downcast_ref::<T>()
            .expect("Signal type mismatch — different T used after construction")
            .clone()
    }

    /// Replace the value. Triggers re-evaluation if the new value differs
    /// from the current one.
    pub fn set(&self, value: T)
    where
        T: PartialEq + Send + Sync + 'static,
    {
        let mut cell = self.cell.write();
        let changed = !cell
            .value
            .downcast_ref::<T>()
            .is_some_and(|current| current == &value);
        cell.value = Box::new(value);
        if changed {
            let enqueue = std::rc::Rc::clone(&cell.enqueue);
            drop(cell);
            enqueue();
        }
    }

    /// Apply a transformation to the current value.
    pub fn update<F>(&self, f: F)
    where
        F: FnOnce(&T) -> T,
        T: PartialEq + Clone + Send + Sync + 'static,
    {
        let new_value = {
            let cell = self.cell.read();
            let current: &T = cell
                .value
                .downcast_ref::<T>()
                .expect("Signal type mismatch");
            f(current)
        };
        self.set(new_value);
    }
}

impl<T: 'static> Clone for Signal<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: 'static> Copy for Signal<T> {}

/// Handle to explicitly durable, named JSON state.
///
/// Unlike [`Signal<T>`], this state is keyed independently of hook order and
/// can be included in a scheduler recovery snapshot. Keeping the value as
/// `serde_json::Value` makes the serialization contract identical across the
/// JavaScript, Python, and Go hosts.
#[derive(Clone, Copy)]
pub struct DurableSignal {
    cell: GenerationalBox<DurableStateCellInner, UnsyncStorage>,
}

impl DurableSignal {
    /// Read the current durable JSON value.
    #[must_use]
    pub fn get(&self) -> Value {
        self.cell.read().value.clone()
    }

    /// Replace the durable JSON value and schedule re-evaluation on change.
    pub fn set(&self, value: Value) {
        let mut cell = self.cell.write();
        if cell.value == value {
            return;
        }
        cell.value = value;
        let enqueue = std::rc::Rc::clone(&cell.enqueue);
        drop(cell);
        enqueue();
    }

    /// Transform the durable value in place.
    pub fn update(&self, f: impl FnOnce(&Value) -> Value) {
        self.set(f(&self.get()));
    }
}

// --- Hooks ---

/// Node-local state. Returns a [`Signal<T>`] handle.
pub fn use_state<T>(initial: T) -> Signal<T>
where
    T: PartialEq + Clone + Send + Sync + 'static,
{
    let index = next_hook_index();
    let node_rc = current_node_rc();
    let enqueue = with_ctx(|c| std::rc::Rc::clone(&c.enqueue_self));

    let cell = {
        let mut node = node_rc.borrow_mut();
        ensure_slot_count(&mut node, index + 1);
        match &node.hooks[index] {
            HookSlot::State(state) => state.value,
            _ => {
                let cell = node.owner.insert(StateCellInner {
                    value: Box::new(initial.clone()),
                    enqueue: std::rc::Rc::clone(&enqueue),
                });
                node.hooks[index] = HookSlot::State(StateHookState { value: cell });
                cell
            }
        }
    };

    Signal::<T> {
        cell,
        _marker: std::marker::PhantomData,
    }
}

/// Return a callback that buffers an effect intent in the active durable
/// message-handler activation. Dispatch happens only after host commit.
pub fn use_durable_effect(
) -> std::rc::Rc<dyn Fn(DurableEffectRequest) -> Result<String, crate::RuntimeError>> {
    with_ctx(|ctx| std::rc::Rc::clone(&ctx.enqueue_durable_effect))
}

/// Return the command surface for scheduling and cancelling durable timers.
/// Scheduling returns the stable timer id; cancellation returns `None`.
pub fn use_durable_timer(
) -> std::rc::Rc<dyn Fn(DurableTimerCommand) -> Result<Option<String>, crate::RuntimeError>> {
    with_ctx(|ctx| std::rc::Rc::clone(&ctx.durable_timer))
}

/// Node-local JSON state with a stable recovery key.
///
/// Keys must be non-empty and may appear only once in a component
/// evaluation. Violations panic before the scheduler can commit the
/// activation, matching the existing hook misuse failure model.
pub fn use_durable_state(key: impl Into<String>, initial: Value) -> DurableSignal {
    let key = key.into();
    assert!(
        !key.trim().is_empty(),
        "durable state key must be non-empty"
    );

    let node_rc = current_node_rc();
    let enqueue = with_ctx(|ctx| std::rc::Rc::clone(&ctx.enqueue_self));
    let cell = {
        let mut node = node_rc.borrow_mut();
        assert!(
            node.durable_keys_seen.insert(key.clone()),
            "duplicate durable state key `{key}` in node `{}`",
            node.id
        );
        if let Some(cell) = node.durable_state.get(&key) {
            *cell
        } else {
            let value = node.restored_durable_state.remove(&key).unwrap_or(initial);
            let cell = node.owner.insert(DurableStateCellInner { value, enqueue });
            node.durable_state.insert(key, cell);
            cell
        }
    };
    DurableSignal { cell }
}

/// Reducer-style state. The reducer maps `(state, action)` to a new state.
///
/// Returns a [`Signal<T>`] for reads and a dispatch function for writes.
pub fn use_reducer<T, A, R>(reducer: R, initial: T) -> (Signal<T>, std::rc::Rc<dyn Fn(A)>)
where
    T: PartialEq + Clone + Send + Sync + 'static,
    A: 'static,
    R: Fn(&T, A) -> T + 'static,
{
    let signal: Signal<T> = use_state(initial);
    let reducer = std::rc::Rc::new(reducer);
    let dispatch: std::rc::Rc<dyn Fn(A)> = std::rc::Rc::new(move |action: A| {
        let current = signal.get();
        let next = reducer(&current, action);
        signal.set(next);
    });
    (signal, dispatch)
}

/// Synchronous side effect with dependency tracking. Runs during the sync
/// flush path; ignored on the async flush path's sync slot — both paths
/// process sync effects identically.
pub fn use_effect<F>(callback: F, deps: Vec<Value>)
where
    F: Fn() -> Option<EffectCleanup> + 'static,
{
    install_effect_slot(EffectCallback::Sync(std::rc::Rc::new(callback)), deps);
}

/// Async side effect with dependency tracking. The callback returns a future
/// that the async flush path awaits. **Only runs on the async flush path** —
/// invoking `use_effect_async` from a node that's evaluated synchronously
/// will leave the effect pending (debug-asserted in the lifecycle).
///
/// Mirrors the TS Phase 6 design: async is opt-in, not a sync replacement.
pub fn use_effect_async<F, Fut>(callback: F, deps: Vec<Value>)
where
    F: Fn() -> Fut + 'static,
    Fut: std::future::Future<Output = Option<AsyncEffectCleanup>> + 'static,
{
    let wrapped = std::rc::Rc::new(move || {
        let fut = callback();
        Box::pin(fut)
            as std::pin::Pin<Box<dyn std::future::Future<Output = Option<AsyncEffectCleanup>>>>
    });
    install_effect_slot(EffectCallback::Async(wrapped), deps);
}

fn install_effect_slot(callback: EffectCallback, deps: Vec<Value>) {
    let index = next_hook_index();
    let node_rc = current_node_rc();
    let mut node = node_rc.borrow_mut();
    ensure_slot_count(&mut node, index + 1);

    let should_run = match &node.hooks[index] {
        HookSlot::Effect(state) => deps_changed(state.committed_deps.as_ref(), Some(&deps)),
        _ => true,
    };

    if !should_run {
        return;
    }

    let prev_cleanup = match std::mem::replace(&mut node.hooks[index], HookSlot::Empty) {
        HookSlot::Effect(prev) => prev.cleanup,
        _ => None,
    };
    node.hooks[index] = HookSlot::Effect(EffectHookState {
        callback: Some(callback),
        committed_deps: None,
        pending_deps: Some(deps),
        cleanup: prev_cleanup,
    });
    node.pending_effects.push(index);
}

/// Memoized computation.
pub fn use_memo<T, F>(factory: F, deps: Vec<Value>) -> T
where
    T: Clone + 'static,
    F: FnOnce() -> T,
{
    let index = next_hook_index();
    let node_rc = current_node_rc();

    let recompute = {
        let mut node = node_rc.borrow_mut();
        ensure_slot_count(&mut node, index + 1);
        match &node.hooks[index] {
            HookSlot::Memo(state) => deps_changed(state.deps.as_ref(), Some(&deps)),
            _ => true,
        }
    };

    if recompute {
        let value = factory();
        let mut node = node_rc.borrow_mut();
        node.hooks[index] = HookSlot::Memo(MemoHookState {
            value: Some(Box::new(value.clone())),
            deps: Some(deps),
        });
        return value;
    }

    let node = node_rc.borrow();
    if let HookSlot::Memo(state) = &node.hooks[index] {
        if let Some(stored) = &state.value {
            if let Some(cast) = stored.downcast_ref::<T>() {
                return cast.clone();
            }
        }
    }
    panic!("use_memo: stored value type mismatch")
}

/// Memoized callback wrapper.
pub fn use_callback<F>(callback: F, deps: Vec<Value>) -> std::rc::Rc<F>
where
    F: 'static,
{
    use_memo(|| std::rc::Rc::new(callback), deps)
}

/// Register a handler for incoming messages of a given type.
pub fn use_message<S, F>(message_type: S, handler: F)
where
    S: Into<String>,
    F: Fn(&IncomingMessage) + 'static,
{
    let index = next_hook_index();
    let node_rc = current_node_rc();
    let mut node = node_rc.borrow_mut();
    ensure_slot_count(&mut node, index + 1);

    let message_type: String = message_type.into();
    let handler: MessageHandler = std::rc::Rc::new(handler);
    node.hooks[index] = HookSlot::Message(MessageHookState {
        message_type: message_type.clone(),
        handler: std::rc::Rc::clone(&handler),
    });
    node.message_handlers
        .entry(message_type)
        .or_default()
        .push(handler);
}

/// Returns a function that emits messages from the current node.
pub fn use_emit() -> std::rc::Rc<dyn Fn(&str, Value, EmitOptions)> {
    with_ctx(|c| std::rc::Rc::clone(&c.emit))
}

/// Snapshot of current neighbor IDs, optionally filtered by predicate.
pub fn use_neighbors<F>(filter: Option<F>) -> Vec<String>
where
    F: Fn(&str) -> bool,
{
    let neighbors = with_ctx(|c| c.neighbors.clone());
    match filter {
        Some(f) => neighbors.into_iter().filter(|id| f(id)).collect(),
        None => neighbors,
    }
}

/// Register a topology event handler.
pub fn use_topology<F>(handler: F, options: Option<TopologyFilterOptions>)
where
    F: Fn(&crate::graph::TopologyEvent) + 'static,
{
    let index = next_hook_index();
    let node_rc = current_node_rc();
    let mut node = node_rc.borrow_mut();
    ensure_slot_count(&mut node, index + 1);

    let handler: TopologyHandler = std::rc::Rc::new(handler);
    node.hooks[index] = HookSlot::Topology(TopologyHookState {
        handler: std::rc::Rc::clone(&handler),
        options: options.clone(),
    });
    node.topology_handlers.push((handler, options));
}

/// Subscribe to an external event source.
pub fn use_source<F>(factory: F, deps: Vec<Value>)
where
    F: FnOnce() -> Arc<dyn EventSource>,
{
    let index = next_hook_index();
    let node_rc = current_node_rc();
    let (register, unregister) = with_ctx(|c| {
        (
            std::rc::Rc::clone(&c.register_source),
            std::rc::Rc::clone(&c.unregister_source),
        )
    });

    let recreate = {
        let mut node = node_rc.borrow_mut();
        ensure_slot_count(&mut node, index + 1);
        match &node.hooks[index] {
            HookSlot::Source(state) => deps_changed(state.deps.as_ref(), Some(&deps)),
            _ => true,
        }
    };

    if !recreate {
        return;
    }

    // Stop any prior source for this slot before constructing the new one.
    let prev_source_id = {
        let mut node = node_rc.borrow_mut();
        if let HookSlot::Source(prev) = std::mem::replace(&mut node.hooks[index], HookSlot::Empty) {
            if let Some(src) = prev.source {
                src.stop();
                let id = src.id().to_string();
                node.active_sources.remove(&id);
                Some(id)
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some(id) = prev_source_id {
        unregister(&id);
    }

    // Build the new source outside any borrow.
    let source = factory();
    register(Arc::clone(&source));

    let mut node = node_rc.borrow_mut();
    node.active_sources
        .insert(source.id().to_string(), Arc::clone(&source));
    node.hooks[index] = HookSlot::Source(SourceHookState {
        source: Some(source),
        deps: Some(deps),
    });
}

// --- Supervision hooks (M5) ---

/// Watch another node by ID. The active node receives a `__DOWN__`
/// system message when the target unmounts. Re-registered on every
/// evaluation; `use_message("__DOWN__", ...)` consumes the events.
///
/// Returns `Some(MonitorRef)` if the target is currently registered
/// or — once the dist crate has installed a `RemoteMonitorSink`
/// (M14.5) — if the target is a `name@<other_node>` reachable via
/// the cluster. Returns `None` if the local id is unknown or the
/// remote target is requested without an installed sink.
pub fn use_monitor(target_id: &str) -> Option<crate::runtime::supervision::MonitorRef> {
    let _ = next_hook_index(); // Reserve a hook slot for ordering parity.
    with_ctx(|c| (c.register_monitor)(target_id))
}

/// Cancel a previously-registered monitor.
pub fn use_demonitor(mref: crate::runtime::supervision::MonitorRef) {
    let cancel = with_ctx(|c| std::rc::Rc::clone(&c.cancel_monitor));
    cancel(mref);
}

/// Establish a bidirectional link with another node. Re-asserted every
/// evaluation; if the target has unmounted between evaluations this
/// silently no-ops. Returns `true` if the link was (re-)established
/// — locally for a known id, or via the dist crate's
/// `RemoteLinkSink` for `name@<other_node>` targets (M14.5).
pub fn use_link(target_id: &str) -> bool {
    let _ = next_hook_index();
    with_ctx(|c| (c.register_link)(target_id))
}

/// Remove a bidirectional link. No-op if absent, the target has
/// unmounted, or the remote target's `RemoteUnlinkSink` isn't
/// installed.
pub fn use_unlink(target_id: &str) {
    with_ctx(|c| (c.unregister_link)(target_id));
}

/// Join a process group on every evaluation. Idempotent — multiple
/// evaluations don't create duplicate memberships. The scheduler
/// auto-removes the node from all groups on unmount.
pub fn use_group_join(group: &str) {
    let _ = next_hook_index();
    with_ctx(|c| (c.group_join)(group));
}

/// Leave a process group.
pub fn use_group_leave(group: &str) {
    with_ctx(|c| (c.group_leave)(group));
}

// --- ctx mutation surface (deferred via the scheduler's pending queue) ---
//
// Two flavors per operation:
//
// * `ctx_*` free functions read `HOOK_CTX` directly — only valid from
//   inside a component body (where the lifecycle has installed the
//   thread-local).
// * `use_*` hook accessors return the closure so it can be captured into
//   effects / message handlers and called later, after the component
//   body has returned and `HOOK_CTX` has been cleared. This mirrors the
//   `useEmit` pattern.

/// `ctx.spawnChild(spec)` — request a new child node be spawned. Only
/// valid in the component body. For body+effect use, prefer
/// [`use_spawn_child`].
pub fn ctx_spawn_child(spec: crate::runtime::types::ComposeChildSpec) {
    with_ctx(|c| (c.spawn_child)(spec));
}

/// Hook accessor: returns a closure that spawns a child node. Capture
/// during the component body, call from effects / handlers later.
pub fn use_spawn_child() -> std::rc::Rc<dyn Fn(crate::runtime::types::ComposeChildSpec)> {
    with_ctx(|c| std::rc::Rc::clone(&c.spawn_child))
}

/// `ctx.unmount()` — request the active node be unmounted. Component-body
/// only; for body+effect use, prefer [`use_unmount_self`].
pub fn ctx_unmount_self() {
    with_ctx(|c| (c.unmount_self)());
}

/// Hook accessor: returns a closure that unmounts the active node.
pub fn use_unmount_self() -> std::rc::Rc<dyn Fn()> {
    with_ctx(|c| std::rc::Rc::clone(&c.unmount_self))
}

/// `ctx.compose(specs)` — declare the active node's composed children.
/// Component-body only; for body+effect use, prefer [`use_compose`].
pub fn ctx_compose(specs: Vec<crate::runtime::types::ComposeChildSpec>) {
    with_ctx(|c| (c.compose)(specs));
}

/// Hook accessor: returns a closure that sets the active node's
/// composed-children list and spawns any missing entries.
pub fn use_compose() -> std::rc::Rc<dyn Fn(Vec<crate::runtime::types::ComposeChildSpec>)> {
    with_ctx(|c| std::rc::Rc::clone(&c.compose))
}

/// `ctx.acceptEdge(targetId)` — Phase-9 edge negotiation. Synchronously
/// adds a graph edge between the active node and `target_id`. Component-
/// body only; for body+effect/handler use, prefer [`use_accept_edge`].
pub fn ctx_accept_edge(target_id: &str) {
    with_ctx(|c| (c.accept_edge)(target_id));
}

/// Hook accessor: returns a closure that accepts an edge from the active
/// node to a target by id. Capture during the body, call from effects /
/// handlers later.
pub fn use_accept_edge() -> std::rc::Rc<dyn Fn(&str)> {
    with_ctx(|c| std::rc::Rc::clone(&c.accept_edge))
}

/// Install a predicate that decides whether THIS node would accept an
/// incoming edge / Tier 3 reach for a given `(requesting_source_id,
/// msg_type)`. The predicate is consulted by:
///
/// * `drain_pending_mutations::AcceptEdge` — vetoes the edge if the
///   predicate returns `false` (msgType is the empty string in this path
///   because Phase-9 `useAcceptEdge` is channel-shaped, not message-typed).
/// * Tier 3 graph queries (`g.tier3(msgType)`) — restricts the directed
///   reachability set to (source, target) pairs the target would accept.
///
/// Nodes that don't call this hook leave the predicate `None`, which
/// preserves the Phase-9 default-accept behavior. The predicate is
/// re-installed on every render; conditional hook calls revert to
/// default-accept on the next render where the call is omitted.
/// M12.3a-followon-F.
pub fn use_edge_accept_handler<F>(predicate: F)
where
    F: Fn(&str, &str) -> bool + 'static,
{
    let _ = next_hook_index();
    let predicate: crate::runtime::node_instance::EdgeAcceptPredicate = std::rc::Rc::new(predicate);
    with_ctx(|c| (c.set_edge_accept_handler)(predicate));
}

// --- Helpers ---

fn ensure_slot_count(node: &mut NodeInstance, count: usize) {
    while node.hooks.len() < count {
        node.hooks.push(HookSlot::Empty);
    }
}

fn deps_changed(old: Option<&Vec<Value>>, new: Option<&Vec<Value>>) -> bool {
    match (old, new) {
        (Some(o), Some(n)) if o.len() == n.len() => o.iter().zip(n.iter()).any(|(a, b)| a != b),
        _ => true,
    }
}
