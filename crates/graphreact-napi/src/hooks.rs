//! JS-facing hook surface — `useState`, `emit`, `useMessage`, `useEffect`.
//!
//! These functions read the same thread-local context the Rust-native
//! hooks read; they only run correctly when called from inside a JS
//! component body (which is itself called synchronously from
//! `JsScheduler` entry points that install an `EnvGuard`).
//!
//! The JS-side `useEffect` covers the *sync* path. The async variant is
//! exposed as `useEffectAsync` and the JS package's `useEffect` chooses
//! between the two based on whether the supplied callback is an
//! `AsyncFunction` (auto-detect at registration time, per design memo).

use std::mem::ManuallyDrop;
use std::rc::Rc;

use graphreact_core::{
    delay_source as core_delay_source, timer_source as core_timer_source,
    use_accept_edge as core_use_accept_edge, use_compose as core_use_compose,
    use_demonitor as core_use_demonitor, use_effect as core_use_effect,
    use_emit as core_use_emit, use_group_join as core_use_group_join,
    use_group_leave as core_use_group_leave, use_link as core_use_link,
    use_memo as core_use_memo, use_message as core_use_message, use_monitor as core_use_monitor,
    use_neighbors as core_use_neighbors, use_source as core_use_source,
    use_spawn_child as core_use_spawn_child, use_state as core_use_state,
    use_topology as core_use_topology, use_unlink as core_use_unlink,
    use_unmount_self as core_use_unmount_self, ComposeChildSpec, EffectCleanup,
    EmitOptions as CoreEmitOptions, IncomingMessage, MonitorRef as CoreMonitorRef,
    Signal as CoreSignal, TopologyEvent, TopologyEventKind, TopologyFilterOptions,
};
use napi::{Env, JsFunction, JsUnknown, Ref, Result};
use serde_json::Value;

use crate::scheduler::{current_async_pending, current_env};

/// Reactive cell handle exposed to JS as `Signal`.
///
/// Backed by a Rust-core `Signal<serde_json::Value>` — JS values cross
/// the bridge as `serde_json::Value` for parity with the rest of the
/// runtime (graph attributes, message payloads, manifests).
///
/// Rust-side struct name matches the JS-side class name to avoid
/// napi-derive's `js_name`-on-struct lookup quirks (the macro
/// occasionally fails to find the constructor in release builds when
/// the names diverge).
#[napi]
pub struct Signal {
    inner: CoreSignal<Value>,
}

#[napi]
impl Signal {
    /// Read the current value.
    #[napi]
    pub fn get(&self) -> Value {
        self.inner.get()
    }

    /// Replace the value. Triggers re-evaluation if the new value differs.
    #[napi]
    pub fn set(&self, value: Value) {
        self.inner.set(value);
    }

    /// `update(fn)` — apply a JS function to the current value, store the
    /// result. The JS function receives the current value and must return
    /// the next one synchronously.
    #[napi]
    pub fn update(&self, env: Env, updater: JsFunction) -> Result<()> {
        let current = self.inner.get();
        let arg = env.to_js_value(&current)?;
        let result = updater.call(None, &[arg])?;
        let next: Value = env.from_js_value(result)?;
        self.inner.set(next);
        Ok(())
    }
}

/// `useState(initial)` — register a state hook on the active node. Returns
/// a `Signal` handle with `get`/`set`/`update` methods.
#[napi(js_name = "useState")]
pub fn js_use_state(initial: Value) -> Signal {
    Signal {
        inner: core_use_state::<Value>(initial),
    }
}

/// Optional steering for `emit`. JS shape: `{ to?, ttl?, edgeKey?, direct? }`.
#[napi(object)]
pub struct EmitOptions {
    pub to: Option<Vec<String>>,
    pub ttl: Option<u32>,
    #[napi(js_name = "edgeKey")]
    pub edge_key: Option<String>,
    pub direct: Option<String>,
}

