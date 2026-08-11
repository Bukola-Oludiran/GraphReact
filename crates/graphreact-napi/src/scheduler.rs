//! `Scheduler` — JS-facing wrapper over the Rust runtime scheduler.
//!
//! Architecture A (the chosen path): JS-defined components are stored as
//! `Reference<()>` handles to JS functions. During node evaluation the
//! Rust closure captured in `NodeDefinition` looks up the function via the
//! thread-local `Env` and calls it synchronously. The JS function returns
//! a manifest object that the bridge converts to `PermissionManifest`.
//!
//! All scheduler entry points (start, process_batch, send, etc.) install a
//! thread-local `Env` for the duration of the call so any nested calls
//! from JS-side hooks (and from the Rust-stored component closures) can
//! find it.
//!
//! ## M6.5 — async cascade
//!
//! Synchronous evaluation runs on the V8 main thread (this thread) and
//! returns when the scheduler is sync-quiescent. Async effects from
//! `useEffectAsync` are NOT awaited inside Rust — the JS Promise is
//! captured into a per-scheduler queue and surfaced back to JS via the
//! `runXRound` family. The TS-side wrapper drives the multi-round cascade
//! via `Promise.all`, letting V8 microtasks flow naturally between rounds.
//! Async cleanups (Promises that resolve to functions) are installed back
//! via `installAsyncCleanup`. See [crate::hooks::js_use_effect_async] for
//! the capture path.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::rc::Rc;

use graphreact_core::{NodeContext, NodeDefinition, PermissionManifest, Scheduler};
use napi::threadsafe_function::{
    ThreadSafeCallContext, ThreadsafeFunction, ThreadsafeFunctionCallMode,
};
use napi::{Env, JsFunction, JsObject, JsUnknown, Ref, Result};
use serde_json::Value;

use crate::conversion::{attrs_to_json, err_to_napi, json_to_attrs};
use crate::graph::JsCommunicationGraph;

thread_local! {
    /// Set on entry to any `JsScheduler` method that may trigger node
    /// evaluation. Read by the Rust closure stored on a JS-defined
    /// `NodeDefinition` so it can call back into JS via napi-rs.
    static CURRENT_ENV: Cell<Option<Env>> = const { Cell::new(None) };

    /// Active async pending state for the current scheduler entry. Set by
    /// the `runXRound` family so `useEffectAsync` can capture Promises into
    /// the right scheduler's queue. None during purely-sync entries
    /// (`start`, `processBatch`, `send`, `stop`) — `useEffectAsync` then
    /// fires and discards the Promise (with a console warning).
    static CURRENT_ASYNC_PENDING: RefCell<Option<Rc<AsyncPendingState>>> = const { RefCell::new(None) };
}

/// RAII guard — pushes the active `Env` onto the thread-local on
/// construction and clears it on drop. Any panic or early return cleans
/// up automatically.
struct EnvGuard;

impl EnvGuard {
    fn new(env: Env) -> Self {
        CURRENT_ENV.with(|cell| cell.set(Some(env)));
        Self
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        CURRENT_ENV.with(|cell| cell.set(None));
    }
}

/// Read the current `Env`. Panics outside any scheduler entry — the closure
/// stored on a JS-defined `NodeDefinition` must only run during a scheduler
/// pass that installed an `EnvGuard`.
pub(crate) fn current_env() -> Env {
    CURRENT_ENV.with(Cell::get).expect(
        "current_env: no Env on thread-local — JS component invoked outside a scheduler entry",
    )
}

/// One slot in the async cleanup map. Tracks the race between the
/// effect's cleanup closure (fires when deps change or the node
/// unmounts) and `installAsyncCleanup` (fires when the captured Promise
/// resolves to a function). Whichever lands first leaves a marker; the
/// second arrival reads it and acts accordingly.
enum SlotEntry {
    /// Cleanup function ready to run on the next dep-change / unmount.
    /// Installed by `install_cleanup` when the cleanup closure hasn't
    /// run yet for this slot.
    Cleanup(Rc<ManuallyDrop<Ref<()>>>),
    /// The cleanup closure ran but the Promise hadn't resolved yet —
    /// the slot is "orphaned." When `install_cleanup` later receives
    /// the cleanup function, it must run immediately because the slot's
    /// owner has already been re-run / unmounted.
    Obsolete,
}