impl From<EmitOptions> for CoreEmitOptions {
    fn from(o: EmitOptions) -> Self {
        CoreEmitOptions {
            to: o.to,
            ttl: o.ttl,
            edge_key: o.edge_key,
            direct: o.direct,
        }
    }
}

/// `emit(type, payload, options?)` — send a message from the active node.
///
/// The JS-side `useEmit` hook returns this function directly.
#[napi(js_name = "emit")]
pub fn js_emit(r#type: String, payload: Value, options: Option<EmitOptions>) {
    let emit = core_use_emit();
    let opts = options.map(CoreEmitOptions::from).unwrap_or_default();
    emit(&r#type, payload, opts);
}

/// `useMessage(type, handler)` — register a handler invoked when the
/// active node receives a message of the given type. Handler signature:
/// `(msg: { from, type, payload, ttl }) => void`.
#[napi(js_name = "useMessage")]
pub fn js_use_message(env: Env, message_type: String, handler: JsFunction) -> Result<()> {
    let handler_ref: Rc<ManuallyDrop<Ref<()>>> =
        Rc::new(ManuallyDrop::new(env.create_reference(handler)?));
    core_use_message(message_type, move |msg: &IncomingMessage| {
        let env = current_env();
        if let Err(e) = invoke_message_handler(env, &handler_ref, msg) {
            // Don't crash the runtime on a single-handler failure —
            // surface as a warning. Stage-4 wires this to supervision.
            eprintln!("[graphreact-napi] message handler error: {e}");
        }
    });
    Ok(())
}

fn invoke_message_handler(env: Env, handler_ref: &Ref<()>, msg: &IncomingMessage) -> Result<()> {
    let handler: JsFunction = env.get_reference_value(handler_ref)?;
    let mut obj = env.create_object()?;
    obj.set_named_property("from", env.create_string(&msg.from)?)?;
    obj.set_named_property("type", env.create_string(&msg.r#type)?)?;
    obj.set_named_property("payload", env.to_js_value(&msg.payload)?)?;
    obj.set_named_property("ttl", env.create_uint32(msg.ttl)?)?;
    let _ = handler.call(None, &[obj])?;
    Ok(())
}

/// `useEffect(callback, deps)` — synchronous side-effect hook. The JS
/// callback is invoked every time `deps` change; if it returns a function,
/// that function runs as cleanup before the next invocation (and on
/// unmount). Async callbacks are routed through `useEffectAsync`.
#[napi(js_name = "useEffect")]
pub fn js_use_effect(env: Env, callback: JsFunction, deps: Vec<Value>) -> Result<()> {
    let callback_ref: Rc<ManuallyDrop<Ref<()>>> =
        Rc::new(ManuallyDrop::new(env.create_reference(callback)?));
    core_use_effect(
        move || -> Option<EffectCleanup> {
            let env = current_env();
            match invoke_effect_callback(env, &callback_ref) {
                Ok(cleanup) => cleanup,
                Err(e) => {
                    eprintln!("[graphreact-napi] effect callback error: {e}");
                    None
                }
            }
        },
        deps,
    );
    Ok(())
}

/// Invoke a JS effect callback. If it returns a JS function, capture that
/// function as the cleanup. All other return shapes (undefined, null,
/// values, Promises) yield no cleanup — Promises are handled by
/// `useEffectAsync`, not this path.
fn invoke_effect_callback(env: Env, callback_ref: &Ref<()>) -> Result<Option<EffectCleanup>> {
    let callback: JsFunction = env.get_reference_value(callback_ref)?;
    let result: JsUnknown = callback.call_without_args(None)?;

    if result.get_type()? == napi::ValueType::Function {
        // `JsUnknown::cast<T>` is `unsafe` in napi-rs because it skips the
        // runtime type check. We just verified the type above; the cast
        // is sound. Localize the unsafe scope to this one line so the
        // workspace `unsafe_code = deny` lint is contained.
        #[allow(unsafe_code)]
        let cleanup_fn: JsFunction = unsafe { result.cast::<JsFunction>() };
        let cleanup_ref: Rc<ManuallyDrop<Ref<()>>> =
            Rc::new(ManuallyDrop::new(env.create_reference(cleanup_fn)?));
        let cleanup: EffectCleanup = Box::new(move || {
            let env = current_env();
            if let Err(e) = invoke_cleanup(env, &cleanup_ref) {
                eprintln!("[graphreact-napi] effect cleanup error: {e}");
            }
        });
        return Ok(Some(cleanup));
    }
    Ok(None)
}

fn invoke_cleanup(env: Env, cleanup_ref: &Ref<()>) -> Result<()> {
    let cleanup: JsFunction = env.get_reference_value(cleanup_ref)?;
    // Cleanup functions normally return `undefined`; ignore the return
    // value rather than coercing it to an object (which would throw).
    let _: JsUnknown = cleanup.call_without_args(None)?;
    Ok(())
}

/// `useEffectAsync(callback, deps)` — async side-effect hook. The JS
/// callback should return a Promise (i.e. be an `async function`).
///
/// **M6.5 design:** the callback is invoked synchronously on the V8 main
/// thread during the active scheduler round; the returned Promise is
/// captured into the scheduler's pending queue. The TS-side
/// `processBatchAsync` / `startAsync` / `sendAsync` wrapper awaits all
/// captured Promises via `Promise.all`, then drives subsequent rounds.
///
/// Async cleanup: if the Promise resolves to a JS function, the wrapper
/// installs it back into the slot via `installAsyncCleanup(cleanupId,
/// fn)`. The captured cleanup runs on the next dep-change re-run or on
/// unmount, just like a sync cleanup.
///
/// Sync-mode degradation: invoking `useEffectAsync` from a purely-sync
/// scheduler entry (`start` / `processBatch` / `send` / `stop`) calls the
/// callback fire-and-forget — the Promise has no awaiter, so any errors
/// are unhandled rejections from V8's perspective. Use the async entries
/// (`startAsync` etc.) to actually await async effects.
#[napi(js_name = "useEffectAsync")]
pub fn js_use_effect_async(env: Env, callback: JsFunction, deps: Vec<Value>) -> Result<()> {
    let callback_ref: Rc<ManuallyDrop<Ref<()>>> =
        Rc::new(ManuallyDrop::new(env.create_reference(callback)?));
    let _ = env;

    core_use_effect(
        move || -> Option<EffectCleanup> {
            let env = current_env();
            let async_state = current_async_pending();

            // Allocate a fresh slot id for this run. Independent across
            // runs so a re-fire while an earlier Promise is still in
            // flight doesn't collide on cleanup routing.
            let slot_id = async_state
                .as_ref()
                .map(|s| s.next_slot_id())
                .unwrap_or(u32::MAX);

            // Invoke the JS callback synchronously and inspect the
            // returned value.
            let callback: JsFunction = match env.get_reference_value(&callback_ref) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[graphreact-napi] async effect callback ref invalid: {e}");
                    return None;
                }
            };
            let result: JsUnknown = match callback.call_without_args(None) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[graphreact-napi] async effect callback threw: {e}");
                    return None;
                }
            };

            let is_promise_like = matches!(result.get_type(), Ok(napi::ValueType::Object));
            if !is_promise_like {
                eprintln!("[graphreact-napi] useEffectAsync: callback did not return a Promise (got {:?})", result.get_type());
                return None;
            }

            // Capture the Promise into the active scheduler's pending
            // queue, OR fire-and-forget if no async round is active.
            if let Some(state) = async_state.as_ref() {
                let promise_ref = match env.create_reference(result) {
                    Ok(r) => Rc::new(ManuallyDrop::new(r)),
                    Err(e) => {
                        eprintln!("[graphreact-napi] failed to capture async effect Promise: {e}");
                        return None;
                    }
                };
                state.push_pending(slot_id, promise_ref);
            } else {
                eprintln!("[graphreact-napi] useEffectAsync called outside async scheduler entry; Promise discarded — use startAsync/processBatchAsync/sendAsync/stopAsync to await");
            }

            // Install a cleanup that consumes any async-resolved cleanup
            // function for this run's slot. The closure captures the
            // `Rc<AsyncPendingState>` directly so it can run regardless
            // of whether the next entry installed the TLS guard.
            let async_state_for_cleanup = async_state;
            let cleanup: EffectCleanup = Box::new(move || {
                let Some(state) = async_state_for_cleanup else {
                    return;
                };
                let Some(cleanup_ref) = state.take_cleanup(slot_id) else {
                    return;
                };
                let env = current_env();
                let cleanup_fn: JsFunction = match env.get_reference_value(&cleanup_ref) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("[graphreact-napi] async cleanup ref invalid: {e}");
                        return;
                    }
                };
                if let Err(e) = cleanup_fn.call_without_args(None) {
                    eprintln!("[graphreact-napi] async cleanup error: {e}");
                }
            });
            Some(cleanup)
        },
        deps,
    );
    Ok(())
}

// --- useMemo / useCallback ---

/// `useMemo(factory, deps)` — memoize a JS factory. The factory is called
/// only when `deps` change; the cached value is returned otherwise.
///
/// JS values cross the bridge as `serde_json::Value`, so anything
/// non-serializable (functions, classes) round-trips through `useCallback`
/// instead.
#[napi(js_name = "useMemo")]
pub fn js_use_memo(env: Env, factory: JsFunction, deps: Vec<Value>) -> Result<Value> {
    let factory_ref: Rc<ManuallyDrop<Ref<()>>> =
        Rc::new(ManuallyDrop::new(env.create_reference(factory)?));
    let value = core_use_memo(
        move || -> Value {
            let env = current_env();
            invoke_memo_factory(env, &factory_ref).unwrap_or(Value::Null)
        },
        deps,
    );
    Ok(value)
}

fn invoke_memo_factory(env: Env, factory_ref: &Ref<()>) -> Result<Value> {
    let factory: JsFunction = env.get_reference_value(factory_ref)?;
    let result: JsUnknown = factory.call_without_args(None)?;
    let value: Value = env.from_js_value(result)?;
    Ok(value)
}

/// `useCallback(callback, deps)` — memoize a JS function reference. Returns
/// the same JS function across evaluations until `deps` change. Useful for
/// preserving referential equality in nested handler bindings.
#[napi(js_name = "useCallback")]
pub fn js_use_callback(env: Env, callback: JsFunction, deps: Vec<Value>) -> Result<JsFunction> {
    // Create a Ref now — if `useMemo` discards this closure (deps stable),
    // the new Ref leaks; the cached one stays in the memo slot.
    let new_ref: Rc<ManuallyDrop<Ref<()>>> =
        Rc::new(ManuallyDrop::new(env.create_reference(callback)?));
    let cached_ref = core_use_memo(move || -> Rc<ManuallyDrop<Ref<()>>> { new_ref }, deps);
    env.get_reference_value(&cached_ref)
}