/// Per-scheduler async machinery. Holds the queue of in-flight Promises
/// captured during the current round, and the slot map of cleanup
/// functions installed back from JS after Promises resolve.
///
/// Lives on `JsScheduler` as `Rc<AsyncPendingState>`. The TLS pointer
/// in `CURRENT_ASYNC_PENDING` is set during `runXRound` entry so the
/// `useEffectAsync` hook can find the active scheduler's queue without
/// the hook needing a back-reference to the scheduler instance.
pub(crate) struct AsyncPendingState {
    /// Captured Promises from `useEffectAsync` calls in the current round.
    /// Drained at the end of each `runXRound` and shipped back to JS.
    pending: RefCell<Vec<PendingAsyncEntry>>,
    /// Cleanup-slot map: slot id → either a captured cleanup ready to
    /// fire, or an obsolete marker. See [`SlotEntry`].
    cleanups: RefCell<HashMap<u32, SlotEntry>>,
    /// Monotonic id allocator. One id per `useEffectAsync` invocation per
    /// re-run — fresh slot per run avoids collisions when a Promise is
    /// still in flight while deps change and the effect re-fires.
    next_id: Cell<u32>,
}

/// A single Promise captured during a round, addressable by `slot_id` so
/// the JS-side `Promise.all`-driver can route the resolved cleanup
/// function (if any) back via `installAsyncCleanup(slot_id, fn)`.
pub(crate) struct PendingAsyncEntry {
    pub(crate) slot_id: u32,
    pub(crate) promise: Rc<ManuallyDrop<Ref<()>>>,
}

impl AsyncPendingState {
    pub(crate) fn new() -> Self {
        Self {
            pending: RefCell::new(Vec::new()),
            cleanups: RefCell::new(HashMap::new()),
            next_id: Cell::new(0),
        }
    }

    /// Allocate a fresh slot id for this effect run.
    pub(crate) fn next_slot_id(&self) -> u32 {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        id
    }

    /// Queue a captured Promise for the round's drain.
    pub(crate) fn push_pending(&self, slot_id: u32, promise: Rc<ManuallyDrop<Ref<()>>>) {
        self.pending
            .borrow_mut()
            .push(PendingAsyncEntry { slot_id, promise });
    }

    /// Drain all pending entries — called once per round before returning
    /// the result to JS.
    pub(crate) fn take_pending(&self) -> Vec<PendingAsyncEntry> {
        std::mem::take(&mut *self.pending.borrow_mut())
    }

    /// Install a cleanup function for `slot_id`.
    ///
    /// Returns `Some(cleanup_ref)` iff the slot was already marked
    /// [`SlotEntry::Obsolete`] — the caller is then responsible for
    /// invoking the cleanup immediately on the V8 thread (the effect
    /// owner has already moved on, so deferring the cleanup would never
    /// run it). Returns `None` in the common case where the slot is
    /// fresh: the cleanup is parked until the next dep-change / unmount.
    #[must_use]
    pub(crate) fn install_cleanup(
        &self,
        slot_id: u32,
        cleanup: Rc<ManuallyDrop<Ref<()>>>,
    ) -> Option<Rc<ManuallyDrop<Ref<()>>>> {
        let mut map = self.cleanups.borrow_mut();
        match map.remove(&slot_id) {
            Some(SlotEntry::Obsolete) => Some(cleanup),
            // Cleanup arrives over an already-installed cleanup — rare
            // (means the same slot id resolved twice), but accept by
            // overwriting; the prior one never gets run.
            _ => {
                map.insert(slot_id, SlotEntry::Cleanup(cleanup));
                None
            }
        }
    }