// --- useReducer ---

/// Reducer handle returned by [`js_use_reducer`]. JS exposes the current
/// state via the `state` getter and writes via `dispatch(action)`.
///
/// JS-side ergonomic wrapper (in the package's index.ts) can destructure
/// to `[state, dispatch]` if the React-style tuple is preferred.
#[napi]
pub struct Reducer {
    signal: CoreSignal<Value>,
    reducer_ref: Rc<ManuallyDrop<Ref<()>>>,
}

#[napi]
impl Reducer {
    /// Current reducer state.
    #[napi(getter)]
    pub fn state(&self) -> Value {
        self.signal.get()
    }

    /// Apply the reducer to `(state, action)` and store the result.
    #[napi]
    pub fn dispatch(&self, env: Env, action: Value) -> Result<()> {
        let reducer: JsFunction = env.get_reference_value(&self.reducer_ref)?;
        let current = self.signal.get();
        let args = [env.to_js_value(&current)?, env.to_js_value(&action)?];
        let result: JsUnknown = reducer.call(None, &args)?;
        let next: Value = env.from_js_value(result)?;
        self.signal.set(next);
        Ok(())
    }
}

/// `useReducer(reducer, initial)` — Redux-style state hook. Returns a
/// `Reducer` handle with `.state` (read) and `.dispatch(action)` (write).
#[napi(js_name = "useReducer")]
pub fn js_use_reducer(env: Env, reducer: JsFunction, initial: Value) -> Result<Reducer> {
    let reducer_ref: Rc<ManuallyDrop<Ref<()>>> =
        Rc::new(ManuallyDrop::new(env.create_reference(reducer)?));
    let signal = core_use_state::<Value>(initial);
    Ok(Reducer {
        signal,
        reducer_ref,
    })
}

// --- useNeighbors ---

/// `useNeighbors()` — returns the active node's neighbor IDs. JS-side can
/// further filter with `.filter(...)`; an in-Rust filter is intentionally
/// not exposed (would require a JS callback per neighbor).
#[napi(js_name = "useNeighbors")]
pub fn js_use_neighbors() -> Vec<String> {
    core_use_neighbors::<fn(&str) -> bool>(None)
}

// --- useTopology ---

/// Filter for `useTopology`. JS shape: `{ kinds?: string[], neighborsOnly?: bool }`.
#[napi(object)]
pub struct TopologyFilter {
    pub kinds: Option<Vec<String>>,
    #[napi(js_name = "neighborsOnly")]
    pub neighbors_only: Option<bool>,
}

/// `useTopology(handler, filter?)` — register a handler invoked when graph
/// topology changes (node/edge add/remove/update). Handler signature:
/// `(event: { kind, nodeId? | u, v, key? }) => void`.
#[napi(js_name = "useTopology")]
pub fn js_use_topology(
    env: Env,
    handler: JsFunction,
    filter: Option<TopologyFilter>,
) -> Result<()> {
    let handler_ref: Rc<ManuallyDrop<Ref<()>>> =
        Rc::new(ManuallyDrop::new(env.create_reference(handler)?));
    let core_filter = filter.map(|f| TopologyFilterOptions {
        kinds: f.kinds.map(|ks| {
            ks.iter()
                .filter_map(|k| match k.as_str() {
                    "node-added" | "NodeAdded" => Some(TopologyEventKind::NodeAdded),
                    "node-removed" | "NodeRemoved" => Some(TopologyEventKind::NodeRemoved),
                    "edge-added" | "EdgeAdded" => Some(TopologyEventKind::EdgeAdded),
                    "edge-removed" | "EdgeRemoved" => Some(TopologyEventKind::EdgeRemoved),
                    "edge-updated" | "EdgeUpdated" => Some(TopologyEventKind::EdgeUpdated),
                    _ => None,
                })
                .collect()
        }),
        neighbors_only: f.neighbors_only.unwrap_or(false),
    });
    core_use_topology(
        move |event: &TopologyEvent| {
            let env = current_env();
            if let Err(e) = invoke_topology_handler(env, &handler_ref, event) {
                eprintln!("[graphreact-napi] topology handler error: {e}");
            }
        },
        core_filter,
    );
    let _ = env;
    Ok(())
}