    /// Pop the cleanup for `slot_id` if installed.
    ///
    /// If the slot is empty, leaves an [`SlotEntry::Obsolete`] marker so
    /// a later [`Self::install_cleanup`] runs the cleanup immediately
    /// (rather than parking it for a re-run that will never come).
    pub(crate) fn take_cleanup(&self, slot_id: u32) -> Option<Rc<ManuallyDrop<Ref<()>>>> {
        let mut map = self.cleanups.borrow_mut();
        match map.remove(&slot_id) {
            Some(SlotEntry::Cleanup(f)) => Some(f),
            Some(SlotEntry::Obsolete) => None,
            None => {
                map.insert(slot_id, SlotEntry::Obsolete);
                None
            }
        }
    }
}

/// RAII guard — installs a scheduler's async pending state into the TLS
/// slot for the duration of a `runXRound` entry. Cleared on drop so a
/// subsequent purely-sync call doesn't accidentally capture into a stale
/// state.
struct AsyncPendingGuard;

impl AsyncPendingGuard {
    fn new(state: Rc<AsyncPendingState>) -> Self {
        CURRENT_ASYNC_PENDING.with(|cell| {
            *cell.borrow_mut() = Some(state);
        });
        Self
    }
}

impl Drop for AsyncPendingGuard {
    fn drop(&mut self) {
        CURRENT_ASYNC_PENDING.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Read the active async pending state. Returns `None` outside a
/// `runXRound` entry — `useEffectAsync` falls back to fire-and-forget
/// behaviour with a warning when this happens.
pub(crate) fn current_async_pending() -> Option<Rc<AsyncPendingState>> {
    CURRENT_ASYNC_PENDING.with(|cell| cell.borrow().clone())
}

/// Reactive scheduler exposed to JS.
#[napi(js_name = "Scheduler")]
pub struct JsScheduler {
    pub(crate) inner: Scheduler,
    /// Per-scheduler async machinery — holds in-flight Promises +
    /// cleanup slots. Wrapped in `Rc` so the TLS guard can borrow it for
    /// the duration of a `runXRound` entry without needing `&mut self`
    /// access for the hook side.
    pub(crate) async_state: Rc<AsyncPendingState>,
}

#[napi]
impl JsScheduler {
    /// Construct a scheduler over a graph. The graph is moved into the
    /// scheduler — JS retains the wrapper but mutations no longer flow
    /// through it (use scheduler methods to mutate at runtime).
    #[napi(constructor)]
    pub fn new(graph: &mut JsCommunicationGraph) -> Self {
        // Take ownership of the inner graph. We swap with a fresh empty
        // graph so the JS-side `Graph` object remains valid (its inner
        // is replaced, not deallocated). After this point, graph
        // mutations on the JS-held `Graph` won't affect the scheduler.
        let inner_graph = std::mem::take(&mut graph.inner);
        Self {
            inner: Scheduler::new(inner_graph),
            async_state: Rc::new(AsyncPendingState::new()),
        }
    }

    /// Start the scheduler — mount all registered nodes synchronously.
    ///
    /// Wrapped in `within_runtime_if_available` so any `tokio::spawn`
    /// calls made during effect flushing — most notably from event
    /// sources registered via [`crate::hooks::js_use_source`] — execute
    /// against napi-rs's bundled multi-thread runtime instead of
    /// panicking with "no runtime context."
    #[napi]
    pub fn start(&mut self, env: Env) -> Result<()> {
        let _guard = EnvGuard::new(env);
        napi::bindgen_prelude::within_runtime_if_available(|| {
            self.inner.start().map_err(err_to_napi)
        })
    }

    /// Drain the dirty queue + message queue until quiescent. See
    /// [`Self::start`] for the runtime-context note.
    #[napi(js_name = "processBatch")]
    pub fn process_batch(&mut self, env: Env) -> Result<()> {
        let _guard = EnvGuard::new(env);
        napi::bindgen_prelude::within_runtime_if_available(|| {
            self.inner.process_batch().map_err(err_to_napi)
        })
    }

    /// Inject a host-side message into a node. See [`Self::start`] for
    /// the runtime-context note.
    #[napi]
    pub fn send(&mut self, env: Env, target: String, r#type: String, payload: Value) -> Result<()> {
        let _guard = EnvGuard::new(env);
        napi::bindgen_prelude::within_runtime_if_available(|| {
            self.inner.send(&target, &r#type, payload);
            self.inner.process_batch().map_err(err_to_napi)
        })
    }

    /// Stop — unmount every node, run cleanups, drop pending work. See
    /// [`Self::start`] for the runtime-context note.
    #[napi]
    pub fn stop(&mut self, env: Env) {
        let _guard = EnvGuard::new(env);
        napi::bindgen_prelude::within_runtime_if_available(|| {
            self.inner.stop();
        });
    }

    /// Whether a node with the given ID is currently registered.
    #[napi(js_name = "hasNode")]
    pub fn has_node(&self, id: String) -> bool {
        self.inner.has_node(&id)
    }

    /// All node IDs currently in the live graph. The JS-side `Graph`
    /// wrapper passed to `new Scheduler()` is moved-from at construction
    /// — these accessors expose the scheduler's *internal* graph for
    /// `useGraphQuery` and other introspection.
    #[napi(js_name = "graphNodes")]
    pub fn graph_nodes(&self) -> Vec<String> {
        self.inner
            .graph
            .borrow()
            .nodes()
            .map(String::from)
            .collect()
    }

    /// Direct neighbours of `id` in the live graph.
    #[napi(js_name = "graphNeighbors")]
    pub fn graph_neighbors(&self, id: String) -> Result<Vec<String>> {
        self.inner
            .graph
            .borrow()
            .neighbors(&id)
            .map_err(err_to_napi)
    }

    /// Whether the live graph has an edge between `u` and `v`. `key` is
    /// optional; `None` matches any multi-edge between the pair.
    #[napi(js_name = "graphHasEdge")]
    pub fn graph_has_edge(&self, u: String, v: String, key: Option<String>) -> bool {
        self.inner.graph.borrow().has_edge(&u, &v, key.as_deref())
    }

    /// Live-graph node attribute snapshot. Errors if the node is absent.
    #[napi(js_name = "graphGetNodeData")]
    pub fn graph_get_node_data(&self, id: String) -> Result<Value> {
        let g = self.inner.graph.borrow();
        let attrs = g.get_node_data(&id).map_err(err_to_napi)?;
        Ok(attrs_to_json(attrs))
    }

    /// Override the per-instance cap on `process_batch` cascade rounds.
    /// Default is 100. Bump this for graphs with legitimately deep
    /// cascades (recursive type trees, deep dependency DAGs).
    #[napi(js_name = "setMaxProcessDepth")]
    pub fn set_max_process_depth(&mut self, max: u32) {
        self.inner.set_max_process_depth(max as usize);
    }

    /// Current cascade-round cap.
    #[napi(js_name = "maxProcessDepth", getter)]
    pub fn max_process_depth(&self) -> u32 {
        u32::try_from(self.inner.max_process_depth()).unwrap_or(u32::MAX)
    }

    /// Validate the graph against the manifests collected from registered
    /// nodes. Mirrors the TS `validateGraph` — call AFTER `start()` so the
    /// scheduler has populated manifests by evaluating each node once.
    ///
    /// Returns an array of `{ level, message }` objects.
    #[napi(js_name = "validateGraph")]
    pub fn validate_graph(&self) -> Vec<JsValidationWarning> {
        use graphreact_algorithms::validation::{validate_graph, WarningLevel};
        let manifests = self.inner.snapshot_manifests();
        validate_graph(&self.inner.graph.borrow(), &manifests)
            .into_iter()
            .map(|w| JsValidationWarning {
                level: match w.level {
                    WarningLevel::Warn => "warn".to_string(),
                    WarningLevel::Error => "error".to_string(),
                },
                message: w.message,
            })
            .collect()
    }

    /// Add a node to the running scheduler. Mirrors TS `addNodeAtRuntime`.
    /// Triggers immediate evaluation; neighbors are marked dirty.
    #[napi(js_name = "addNodeAtRuntime")]
    pub fn add_node_at_runtime(
        &mut self,
        env: Env,
        id: String,
        definition: JsObject,
        props: Option<Value>,
        connect_to: Option<Vec<String>>,
    ) -> Result<()> {
        let _guard = EnvGuard::new(env);
        let node_def = build_node_definition(env, definition)?;
        let attrs = props.map(json_to_attrs).unwrap_or_default();
        let connect_to = connect_to.unwrap_or_default();
        self.inner
            .add_node_at_runtime(node_def, id, attrs, connect_to)
            .map(|_| ())
            .map_err(err_to_napi)
    }

    /// Remove a node from the running scheduler. Mirrors TS
    /// `removeNodeAtRuntime`. Cleanups run; neighbors are marked dirty.
    #[napi(js_name = "removeNodeAtRuntime")]
    pub fn remove_node_at_runtime(&mut self, env: Env, id: String) -> Result<()> {
        let _guard = EnvGuard::new(env);
        self.inner.remove_node_at_runtime(&id).map_err(err_to_napi)
    }

    /// Hot-reload: replace a node's component function while preserving
    /// its hook state. The node is marked dirty and re-evaluates on the
    /// next batch with the new behavior.
    #[napi(js_name = "replaceComponent")]
    pub fn replace_component(&mut self, env: Env, id: String, definition: JsObject) -> Result<()> {
        let _guard = EnvGuard::new(env);
        let new_def = build_node_definition(env, definition)?;
        self.inner
            .replace_definition(&id, new_def)
            .map_err(err_to_napi)
    }

    /// Run one synchronous evaluation round of the initial mount cycle
    /// and return any Promises captured from `useEffectAsync` calls.
    ///
    /// Mirrors [`Self::start`] but exposes the async-effect handoff
    /// surface. The TS-side `Scheduler.startAsync` wrapper drives the
    /// multi-round cascade: await `Promise.all(pendingPromises)`, then
    /// call `runBatchRound` until `pendingPromises` is empty and
    /// `hasMoreWork` is false.
    #[napi(js_name = "runStartRound")]
    pub fn run_start_round(&mut self, env: Env) -> Result<AsyncRoundResult> {
        let _env_guard = EnvGuard::new(env);
        let _async_guard = AsyncPendingGuard::new(Rc::clone(&self.async_state));
        napi::bindgen_prelude::within_runtime_if_available(|| {
            self.inner.start().map_err(err_to_napi)
        })?;
        self.collect_round_result(env)
    }

    /// Run one synchronous round of `process_batch` and return any
    /// captured Promises. Used by the TS wrapper to advance the cascade
    /// after awaiting prior-round Promises.
    #[napi(js_name = "runBatchRound")]
    pub fn run_batch_round(&mut self, env: Env) -> Result<AsyncRoundResult> {
        let _env_guard = EnvGuard::new(env);
        let _async_guard = AsyncPendingGuard::new(Rc::clone(&self.async_state));
        napi::bindgen_prelude::within_runtime_if_available(|| {
            self.inner.process_batch().map_err(err_to_napi)
        })?;
        self.collect_round_result(env)
    }

    /// Inject a host-side message and run a synchronous round, returning
    /// any captured Promises. TS-side `sendAsync` wraps this and the
    /// follow-on `runBatchRound` calls.
    #[napi(js_name = "runSendRound")]
    pub fn run_send_round(
        &mut self,
        env: Env,
        target: String,
        r#type: String,
        payload: Value,
    ) -> Result<AsyncRoundResult> {
        let _env_guard = EnvGuard::new(env);
        let _async_guard = AsyncPendingGuard::new(Rc::clone(&self.async_state));
        napi::bindgen_prelude::within_runtime_if_available(|| {
            self.inner.send(&target, &r#type, payload);
            self.inner.process_batch().map_err(err_to_napi)
        })?;
        self.collect_round_result(env)
    }

    /// Run unmount-with-cleanups, returning any captured Promises. Async
    /// cleanups installed via `installAsyncCleanup` run synchronously as
    /// part of the inner `stop()` (they're plain JS function calls). The
    /// returned `pendingPromises` covers any *new* `useEffectAsync` calls
    /// that fire during a final flush before unmount (typically empty).
    #[napi(js_name = "runStopRound")]
    pub fn run_stop_round(&mut self, env: Env) -> Result<AsyncRoundResult> {
        let _env_guard = EnvGuard::new(env);
        let _async_guard = AsyncPendingGuard::new(Rc::clone(&self.async_state));
        napi::bindgen_prelude::within_runtime_if_available(|| {
            self.inner.stop();
        });
        self.collect_round_result(env)
    }

    /// Install the JS-side source-wakeup callback.
    ///
    /// Sources fire on tokio worker threads (timer ticks, HTTP
    /// connections, fs-watch events) and push events into the
    /// scheduler's cross-thread external inbox. Without a wake-up
    /// bridge, those events sit until a sync round happens to drain
    /// the inbox. This method registers a JS callback that the source
    /// bridge invokes via [`ThreadsafeFunction`] from any thread —
    /// the call lands on the V8 main thread on V8's next event-loop
    /// iteration. The callback typically calls `processBatchAsync` to
    /// drain.
    ///
    /// The TS-side `Scheduler` subclass installs this automatically
    /// in its constructor; users who construct the native class
    /// directly can install their own.
    #[napi(js_name = "installSourceWakeup")]
    pub fn install_source_wakeup(&mut self, env: Env, callback: JsFunction) -> Result<()> {
        let tsfn: ThreadsafeFunction<()> =
            env.create_threadsafe_function(&callback, 0, |_ctx: ThreadSafeCallContext<()>| {
                Ok(Vec::<JsUnknown>::new())
            })?;
        let wake: std::sync::Arc<dyn Fn() + Send + Sync> = {
            let tsfn = tsfn.clone();
            std::sync::Arc::new(move || {
                // `Ok(())` because the default `CalleeHandled` error
                // strategy expects a Result; rejection delivery is
                // userland's responsibility — we never reject.
                tsfn.call(Ok(()), ThreadsafeFunctionCallMode::NonBlocking);
            })
        };
        self.inner.set_external_wake(Some(wake));
        Ok(())
    }

    /// Install a JS cleanup function for `cleanup_id`. Called from the
    /// TS-side wrapper after `await promise` resolved to a function.
    /// The function is consumed by the next dep-change re-run / unmount
    /// of the originating `useEffectAsync` call.
    ///
    /// Race handling: if the cleanup closure already ran (the effect
    /// re-fired or the node unmounted before the Promise resolved), the
    /// slot is marked obsolete and we invoke the cleanup synchronously
    /// here — otherwise it would never run.
    #[napi(js_name = "installAsyncCleanup")]
    pub fn install_async_cleanup(
        &mut self,
        env: Env,
        cleanup_id: u32,
        cleanup_fn: JsFunction,
    ) -> Result<()> {
        // `Ref<()>` — same `ManuallyDrop` trick as elsewhere; constant
        // leak per orphaned cleanup is acceptable.
        let cleanup_ref: Rc<ManuallyDrop<Ref<()>>> =
            Rc::new(ManuallyDrop::new(env.create_reference(cleanup_fn)?));
        if let Some(immediate) = self.async_state.install_cleanup(cleanup_id, cleanup_ref) {
            // Slot was already obsolete — run cleanup now.
            let cleanup_fn: JsFunction = env.get_reference_value(&immediate)?;
            if let Err(e) = cleanup_fn.call_without_args(None) {
                eprintln!("[graphreact-napi] obsolete async cleanup error: {e}");
            }
        }
        Ok(())
    }

    /// Drain the round's captured Promises, materialise them as JS
    /// objects, and report whether further rounds are likely needed.
    /// `has_more_work` is `true` iff at least one Promise was captured —
    /// resolution may produce new dirty state, so the TS driver always
    /// re-enters with `runBatchRound` after awaiting.
    fn collect_round_result(&self, env: Env) -> Result<AsyncRoundResult> {
        let pending = self.async_state.take_pending();
        let has_more_work = !pending.is_empty();
        let mut effects: Vec<PendingAsyncEffect> = Vec::with_capacity(pending.len());
        for entry in pending {
            let promise: JsObject = env.get_reference_value(&entry.promise)?;
            effects.push(PendingAsyncEffect {
                promise,
                cleanup_id: entry.slot_id,
            });
        }
        Ok(AsyncRoundResult {
            pending_promises: effects,
            has_more_work,
        })
    }

    /// Register a node defined by a JS component function.
    ///
    /// `definition` is a JS object of shape `{ name: string, component: Function }`.
    /// `component(ctx)` is called synchronously by the scheduler during
    /// evaluation and must return `{ accepts: string[], emits: string[] }`
    /// (channels are parsed in a follow-up sub-task).
    ///
    /// The JS function reference is held via `napi::Ref<()>` for the life
    /// of the registered closure — it's not unref'd on stop. Hot-reload
    /// (M6 stage 3 follow-up: `replaceComponent`) is the explicit path
    /// for swapping a node's behavior.
    #[napi(js_name = "registerNode")]
    pub fn register_node(
        &mut self,
        env: Env,
        id: String,
        definition: JsObject,
        props: Option<Value>,
        height: Option<u32>,
    ) -> Result<()> {
        let _guard = EnvGuard::new(env);
        let node_def = build_node_definition(env, definition)?;
        let attrs = props.map(json_to_attrs).unwrap_or_default();
        let height = height.unwrap_or(0);
        self.inner
            .register_node(node_def, id, attrs, height)
            .map(|_| ())
            .map_err(err_to_napi)
    }
}

/// JS shape of a validation finding from `validateGraph`.
#[napi(object, js_name = "ValidationWarning")]
pub struct JsValidationWarning {
    /// `"warn"` or `"error"`.
    pub level: String,
    pub message: String,
}

/// One Promise captured during an async round, paired with the slot id
/// the TS wrapper uses to route the resolved cleanup function (if any)
/// back via `installAsyncCleanup`.
#[napi(object, js_name = "PendingAsyncEffect")]
pub struct PendingAsyncEffect {
    /// The JS Promise the wrapper should `await`.
    pub promise: JsObject,
    /// Routing key for `installAsyncCleanup(cleanupId, fn)`.
    #[napi(js_name = "cleanupId")]
    pub cleanup_id: u32,
}

/// Result of one synchronous evaluation round. `pendingPromises` is the
/// set of in-flight `useEffectAsync` Promises captured this round.
/// `hasMoreWork` is `true` iff resolution may produce more dirty state
/// (which the TS driver translates into another `runBatchRound`).
#[napi(object, js_name = "AsyncRoundResult")]
pub struct AsyncRoundResult {
    #[napi(js_name = "pendingPromises")]
    pub pending_promises: Vec<PendingAsyncEffect>,
    #[napi(js_name = "hasMoreWork")]
    pub has_more_work: bool,
}

/// Build a Rust `NodeDefinition` from a JS `{ name, component }` object.
///
/// The JS function is held via `napi::Ref<()>` for the life of the
/// returned `NodeDefinition`. Each evaluation fetches the function from
/// the thread-local `Env`, builds the JS `ctx` object, calls
/// synchronously, and parses the returned manifest.
pub(crate) fn build_node_definition(env: Env, definition: JsObject) -> Result<NodeDefinition> {
    let name: String = definition
        .get_named_property::<JsUnknown>("name")?
        .coerce_to_string()?
        .into_utf8()?
        .into_owned()?;
    let component_fn: JsFunction = definition.get_named_property("component")?;
    // `Ref<()>` panics on Drop if the napi ref count is non-zero, which
    // it always is during process teardown (V8 is mid-shutdown when
    // finalizers run). Wrap in `ManuallyDrop` so the closure can drop
    // freely; we accept a small constant leak (one napi ref per
    // registered component). M6 stage-4 follow-up: track refs on
    // `JsScheduler` and `unref(env)` them explicitly during `stop`.
    let component_ref: Rc<ManuallyDrop<Ref<()>>> =
        Rc::new(ManuallyDrop::new(env.create_reference(component_fn)?));
    let component_closure = move |ctx: &NodeContext| -> PermissionManifest {
        let env = current_env();
        let component_fn: JsFunction = env
            .get_reference_value(&component_ref)
            .expect("component reference is invalid (already unref'd?)");
        let js_ctx = build_js_context(env, ctx).expect("failed to build JS NodeContext object");
        let result = component_fn
            .call(None, &[js_ctx])
            .expect("JS component function threw");
        parse_manifest(env, result).unwrap_or_default()
    };
    Ok(NodeDefinition::new(name, component_closure))
}

/// JS shape of `ComposeChildSpec` — `{ id, definition, props?, connectTo? }`.
#[napi(object, js_name = "ComposeChildSpec")]
pub struct JsComposeChildSpec {
    pub id: String,
    pub definition: JsObject,
    pub props: Option<Value>,
    #[napi(js_name = "connectTo")]
    pub connect_to: Option<Vec<String>>,
}

pub(crate) fn build_compose_spec(
    env: Env,
    spec: JsComposeChildSpec,
) -> Result<graphreact_core::ComposeChildSpec> {
    Ok(graphreact_core::ComposeChildSpec {
        id: spec.id,
        definition: build_node_definition(env, spec.definition)?,
        props: spec.props.map(json_to_attrs).unwrap_or_default(),
        connect_to: spec.connect_to.unwrap_or_default(),
    })
}

/// Build the JS-side `ctx` object handed to a component callback.
///
/// Stage-3 surface: `id`, `props`, `neighbors`, `myDepth`, `myPath`,
/// `children`. The hook-system additions (`spawnChild`, `unmount`,
/// `lookupComponent`, `compose`) are added in follow-up sub-tasks.
fn build_js_context(env: Env, ctx: &NodeContext) -> Result<JsObject> {
    let mut obj = env.create_object()?;
    obj.set_named_property("id", env.create_string(&ctx.id)?)?;
    obj.set_named_property("props", env.to_js_value(&attrs_to_json(&ctx.props))?)?;
    obj.set_named_property("neighbors", env.to_js_value(&ctx.neighbors)?)?;
    obj.set_named_property("myDepth", env.create_uint32(ctx.my_depth)?)?;
    obj.set_named_property("myPath", env.to_js_value(&ctx.my_path)?)?;
    let children: Vec<serde_json::Value> = ctx
        .children
        .iter()
        .map(|c| serde_json::json!({ "id": c.id, "component": c.component }))
        .collect();
    obj.set_named_property("children", env.to_js_value(&children)?)?;
    Ok(obj)
}

/// Parse a JS-returned manifest object into a `PermissionManifest`.
///
/// Stage-3 surface: flat `accepts` / `emits` arrays. `channels` are parsed
/// in a follow-up sub-task once per-neighbor policies have a clean JS
/// shape and a Rust transform-fn lookup story.
fn parse_manifest(env: Env, result: JsUnknown) -> Result<PermissionManifest> {
    if result.get_type()? == napi::ValueType::Undefined
        || result.get_type()? == napi::ValueType::Null
    {
        return Ok(PermissionManifest::default());
    }
    let value: Value = env.from_js_value(result)?;
    let mut manifest = PermissionManifest::default();
    if let Value::Object(map) = value {
        if let Some(Value::Array(accepts)) = map.get("accepts") {
            manifest.accepts = accepts
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
        if let Some(Value::Array(emits)) = map.get("emits") {
            manifest.emits = emits
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }
    Ok(manifest)
}