fn invoke_topology_handler(env: Env, handler_ref: &Ref<()>, event: &TopologyEvent) -> Result<()> {
    let handler: JsFunction = env.get_reference_value(handler_ref)?;
    let mut obj = env.create_object()?;
    match event {
        TopologyEvent::NodeAdded { node_id } => {
            obj.set_named_property("kind", env.create_string("node-added")?)?;
            obj.set_named_property("nodeId", env.create_string(node_id)?)?;
        }
        TopologyEvent::NodeRemoved { node_id } => {
            obj.set_named_property("kind", env.create_string("node-removed")?)?;
            obj.set_named_property("nodeId", env.create_string(node_id)?)?;
        }
        TopologyEvent::EdgeAdded { u, v, key } => {
            obj.set_named_property("kind", env.create_string("edge-added")?)?;
            obj.set_named_property("u", env.create_string(u)?)?;
            obj.set_named_property("v", env.create_string(v)?)?;
            obj.set_named_property("key", env.create_string(key)?)?;
        }
        TopologyEvent::EdgeRemoved { u, v, key } => {
            obj.set_named_property("kind", env.create_string("edge-removed")?)?;
            obj.set_named_property("u", env.create_string(u)?)?;
            obj.set_named_property("v", env.create_string(v)?)?;
            obj.set_named_property("key", env.create_string(key)?)?;
        }
        TopologyEvent::EdgeUpdated { u, v, key } => {
            obj.set_named_property("kind", env.create_string("edge-updated")?)?;
            obj.set_named_property("u", env.create_string(u)?)?;
            obj.set_named_property("v", env.create_string(v)?)?;
            obj.set_named_property("key", env.create_string(key)?)?;
        }
    }
    let _ = handler.call(None, &[obj])?;
    Ok(())
}

// --- Event sources (M6.6) ---
//
// JS-facing factories return [`JsEventSource`] handles wrapping a
// Rust-native `Arc<dyn EventSource>`. Built-ins (`timer`, `delay`) come
// from `graphreact-core`; HTTP and watch live in this crate's
// [`crate::sources`] module so their deps (`tokio::net`, `notify`)
// don't bloat core's compile graph for browser/WASM targets.
//
// `useSource(source, deps)` registers the source for the active node;
// the same source instance is reused across dep changes (its
// `stop()` + `start()` are called in cycle, so source impls must be
// safe to restart).

use graphreact_core::EventSource;

/// JS-side handle to a Rust-native event source. Owned by the JS
/// caller — passed back into `useSource` to register, kept alive as
/// long as any subscriber needs it.
///
/// Named `EventSourceHandle` rather than the more natural
/// `EventSource` to avoid the Node 18+ global `EventSource` (the
/// WHATWG/MDN web API) — napi-rs registers classes by name on the
/// global object, and a collision with the built-in caused
/// `Failed to get constructor of class 'EventSource'` failures during
/// hook execution.
#[napi]
pub struct EventSourceHandle {
    pub(crate) inner: std::sync::Arc<dyn EventSource>,
}

#[napi]
impl EventSourceHandle {
    /// Stable id assigned by the source factory — useful for
    /// debugging and de-duplication.
    #[napi(getter)]
    pub fn id(&self) -> String {
        self.inner.id().to_string()
    }
}

/// Repeating timer source — emits `tick` at `interval_ms` cadence.
#[napi(js_name = "timerSource")]
pub fn js_timer_source(interval_ms: u32) -> EventSourceHandle {
    EventSourceHandle {
        inner: core_timer_source(u64::from(interval_ms)),
    }
}

/// One-shot delay source — emits `timeout` once after `delay_ms`.
#[napi(js_name = "delaySource")]
pub fn js_delay_source(delay_ms: u32) -> EventSourceHandle {
    EventSourceHandle {
        inner: core_delay_source(u64::from(delay_ms)),
    }
}

/// HTTP server source — binds `127.0.0.1:port` and emits a `request`
/// message for each incoming HTTP/1 request. See
/// [`crate::sources::http_source`] for parser scope and limitations.
#[napi(js_name = "httpSource")]
pub fn js_http_source(port: u32) -> Result<EventSourceHandle> {
    let port = u16::try_from(port).map_err(|_| {
        napi::Error::from_reason(format!("httpSource: port {port} out of u16 range"))
    })?;
    Ok(EventSourceHandle {
        inner: crate::sources::http_source(port),
    })
}

/// Filesystem watch source — emits a `change` message for each event
/// under `path` (recursive). Backed by `notify::RecommendedWatcher`
/// (platform-native FSEvents/inotify/ReadDirectoryChangesW).
#[napi(js_name = "watchSource")]
pub fn js_watch_source(path: String) -> EventSourceHandle {
    EventSourceHandle {
        inner: crate::sources::watch_source(path),
    }
}

/// `useSource(source, deps)` — subscribe the active node to an event
/// source. The source's `start(push)` runs on mount and on each dep
/// change; `stop()` runs before each restart and on unmount. Source
/// instances are reused across dep changes, so source impls must be
/// safe to stop+start cycle.
///
/// **API divergence from TS reference:** TS takes a factory function
/// `() => EventSource` so each dep change reconstructs the source.
/// We take a source handle directly — for the typical case (`useSource(httpSource(8080), [])`)
/// this is identical; for fresh-per-deps use, callers should
/// reconstruct the handle outside the hook (or use a userland
/// `useMemo` wrapper). This avoids a JS factory round-trip on every
/// re-evaluation of components that don't actually rotate their
/// source config.
#[napi(js_name = "useSource")]
pub fn js_use_source(source: &EventSourceHandle, deps: Vec<Value>) {
    let inner = std::sync::Arc::clone(&source.inner);
    core_use_source(move || std::sync::Arc::clone(&inner), deps);
}

// --- M5 supervision hooks ---

/// Opaque monitor handle returned by [`js_use_monitor`].
///
/// Wraps the core `MonitorRef` (a `u64`) so JS can hold it and pass it
/// back to `useDemonitor` without inspecting the structure.
#[napi]
pub struct MonitorRef {
    inner: CoreMonitorRef,
}

#[napi]
impl MonitorRef {
    /// Internal numeric handle — exposed for debugging only.
    #[napi(getter)]
    pub fn id(&self) -> u32 {
        // MonitorRef is a u64; clamp for the JS-friendly u32 surface.
        u32::try_from(self.inner.0).unwrap_or(u32::MAX)
    }
}

/// `useMonitor(targetId)` — watch another node by ID. Returns a handle if
/// the target is registered; `null` otherwise. The active node receives a
/// `__DOWN__` system message when the target unmounts.
#[napi(js_name = "useMonitor")]
pub fn js_use_monitor(target_id: String) -> Option<MonitorRef> {
    core_use_monitor(&target_id).map(|inner| MonitorRef { inner })
}

/// `useDemonitor(ref)` — cancel a previously-registered monitor.
#[napi(js_name = "useDemonitor")]
pub fn js_use_demonitor(monitor: &MonitorRef) {
    core_use_demonitor(monitor.inner);
}

/// `useLink(targetId)` — establish a bidirectional link with another node.
/// Returns `true` if the link was (re-)established, `false` if the target
/// has unmounted between evaluations.
#[napi(js_name = "useLink")]
pub fn js_use_link(target_id: String) -> bool {
    core_use_link(&target_id)
}

/// `useUnlink(targetId)` — remove a bidirectional link. No-op if absent.
#[napi(js_name = "useUnlink")]
pub fn js_use_unlink(target_id: String) {
    core_use_unlink(&target_id);
}

/// `useGroupJoin(group)` — join a process group on every evaluation.
/// Idempotent. The scheduler auto-removes the node from all groups on
/// unmount.
#[napi(js_name = "useGroupJoin")]
pub fn js_use_group_join(group: String) {
    core_use_group_join(&group);
}

/// `useGroupLeave(group)` — leave a process group.
#[napi(js_name = "useGroupLeave")]
pub fn js_use_group_leave(group: String) {
    core_use_group_leave(&group);
}

// --- ctx mutation hook handles ---
//
// These mirror the React-style "capture in body, call in effects" pattern:
// hook accessors return a native class that wraps the Rust closure, so it
// can be invoked later when `HOOK_CTX` has already been cleared.

/// Handle returned by [`js_use_spawn_child`]. Has a `call(spec)` method
/// that pushes a SpawnChild mutation onto the scheduler's pending queue.
#[napi]
pub struct SpawnChildHandle {
    inner: std::rc::Rc<dyn Fn(ComposeChildSpec)>,
}

#[napi]
impl SpawnChildHandle {
    #[napi]
    pub fn call(&self, env: Env, spec: crate::scheduler::JsComposeChildSpec) -> Result<()> {
        let rust_spec = crate::scheduler::build_compose_spec(env, spec)?;
        (self.inner)(rust_spec);
        Ok(())
    }
}

/// `useSpawnChild()` — capture a closure that spawns a child node. Call
/// it (via `.call(spec)`) from inside the body, an effect, or a message
/// handler. Spawn is deferred to the next cascade round.
#[napi(js_name = "useSpawnChild")]
pub fn js_use_spawn_child() -> SpawnChildHandle {
    SpawnChildHandle {
        inner: core_use_spawn_child(),
    }
}

/// Handle returned by [`js_use_unmount_self`]. Has a `call()` method that
/// pushes an UnmountNode mutation for the active node.
#[napi]
pub struct UnmountSelfHandle {
    inner: std::rc::Rc<dyn Fn()>,
}

#[napi]
impl UnmountSelfHandle {
    #[napi]
    pub fn call(&self) {
        (self.inner)();
    }
}

/// `useUnmountSelf()` — capture a closure that unmounts the active node.
#[napi(js_name = "useUnmountSelf")]
pub fn js_use_unmount_self() -> UnmountSelfHandle {
    UnmountSelfHandle {
        inner: core_use_unmount_self(),
    }
}

/// Handle returned by [`js_use_compose`]. Has a `call(specs)` method that
/// declares the active node's composed-children list.
#[napi]
pub struct ComposeHandle {
    inner: std::rc::Rc<dyn Fn(Vec<ComposeChildSpec>)>,
}

#[napi]
impl ComposeHandle {
    #[napi]
    pub fn call(
        &self,
        env: Env,
        specs: Vec<crate::scheduler::JsComposeChildSpec>,
    ) -> Result<()> {
        let rust_specs: Vec<ComposeChildSpec> = specs
            .into_iter()
            .map(|s| crate::scheduler::build_compose_spec(env, s))
            .collect::<Result<Vec<_>>>()?;
        (self.inner)(rust_specs);
        Ok(())
    }
}

/// `useCompose()` — capture a closure that declares the active node's
/// composed children. Spawns any specs whose IDs aren't already
/// registered.
#[napi(js_name = "useCompose")]
pub fn js_use_compose() -> ComposeHandle {
    ComposeHandle {
        inner: core_use_compose(),
    }
}

/// Handle returned by [`js_use_accept_edge`]. Has a `call(targetId)`
/// method that pushes an `AcceptEdge` mutation onto the scheduler's
/// pending queue (the edge materialises before the next cascade round).
#[napi]
pub struct AcceptEdgeHandle {
    inner: std::rc::Rc<dyn Fn(&str)>,
}

#[napi]
impl AcceptEdgeHandle {
    /// Accept an edge from the active node to `targetId`. The edge is
    /// materialised on the next pending-mutation drain and both
    /// endpoints are marked dirty so their `useNeighbors` snapshots
    /// refresh.
    #[napi]
    pub fn call(&self, target_id: String) {
        (self.inner)(&target_id);
    }
}

/// `useAcceptEdge()` — Phase-9 edge negotiation hook. Returns a handle
/// whose `call(targetId)` synchronously enqueues an `AcceptEdge`
/// mutation. Mirrors the TS `ctx.acceptEdge(targetId)` shape.
#[napi(js_name = "useAcceptEdge")]
pub fn js_use_accept_edge() -> AcceptEdgeHandle {
    AcceptEdgeHandle {
        inner: core_use_accept_edge(),
    }
}
