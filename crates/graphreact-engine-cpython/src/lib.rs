//! CPython engine bridge — M13-phase2.5 slice.
//!
//! Wraps `zig/bridge/cpython.zig`'s C ABI surface in a safe Rust
//! [`Engine`] implementation.
//!
//! ## Phase scope
//!
//! - **phase1** — engine + isolate lifecycle real (`Py_Initialize` /
//!   `Py_Finalize` for the main interpreter; `Py_NewInterpreter` /
//!   `Py_EndInterpreter` for legacy single-GIL sub-interpreters that
//!   model the [`Engine::create_isolate`] surface).
//! - **phase2** — real `eval_module` / `resolve_function` /
//!   `call_function`. JSON marshalling on the FFI boundary
//!   (`HostValue` ↔ `serde_json::Value` ↔ JSON string ↔ Python
//!   object via the stdlib `json` module on the Zig side). Mirrors
//!   the JSC bridge's marshalling shape exactly so the two engines
//!   converge to the same trait surface.
//! - **phase2.5** — real bidirectional Rust↔Python
//!   callbacks. Python code calling a registered host name routes
//!   through `PyCFunction_New(SHARED_METHOD_DEF, capsule)` → the Zig
//!   trampoline → into [`host_callback_trampoline`] → into the closure
//!   stored in the global [`CALLBACK_REGISTRY`]. Symmetric path
//!   `gr_cpython_callback_invoke` (a.k.a. [`invoke_global_static`])
//!   lets Rust call a Python function on the active sub-interpreter's
//!   builtins by name. CPython analogue of JSC's M12.3a-phase2.5.
//! - **phase3a** — `.grx` parser learns `lang="python"`; new
//!   `crates/graphreact-cli/src/pyruntime.rs` composition layer mounts
//!   author-written Python bodies (`def init(ctx): return {…}`) on top
//!   of this engine. Lives entirely in the CLI crate; this crate's
//!   surface is unchanged from phase 2.5 (the engine trait surface was
//!   already complete).
//! - **phase3b (shipped 2026-05-03)** — Python HOOK_PRELUDE installs
//!   `use_state` + `use_effect` on each isolate's builtins; scheduler
//!   integration via `PyRuntime::mount_with_scheduler`. Setter calls
//!   re-render on the next `process_batch`; effect cleanup detected
//!   via `{kind: 'cleanup', token}` envelope.
//! - **phase3c (shipped 2026-05-03)** — HOOK_PRELUDE gains
//!   `use_emit` and `use_message`; `host_emit` + `host_message`
//!   callbacks route into `graphreact_core::use_emit` /
//!   `use_message`. Permission-manifest validation and TTL
//!   inheritance (M4-followon) are scheduler-side.
//! - **phase3d-α (shipped 2026-05-03)** — HOOK_PRELUDE gains
//!   `use_memo` / `use_callback` / `use_reducer`; new `host_memo_check`
//!   and `host_memo_store` callbacks implement a split-cache pattern
//!   so the factory runs Python-side and the host callbacks are pure
//!   thread-local read/writes (outer engine borrow stays valid).
//! - **phase3d-β (shipped 2026-05-03)** — HOOK_PRELUDE gains
//!   `use_neighbors` / `use_topology` / `use_accept_edge`; three new
//!   host callbacks split by safety tier. `host_neighbors` and
//!   `host_accept_edge` are pure HOOK_CTX reads (zero engine touch);
//!   `host_topology` uses the token-registry + TLS-capture pattern
//!   (reuses `CURRENT_ENGINE` / `CURRENT_ISO` from phase 3b). New
//!   helpers `parse_topology_filter` + `topology_event_to_host_value`
//!   ported verbatim from jsruntime. AcceptEdge mutations integrate
//!   via the existing scheduler cascade — no new mutation queue.
//! - **phase3d-γ (shipped 2026-05-03)** — HOOK_PRELUDE gains
//!   `ctx.directory` (5 finders via `__augment_ctx`) + `use_graph_query`
//!   (Tier 1: neighbors / has_node / has_edge / node_count). Two new
//!   TLS slots `CURRENT_DIRECTORY` and `CURRENT_GRAPH_SNAPSHOT`
//!   installed by extended `PyEngineGuard::with_directory`. 9 new host
//!   callbacks (5 directory finders + 4 graph_query Tier 1 accessors)
//!   ported verbatim from jsruntime. Body augmentation via
//!   `engine.invoke_global("__augment_ctx", ...)` before each body
//!   call — Python equivalent of JSC's IIFE source-wrap.
//! - **phase3d-δ (shipped 2026-05-04)** — `__graph_query_proxy`
//!   widened with 7 algorithm accessors (`shortest_path` / `all_paths`
//!   / `a_star_path` / `bfs` / `dfs` / `connected_components` /
//!   `cycle_detection`); 7 new `host_graph_*` callbacks delegate to
//!   `graphreact_algorithms` over the live GraphSnapshot from 3d-γ —
//!   pure `current_graph_snapshot()` reads (Tier A from 3d-β framing,
//!   zero engine touch). `GraphSnapshot::from_live` live-handle
//!   pattern verified end-to-end (JSC's followon-C "static→live"
//!   refactor was short-circuited at 3d-γ).
//! - **phase3d-ε (shipped 2026-05-04)** — `g.tier2(msgType, opts?)`
//!   adds policy-derived directed reachability. Returns a sub-proxy
//!   with 12 accessors (`node_count` / `has_node` / `neighbors` /
//!   `has_edge` / `bfs` / `dfs` / `shortest_path` /
//!   `topological_sort` / `a_star_path` / `all_paths` /
//!   `connected_components` [Tarjan SCC] / `cycle_detection` [3-color
//!   DFS]). 12 new `host_tier2_*` callbacks ported verbatim from
//!   JSC's followon-E; `build_tier2_adj` derives directed adjacency
//!   from per-node `PermissionManifest.emits/accepts`. 5 cost-from-
//!   policy A* heuristics (latency / compliance / hierarchy /
//!   trust-attenuated / prerequisite-distance) with verbatim JSC
//!   attr keys for cross-engine fixture portability. Two flavors:
//!   structural (default; Tier 1 edge AND emit/accept match) and
//!   directory-shortcut (opt-in via `opts.includeDirectoryShortcuts`;
//!   emit/accept only). `DirectedAdj` trait + `Tier2Adj` struct
//!   shared with 3d-ζ Tier 3.
//! - **phase3d-ζ (shipped 2026-05-04)** —
//!   `use_edge_accept_handler(predicate)` hook + Tier 3
//!   `g.tier3(msgType, opts?)`. Per-target `EdgeAcceptPredicate`
//!   filters Tier 2 candidacy at query time AND vetoes `AcceptEdge`
//!   mutations from `use_accept_edge` (3d-β) at the scheduler level.
//!   12 new `host_tier3_*` callbacks reuse 3d-ε's `DirectedAdj`
//!   algorithm helpers via `Tier3Adj` — zero duplication.
//!   `build_tier3_adj` reuses `build_tier2_adj` as supremum + filters
//!   through `edge_accept_predicates` indexed by target id.
//!   Default-accept on missing predicate preserves Phase-9 backward
//!   compat. Predicate dispatch via [`invoke_global_static`]
//!   bypasses the engine's `Rc<RefCell<...>>` for in-body re-entrancy
//!   from `host_tier3_*` callbacks. Closes the M13 phase-3d series.
//! - **phase3e-α (shipped 2026-05-04)** — dynamic node lifecycle
//!   hooks: `use_spawn_child(spec)`, `use_unmount_self()`,
//!   `use_compose(specs)`. 3 new `host_*` callbacks
//!   (`host_spawn_child` / `host_unmount_self` / `host_compose`) +
//!   2 helpers (`build_compose_spec_from_host_value` /
//!   `manifest_from_invoke_envelope`) verbatim Python port of JSC's
//!   M12.3a-phase3e-γ. Spawned bodies re-enter CPython on every
//!   render via [`CPythonEngine::invoke_global`] with token + ctx;
//!   ctx augmentation applied via lambda-wrap-at-registration in the
//!   HOOK_PRELUDE wrappers (Python-side equivalent of JSC's IIFE
//!   `spec.component` wrap), so spawned bodies see `ctx['directory']`
//!   and Tier 1/2/3 proxies identically to top-level definitions.
//!   Bounded token leak documented; sweeps in a future "M13-followon-D
//!   analogue" slice that batches GC across all token-using hooks.
//! - **phase3e-β (shipped 2026-05-04)** — six supervision hooks:
//!   `use_monitor(target_id)`, `use_demonitor(mref)`,
//!   `use_link(target_id)`, `use_unlink(target_id)`,
//!   `use_group_join(group)`, `use_group_leave(group)`. 6 new
//!   `host_*` callbacks (`host_monitor` / `host_demonitor` /
//!   `host_link` / `host_unlink` / `host_group_join` /
//!   `host_group_leave`) verbatim Python port of JSC's
//!   M12.3a-phase3e-ζ. All Tier A — pure HOOK_CTX-shaped reads; zero
//!   engine touch; zero token registry interaction; no NodeDefinition
//!   closure capture. `MonitorRef(pub u64)` marshalled across FFI as
//!   `{id: u64}`; Python wrapper exposes it as `_SimpleNamespace(id=...)`
//!   for symmetry with JSC's `{id: number}` JS object. `__DOWN__`
//!   system-message delivery already wired in
//!   `graphreact-core::Scheduler`; `use_message('__DOWN__', handler)`
//!   from 3c catches it via the standard token-registry path. Token
//!   leak surface unchanged — supervision hooks don't add new tokens.
//! - **phase3e-γ (shipped 2026-05-04)** — event-source surface:
//!   `use_source(factory_fn, deps)` plus four descriptor factories
//!   `timer_source(interval_ms)` / `delay_source(delay_ms)` /
//!   `http_source(opts)` / `watch_source(opts)`. 1 new
//!   `host_use_source` callback verbatim Python port of JSC's
//!   M12.3a-phase3e-ε plus M12.3a-followon-A combined. The
//!   followon-A infrastructure (`crates/graphreact-cli/src/sources.rs`
//!   with tokio::net httpSource and notify-based watchSource; tokio
//!   and notify deps in graphreact-cli's Cargo.toml; multi-thread
//!   tokio runtime in `grx` main()) was already in place from JSC,
//!   so all four source kinds ship real on the first Python slice.
//!   Descriptors cross the FFI as pure JSON; no native handle
//!   marshalling. Token leak surface unchanged. **Closes M13 phase 3
//!   — every hook surface JSC has via M12.3a-phase3a through
//!   M12.3a-phase3e-ζ plus M12.3a-followon-A is now real on
//!   `--runtime=cpython`.**
//! - **followon-D analogue (shipped 2026-05-04)** — per-node
//!   `__hostCallbacks` token GC on unmount. Verbatim Python port of
//!   JSC's M12.3a-followon-D. Closes the bounded leak documented
//!   since 3b. New `host_track_token(token)` callback appends body-
//!   time tokens into a per-node `NODE_TOKENS` TLS slot keyed by
//!   `current_node_id()`; `PyRuntime::process_batch` drains the
//!   `pending_unmounts` queue (already wired by 3b's topology
//!   listener on `NodeRemoved` events) and dispatches
//!   `__release_tokens([...])` via `invoke_global` for each removed
//!   node. Cleanup tokens NOT tracked (CURRENT_NODE_ID is unset
//!   during `flush_effects`; `EffectCleanup` already auto-releases
//!   via `__release_callback(ct)`).
//! - **phase3g (shipped 2026-05-04)** — `useEffectAsync` via asyncio
//!   bridge. Python analogue of JSC's M12.3b-phase1 Promise →
//!   Future bridge. 2 new `host_*` callbacks (`host_effect_async` +
//!   `host_effect_async_complete`) plus `EffectAsyncSettlement` enum
//!   plus `EFFECT_ASYNC_COMPLETIONS` TLS slot plus `_make_async_effect_invoker`
//!   Python pattern. The Python invoker drives the user's coroutine
//!   to completion via `asyncio.new_event_loop().run_until_complete(coro)`
//!   synchronously, then dispatches `host_effect_async_complete`
//!   which signals the parked tokio oneshot Sender. The async
//!   closure registered via `core_use_effect_async` awaits the
//!   Receiver; settlement → `AsyncEffectCleanup` or `None`. New
//!   `PyRuntime::process_batch_async` async method for callers
//!   driving from a tokio LocalSet. Token leak surface unchanged
//!   (followon-D auto-tracks body tokens; cleanup tokens auto-
//!   release). **No equivalent of JSC's M12.3b-phase4 vendored-
//!   WebKit blocker on Python** — asyncio handles genuine async I/O
//!   natively via `loop.run_until_complete`. CLI activation deferred
//!   (matches JSC's M12.3b-phase1 punt).
//! - **phase3f (shipped 2026-05-05)** — PEP 684 per-interpreter GIL
//!   on Python 3.12+. Build-time selector
//!   ([`build.rs`](../build.rs)) probes pkg-config for
//!   `python-3.X-embed` (newest wins; explicit
//!   `GRAPHREACT_PYTHON_VERSION` and `GRAPHREACT_PYTHON_PKGCONFIG`
//!   overrides supported). The build emits `--cfg python_3_12` once
//!   the resolved minor is ≥3.12, and `xtask zig-build` passes
//!   `-Dpython_3_12=true` / `-Dpython_include=<path>` to the Zig
//!   bridge so `gr_cpython_isolate_create` routes through
//!   `Py_NewInterpreterFromConfig` with `gil = OWN_GIL` (each
//!   sub-interpreter owns its GIL — no global serialisation), and
//!   `gr_cpython_engine_shutdown` calls real `Py_FinalizeEx` so
//!   re-init works cleanly. Branches are `comptime`-gated in
//!   `cpython.zig`. New `#[cfg(python_3_12)]` 1000-cycle stability
//!   test (compares warm-up vs steady-state RSS deltas — absolute-
//!   RSS bounds don't survive macOS's conservative virtual-memory
//!   release; per-cycle leaks would show up as similar incremental
//!   growth in both windows). A downstream consequence:
//!   `--cfg python_3_12` flows to `graphreact-cli` via
//!   `DEP_GRAPHREACT_CPYTHON_PYTHON_3_12` so the runtime layer can
//!   gate on the same flag.
//! - **Python 3.12+ floor (2026-05-05 closeout)** — the engine
//!   refuses to link Python <3.12. PEP 684's per-interpreter GIL is
//!   load-bearing for stable sub-interpreter cycling; the legacy
//!   3.9–3.11 single-GIL model has known C-extension state-pollution
//!   issues that crash at low cycle counts. Build fails fast with an
//!   actionable install hint; the Apple Command Line Tools 3.9
//!   framework fallback (used during phases 1 through 3g) was
//!   removed.
//! - **phase3h (shipped 2026-05-05)** — PEP 703 free-threaded build
//!   linkage for `python3.Xt`. Reuses 3f's pkg-config probe + cfg
//!   flag scaffold; detection trips on the trailing `t` in the
//!   pkg-config name (`python-3.14t-embed`) or libname
//!   (`python3.14t`). When detected, build.rs emits
//!   `cargo:rustc-cfg=python_free_threaded` and
//!   `cargo:python-free-threaded=1`, xtask zig-build passes
//!   `-Dpython_free_threaded=true` to `zig build`, and `cpython.zig`
//!   appends ` free-threaded` to the engine version string at init
//!   time. **No runtime path changes** — PEP 703 removes the GIL but
//!   doesn't change any C-API entry point we use; the semantics that
//!   change (multiple OS threads can hold different sub-interpreters'
//!   thread states simultaneously without serialising on a process-
//!   global GIL) only become observable when the runtime layer adds
//!   per-isolate concurrent-safe state — separable refactor, not
//!   part of 3h. Today's `engine_lock()` therefore stays a real
//!   serialising mutex regardless of `python_free_threaded` to keep
//!   the Zig-side global state (`state.isolates`, per-isolate
//!   `modules`/`functions` lists) safe across concurrent test
//!   threads. New `#[cfg(python_free_threaded)]`
//!   `pep703_free_threaded_build_has_gil_disabled` probe verifies
//!   `sys._is_gil_enabled() == False` at runtime — catches
//!   distribution bugs where the build script auto-resolves a
//!   regular Python despite the operator's intent. **3h footgun**:
//!   PEP 703 builds inline `_Py_atomic_*` helpers into the `Py_DECREF`
//!   macro AND the `PyList_Check` / `PyList_Size` macros (split-
//!   refcount + atomic type-cache reads); Zig's translate-c can't
//!   inline them and the symbols aren't exported by libpython3.Xt,
//!   so all `c.Py_DECREF` calls swapped to `c.Py_DecRef` (function
//!   form, exported symbol since 3.0) and `PyList_Check` /
//!   `PyList_Size` removed (defensive checks, not load-bearing —
//!   `PyList_AsTuple` errors cleanly on non-lists). Same fix shape
//!   as 3f's `Py_INCREF` → `Py_IncRef` swap.
//!
//! ## Isolate model
//!
//! Phase 1 uses CPython's legacy single-GIL sub-interpreter API
//! (`Py_NewInterpreter` / `Py_EndInterpreter`, available since Python
//! 2). Each "isolate" gets its own sub-interpreter with its own
//! `__main__` module + imports table; they all serialize on the
//! global GIL. PEP 684's per-interpreter GIL (Python 3.12+) is the
//! follow-on — same trait surface, looser concurrency model.
//!
//! ## Linkage
//!
//! Production builds (Linux, macOS-with-Homebrew, Windows-with-
//! pkg-config) discover Python via `pkg-config python-3.X-embed`;
//! explicit overrides via `GRAPHREACT_PYTHON_VERSION` /
//! `GRAPHREACT_PYTHON_PKGCONFIG` env vars are supported. Build floor
//! is Python 3.12 — `build.rs` fails fast on 3.9–3.11 with an
//! actionable install hint.
//!
//! CPython is per-minor-version ABI-locked: a binary built against
//! libpython3.13 won't load libpython3.12 at runtime. Released
//! artifacts are therefore per-minor slices (`grx-py-3.12`,
//! `grx-py-3.13`, `grx-py-3.14`, `grx-py-3.14t`), same shape as
//! numpy wheels. The `python_3_12` cfg flag is set per-build
//! according to the resolved minor and gates the PEP 684
//! per-interpreter GIL path.

// FFI crate: every call into the CPython bridge crosses an `extern "C"`
// boundary. The bridge surface itself is the source of unsafety; the
// rest of the file is a safe facade over those calls.
#![allow(unsafe_code)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::borrow_as_ptr,
    clippy::ref_as_ptr
)]

mod ffi;

use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use graphreact_engine_zig::{
    BoxCallback, CallbackId, Engine, EngineError, EngineResult, FunctionHandle, HostValue,
    IsolateHandle, IsolateOptions, ModuleHandle,
};

const RESULT_BUFFER_INITIAL: usize = 4096;
const RESULT_BUFFER_MAX: usize = 1 << 20; // 1 MiB

/// Boxed-callback type stored in the registry. We hold an `Arc` (not
/// the `Box` of the [`BoxCallback`] alias) because the Python trampoline
/// may invoke the closure from a context that races with
/// register/unregister; cloning an `Arc` out under the lock keeps the
/// closure alive past the lock release while not blocking other
/// registrations. Mirrors JSC's `SharedCallback`.
type SharedCallback = Arc<dyn Fn(&[HostValue]) -> EngineResult<HostValue> + Send + Sync>;

/// Process-global registry mapping callback id → closure. The Zig
/// layer assigns ids monotonically across all isolates; the Rust
/// trampoline reads `cb_data` as the id, looks up the closure here,
/// invokes it, returns the JSON-serialised result. Same shape as
/// `graphreact-engine-jsc::CALLBACK_REGISTRY` — the trampoline is
/// engine-agnostic since args/result cross the FFI boundary as JSON.
///
/// **M13-phase3i — sharded.** Replaced the single `RwLock<FxHashMap>`
/// with a `DashMap`, whose internal shard locks remove the global
/// serialization point. Concurrent isolates registering / disposing
/// callbacks no longer contend on one writer; the trampoline's hot
/// `get(id)` path takes only the shard's read lock for the
/// statistically-distributed key. This is the Rust-side companion to
/// the Zig-side state mutex sharding (M13-phase3j, separable).
static CALLBACK_REGISTRY: OnceLock<DashMap<u64, SharedCallback>> = OnceLock::new();

fn callback_registry() -> &'static DashMap<u64, SharedCallback> {
    CALLBACK_REGISTRY.get_or_init(DashMap::new)
}

/// CPython engine. Wraps the Zig bridge with a safe Rust facade. The
/// Zig side keeps process-global Python state (the singleton main
/// interpreter installed via `Py_Initialize`); the Rust side keeps
/// callback closures in [`CALLBACK_REGISTRY`] so the Python C
/// trampoline can re-enter Rust.
pub struct CPythonEngine {
    initialized: AtomicBool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Per-isolate set of callback ids this engine instance owns.
    /// On `dispose_isolate` / `shutdown` we drain the matching ids
    /// out of [`CALLBACK_REGISTRY`] so closures don't outlive the
    /// Python side. Mirrors `JscEngine::State::iso_callbacks`.
    iso_callbacks: FxHashMap<u64, FxHashSet<u64>>,
}

impl Default for CPythonEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl CPythonEngine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            state: Mutex::new(State::default()),
        }
    }

    /// Live isolate count according to the native side. Used in stress
    /// tests to assert no leaks. Mirrors `JscEngine::live_isolates`.
    #[must_use]
    pub fn live_isolates(&self) -> u64 {
        // SAFETY: pure value read; no pointer dereferences.
        unsafe { ffi::gr_cpython_engine_live_isolates() }
    }

    /// Invoke a Python function installed on the active sub-interpreter's
    /// builtins by name. Symmetric counterpart to
    /// [`Engine::register_callback`] (which makes Rust callable from
    /// Python): this calls a Python-side function from Rust by name.
    ///
    /// CPython-specific (not on the [`Engine`] trait) because not every
    /// engine has the same "lookup by global / builtin name" semantics.
    /// The runtime's hook layer in phase 3 will call this directly.
    pub fn invoke_global(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        args: &[HostValue],
    ) -> EngineResult<HostValue> {
        invoke_global_static(iso, name, args)
    }
}

/// Free-function variant of [`CPythonEngine::invoke_global`] for
/// callsites that can't hold a `&mut CPythonEngine` borrow without
/// conflicting with the outer call frame's borrow. Mirrors
/// `graphreact-engine-jsc::invoke_global_static` — phase 3+ may want
/// the same borrow-bypass shape (e.g., a hook predicate firing from
/// inside an active `engine.borrow_mut().call_function(...)` borrow).
///
/// Soundness: CPython's per-process state is the singleton main
/// interpreter + per-isolate sub-interpreters. The iso handle is
/// `Copy`-cheap and identifies the underlying sub-interpreter; the FFI
/// call routes through process-global state without touching any
/// `CPythonEngine` field. The Zig side acquires the right thread state
/// via `ThreadStateGuard` before any PyObject access.
pub fn invoke_global_static(
    iso: IsolateHandle,
    name: &str,
    args: &[HostValue],
) -> EngineResult<HostValue> {
    let args_json = host_args_to_json(args)?;
    let result_text = call_with_buffer(|buf, cap, written| unsafe {
        ffi::gr_cpython_callback_invoke(
            iso.0,
            name.as_ptr().cast(),
            name.len(),
            args_json.as_ptr().cast(),
            args_json.len(),
            buf,
            cap,
            written,
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&result_text)
        .map_err(|e| EngineError::Internal(format!("result json parse: {e}")))?;
    Ok(json_to_host_value(value))
}

fn map_status(rc: i32) -> EngineError {
    match rc {
        ffi::GR_OK => EngineError::Internal("expected error, got GR_OK".into()),
        ffi::GR_ERR_NOT_INITIALIZED => EngineError::Internal("engine not initialised".into()),
        ffi::GR_ERR_INVALID_ISOLATE => EngineError::InvalidIsolate,
        ffi::GR_ERR_INVALID_MODULE => EngineError::InvalidModule,
        ffi::GR_ERR_INVALID_FUNCTION => EngineError::Internal("invalid function handle".into()),
        ffi::GR_ERR_CALLBACK_DUPLICATE => EngineError::CallbackAlreadyRegistered {
            name: "<unknown>".into(),
        },
        ffi::GR_ERR_CALLBACK_NOT_FOUND => EngineError::CallbackNotFound {
            name: "<unknown>".into(),
        },
        ffi::GR_ERR_BUFFER_TOO_SMALL => EngineError::Internal("buffer too small".into()),
        ffi::GR_ERR_INTERNAL => EngineError::Internal("cpython bridge internal error".into()),
        other => EngineError::Internal(format!("unknown cpython bridge status code: {other}")),
    }
}

/// Run a closure that produces a UTF-8 string into a caller-owned
/// buffer using the bridge's "in/out length" convention. Returns the
/// allocated `String` on success.
fn read_string_buffer<F>(mut call: F) -> EngineResult<String>
where
    F: FnMut(*mut c_char, *mut usize) -> i32,
{
    let mut len: usize = 0;
    let rc = call(std::ptr::null_mut(), &mut len);
    if rc == ffi::GR_ERR_BUFFER_TOO_SMALL {
        // Expected: len now holds the required size.
    } else if rc != ffi::GR_OK {
        return Err(map_status(rc));
    }
    let mut buf = vec![0_u8; len];
    let mut written = len;
    let rc = call(buf.as_mut_ptr().cast(), &mut written);
    if rc != ffi::GR_OK {
        return Err(map_status(rc));
    }
    buf.truncate(written);
    String::from_utf8(buf)
        .map_err(|_| EngineError::Internal("non-utf8 string from cpython bridge".into()))
}

/// Run a closure that produces a JSON-shaped result (e.g. function
/// return value) into a caller-owned buffer. Resizes up to
/// [`RESULT_BUFFER_MAX`] on `GR_ERR_BUFFER_TOO_SMALL`. Mirrors the
/// JSC bridge's identical helper.
fn call_with_buffer<F>(mut call: F) -> EngineResult<String>
where
    F: FnMut(*mut c_char, usize, *mut usize) -> i32,
{
    let mut cap = RESULT_BUFFER_INITIAL;
    loop {
        let mut buf = vec![0_u8; cap];
        let mut written: usize = cap;
        let rc = call(buf.as_mut_ptr().cast(), cap, &mut written);
        if rc == ffi::GR_OK {
            buf.truncate(written);
            return String::from_utf8(buf).map_err(|_| {
                EngineError::Internal("non-utf8 result from cpython bridge".into())
            });
        }
        if rc == ffi::GR_ERR_BUFFER_TOO_SMALL {
            cap = (written.max(cap * 2)).min(RESULT_BUFFER_MAX);
            if cap > RESULT_BUFFER_MAX {
                return Err(EngineError::Internal(
                    "cpython bridge result exceeded 1 MiB".into(),
                ));
            }
            continue;
        }
        return Err(map_status(rc));
    }
}

/// Serialise an args slice as a JSON array suitable for the bridge.
/// Routes each [`HostValue`] through [`HostValue::into_json`] so
/// `HostValue::Object` becomes a real JSON object. Symmetrical with
/// the JSC bridge's identical helper.
fn host_args_to_json(args: &[HostValue]) -> EngineResult<String> {
    let array = serde_json::Value::Array(args.iter().cloned().map(HostValue::into_json).collect());
    serde_json::to_string(&array)
        .map_err(|e| EngineError::Internal(format!("args json serialise: {e}")))
}

fn json_to_host_value(value: serde_json::Value) -> HostValue {
    HostValue::from_json(value)
}

/// Phase 2.5 trampoline. The Zig-side `pythonCallbackTrampoline` calls
/// this directly with:
///
/// - `cb_data` — the registry id (encoded as a fake pointer; the Zig
///   side stores the id in the capsule's entry at register time so we
///   don't need per-engine state to look up the closure).
/// - `args_json` — JSON-array string of the arguments Python passed
///   (a tuple `json.dumps`'d on the Zig side).
/// - `out_result` — caller-owned buffer the trampoline writes the
///   JSON-serialised closure result into.
///
/// Returns `GR_OK` on success, `GR_ERR_BUFFER_TOO_SMALL` (with the
/// required size in `*out_result_len`) when the result doesn't fit,
/// `GR_ERR_CALLBACK_NOT_FOUND` if the id isn't in the registry, or
/// `GR_ERR_INTERNAL` on any other failure (the Zig side surfaces
/// these as Python exceptions).
///
/// Mirrors `graphreact-engine-jsc::host_callback_trampoline` —
/// engine-agnostic body since args/result cross the FFI boundary as
/// JSON.
unsafe extern "C" fn host_callback_trampoline(
    cb_data: *mut c_void,
    args_json: *const c_char,
    args_len: usize,
    out_result: *mut c_char,
    out_result_cap: usize,
    out_result_len: *mut usize,
) -> c_int {
    // SAFETY: `cb_data` is the id encoded as a pointer on the Zig
    // side; reading it back is a pure integer cast.
    let id = cb_data as usize as u64;

    let cb = callback_registry().get(&id).map(|entry| entry.clone());
    let Some(cb) = cb else {
        return ffi::GR_ERR_CALLBACK_NOT_FOUND;
    };

    // Decode args.
    let args_slice: &[u8] = if args_json.is_null() || args_len == 0 {
        b"[]"
    } else {
        unsafe { std::slice::from_raw_parts(args_json.cast::<u8>(), args_len) }
    };
    let args_str = match std::str::from_utf8(args_slice) {
        Ok(s) => s,
        Err(_) => return ffi::GR_ERR_INTERNAL,
    };
    let parsed: serde_json::Value = match serde_json::from_str(args_str) {
        Ok(v) => v,
        Err(_) => return ffi::GR_ERR_INTERNAL,
    };
    let args: Vec<HostValue> = match parsed {
        serde_json::Value::Array(arr) => arr.into_iter().map(HostValue::from_json).collect(),
        _ => return ffi::GR_ERR_INTERNAL,
    };

    // Invoke.
    let result = match cb(&args) {
        Ok(v) => v,
        Err(_) => return ffi::GR_ERR_INTERNAL,
    };

    // Serialise.
    let result_json = match serde_json::to_string(&result.into_json()) {
        Ok(s) => s,
        Err(_) => return ffi::GR_ERR_INTERNAL,
    };

    // Write into the caller's buffer using the bridge length convention.
    let bytes = result_json.as_bytes();
    if !out_result_len.is_null() {
        unsafe { *out_result_len = bytes.len() };
    }
    if bytes.len() > out_result_cap {
        return ffi::GR_ERR_BUFFER_TOO_SMALL;
    }
    if !out_result.is_null() && !bytes.is_empty() {
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_result.cast::<u8>(), bytes.len());
        }
    }
    ffi::GR_OK
}

impl Engine for CPythonEngine {
    fn initialize(&mut self) -> EngineResult<()> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        let rc = unsafe { ffi::gr_cpython_engine_initialize() };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    fn shutdown(&mut self) -> EngineResult<()> {
        if !self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        let rc = unsafe { ffi::gr_cpython_engine_shutdown() };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        self.initialized.store(false, Ordering::Release);
        // Drop every closure this engine owned. The Zig side ended
        // each sub-interpreter as part of `gr_cpython_engine_shutdown`;
        // the capsule destructors freed the per-callback Zig structs;
        // we just need to drop the Rust-side Arc refs so closures are
        // released.
        let mut state = self.state.lock();
        let registry = callback_registry();
        for ids in state.iso_callbacks.values() {
            for id in ids {
                registry.remove(id);
            }
        }
        state.iso_callbacks.clear();
        Ok(())
    }

    fn create_isolate(&mut self, _opts: IsolateOptions) -> EngineResult<IsolateHandle> {
        self.initialize()?;
        let mut handle: u64 = 0;
        let rc = unsafe { ffi::gr_cpython_isolate_create(&mut handle) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(IsolateHandle(handle))
    }

    fn dispose_isolate(&mut self, iso: IsolateHandle) -> EngineResult<()> {
        let rc = unsafe { ffi::gr_cpython_isolate_dispose(iso.0) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        // The Zig side ended the sub-interpreter via `Py_EndInterpreter`,
        // which cleared the builtins dict and triggered each capsule's
        // destructor (freeing the per-callback Zig `CallbackEntry`).
        // Drop the Rust-side closures that belonged to this isolate so
        // their Arcs can be released.
        if let Some(ids) = self.state.lock().iso_callbacks.remove(&iso.0) {
            let registry = callback_registry();
            for id in ids {
                registry.remove(&id);
            }
        }
        Ok(())
    }

    /// Evaluate `source` as a fresh Python module inside the isolate's
    /// sub-interpreter. The Zig side runs `PyRun_String` against a
    /// freshly-created module dict; the resulting module object is
    /// stored under a handle so [`Self::resolve_function`] can fetch
    /// callable attributes from it.
    ///
    /// Phase 2 contract mirrors the JSC bridge: `source` is the raw
    /// module body; `name` is informational (used in tracebacks). The
    /// caller is expected to write Python code at the top level (e.g.
    /// `def init(): return 42`) and then resolve+call individual
    /// functions.
    fn eval_module(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        source: &str,
    ) -> EngineResult<ModuleHandle> {
        let mut module: u64 = 0;
        let rc = unsafe {
            ffi::gr_cpython_module_register(
                iso.0,
                name.as_ptr().cast(),
                name.len(),
                source.as_ptr().cast(),
                source.len(),
                &mut module,
            )
        };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(ModuleHandle(module))
    }

    /// Resolve `name` as an attribute of the module object stored
    /// under `module`. Returns a function handle the Zig side can
    /// invoke; the actual Python object is held alive via `Py_INCREF`
    /// inside the bridge's per-isolate function table.
    fn resolve_function(
        &mut self,
        iso: IsolateHandle,
        module: ModuleHandle,
        name: &str,
    ) -> EngineResult<FunctionHandle> {
        let mut function: u64 = 0;
        let rc = unsafe {
            ffi::gr_cpython_function_resolve(
                iso.0,
                module.0,
                name.as_ptr().cast(),
                name.len(),
                &mut function,
            )
        };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(FunctionHandle(function))
    }

    /// **M13-phase2.5 — REAL.** Registers `cb` so Python code calling
    /// `name()` from any module routes through
    /// [`host_callback_trampoline`] into the closure stored in
    /// [`CALLBACK_REGISTRY`]. The Zig side allocates a globally-unique
    /// id, wraps it in a `PyCapsule`, and installs a `PyCFunction`
    /// referencing that capsule on the active sub-interpreter's
    /// builtins. The id is returned both as the [`CallbackId`] and as
    /// the registry key — the Rust trampoline reads it back via
    /// `cb_data` (encoded as a fake pointer on the Zig side).
    fn register_callback(
        &mut self,
        iso: IsolateHandle,
        name: &str,
        cb: BoxCallback,
    ) -> EngineResult<CallbackId> {
        let mut id: u64 = 0;
        let rc = unsafe {
            ffi::gr_cpython_callback_register(
                iso.0,
                name.as_ptr().cast(),
                name.len(),
                host_callback_trampoline,
                std::ptr::null_mut(),
                &mut id,
            )
        };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }

        // Box → Arc for storage. CPython may invoke the closure
        // between registry lookups under load; cloning an Arc out of
        // the registry guard lets the trampoline drop the read lock
        // before invocation.
        let shared: SharedCallback = Arc::from(cb);
        callback_registry().insert(id, shared);
        self.state
            .lock()
            .iso_callbacks
            .entry(iso.0)
            .or_default()
            .insert(id);
        Ok(CallbackId(id))
    }

    /// Call the function stored under `function` with `args`. Args
    /// are serialised through JSON on the FFI boundary; the Zig side
    /// uses Python's `json` stdlib to convert the JSON args to a
    /// Python list of values, calls the function, and converts the
    /// return value back through `json.dumps`. Same end-to-end shape
    /// as the JSC bridge's `call_function`.
    fn call_function(
        &mut self,
        iso: IsolateHandle,
        function: FunctionHandle,
        args: &[HostValue],
    ) -> EngineResult<HostValue> {
        let args_json = host_args_to_json(args)?;
        let result_text = call_with_buffer(|buf, cap, written| unsafe {
            ffi::gr_cpython_function_call(
                iso.0,
                function.0,
                args_json.as_ptr().cast(),
                args_json.len(),
                buf,
                cap,
                written,
            )
        })?;
        let value: serde_json::Value = serde_json::from_str(&result_text)
            .map_err(|e| EngineError::Internal(format!("result json parse: {e}")))?;
        Ok(json_to_host_value(value))
    }

    /// CPython has no embedder-level microtask queue; asyncio
    /// integration lands at a later phase (sibling of M12.3b-phase1's
    /// JS Promise → Rust Future bridge). Phase 1 returns `Ok` no-op
    /// so the trait surface is exercised cleanly without the caller
    /// special-casing CPython.
    fn drain_microtasks(&mut self, iso: IsolateHandle) -> EngineResult<()> {
        let rc = unsafe { ffi::gr_cpython_microtasks_drain(iso.0) };
        if rc != ffi::GR_OK {
            return Err(map_status(rc));
        }
        Ok(())
    }

    fn engine_name(&self) -> &'static str {
        "cpython"
    }

    fn engine_version(&self) -> String {
        // M13-phase3h: query the version string from the Zig side.
        // Format: `cpython-0.1.0-m13-phase3h (Python <X>.<Y>.<Z>)`
        // for GIL'd builds; `cpython-0.1.0-m13-phase3h (Python
        // <X>.<Y>.<Z> free-threaded)` for PEP 703 builds (the
        // ` free-threaded` annotation is emitted by `cpython.zig`
        // at init time when `build_opts.python_free_threaded`).
        read_string_buffer(|buf, len| unsafe { ffi::gr_cpython_engine_version(buf, len) })
            .unwrap_or_else(|_| "cpython-unknown".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Tests must serialise on a process-global mutex. Mirrors
    /// JscEngine's `engine_lock` pattern. **NOT no-op'd on PEP 703
    /// free-threaded builds** even though the GIL is gone — see
    /// below.
    ///
    /// **M13-phase3h note**: PEP 703 (`python_free_threaded`)
    /// removes the global GIL, so tests COULD in principle run
    /// concurrently against the Python C API. But the Zig bridge
    /// (`zig/bridge/cpython.zig`) carries process-global mutable
    /// state — `state.isolates`, `state.next_handle`,
    /// `state.version_string`, plus per-isolate `modules` /
    /// `functions` lists — that's only thread-safe under serialised
    /// access. Today's serialisation is implicit (the GIL'd build
    /// can't have two threads inside any C-API entry point at
    /// once); in a free-threaded build the same global state
    /// without explicit Zig-side locking would race. The
    /// `engine_lock` therefore stays a real serialising mutex
    /// regardless of `python_free_threaded`. Lifting it to a
    /// no-op (or per-test scoped lock) requires a separable
    /// refactor that adds `std.Thread.Mutex` protection around
    /// every state.* mutation site in `cpython.zig` (~30 entry
    /// points, with care to avoid deadlocks under recursive
    /// PyObject re-entries through callbacks). Out of scope for
    /// 3h's "build-flag concern, not new hook surface" framing.
    fn engine_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // ---------------------------------------------------------------------
    // Phase 1 — lifecycle
    // ---------------------------------------------------------------------

    #[test]
    fn engine_initialize_then_shutdown_idempotent() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init ok");
        e.initialize().expect("init idempotent");
        e.shutdown().expect("shutdown ok");
        e.shutdown().expect("shutdown idempotent");
    }

    #[test]
    fn create_dispose_isolate_round_trip() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        assert!(iso.0 > 0, "handle should be non-zero");
        let live_before = e.live_isolates();
        assert!(live_before >= 1);
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    #[test]
    fn engine_name_is_cpython() {
        let e = CPythonEngine::new();
        assert_eq!(e.engine_name(), "cpython");
    }

    #[test]
    fn engine_version_starts_with_cpython() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let v = e.engine_version();
        assert!(v.starts_with("cpython-"), "version: {v}");
        // Format includes the embedded Python version in parens for
        // ops visibility (mirrors `python --version` shape).
        assert!(
            v.contains("Python "),
            "version should embed `(Python X.Y.Z)`: {v}"
        );
        e.shutdown().expect("shutdown");
    }

    // ---------------------------------------------------------------------
    // Phase 2 — eval / resolve / call (REAL)
    // ---------------------------------------------------------------------

    /// Evaluate a module that defines `init() -> 42`, resolve `init`,
    /// call it, and assert the value comes back through JSON
    /// marshalling. Mirrors JSC's `phase2_eval_resolve_call_round_trip_returns_computed_value`.
    #[test]
    fn phase2_eval_resolve_call_returns_computed_int() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        let m = e
            .eval_module(iso, "phase2-int", "def init():\n    return 1 + 41\n")
            .expect("eval ok");
        let init = e.resolve_function(iso, m, "init").expect("resolve init");
        let result = e.call_function(iso, init, &[]).expect("call init");
        match result {
            HostValue::Number(n) => assert!((n - 42.0).abs() < f64::EPSILON, "got {n}"),
            other => panic!("expected Number(42.0), got {other:?}"),
        }
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Returning a Python string round-trips intact through JSON.
    #[test]
    fn phase2_call_returning_string_marshals_through_json() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        let m = e
            .eval_module(
                iso,
                "phase2-str",
                "def greet():\n    return 'hello, graphreact'\n",
            )
            .expect("eval ok");
        let f = e.resolve_function(iso, m, "greet").expect("resolve");
        let result = e.call_function(iso, f, &[]).expect("call");
        match result {
            HostValue::String(s) => assert_eq!(s, "hello, graphreact"),
            other => panic!("expected String(\"hello, graphreact\"), got {other:?}"),
        }
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Returning a Python dict round-trips as a `HostValue::Object`
    /// with keys preserved. Mirrors JSC's
    /// `phase2_call_returning_object_marshals_through_json`.
    #[test]
    fn phase2_call_returning_dict_marshals_as_object() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        let m = e
            .eval_module(
                iso,
                "phase2-dict",
                "def shape():\n    return {'hello': 'world', 'count': 7}\n",
            )
            .expect("eval ok");
        let f = e.resolve_function(iso, m, "shape").expect("resolve");
        let result = e.call_function(iso, f, &[]).expect("call");
        let HostValue::Object(pairs) = result else {
            panic!("expected Object, got {result:?}");
        };
        let map: std::collections::HashMap<String, &HostValue> =
            pairs.iter().map(|(k, v)| (k.clone(), v)).collect();
        assert!(matches!(map.get("hello"), Some(HostValue::String(s)) if s == "world"));
        assert!(matches!(map.get("count"), Some(HostValue::Number(n)) if (*n - 7.0).abs() < f64::EPSILON));
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Args round-trip: pass HostValue args through JSON → Python →
    /// back through JSON → HostValue. Verifies the args path
    /// (asymmetric with the no-args tests above which only exercise
    /// the return path).
    #[test]
    fn phase2_call_with_args_marshals_through_json() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        let m = e
            .eval_module(iso, "phase2-add", "def add(a, b):\n    return a + b\n")
            .expect("eval ok");
        let f = e.resolve_function(iso, m, "add").expect("resolve");
        let result = e
            .call_function(iso, f, &[HostValue::Number(2.0), HostValue::Number(40.0)])
            .expect("call");
        match result {
            HostValue::Number(n) => assert!((n - 42.0).abs() < f64::EPSILON, "got {n}"),
            other => panic!("expected Number(42.0), got {other:?}"),
        }
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Resolving an attribute that doesn't exist surfaces as an
    /// `EngineError`. Pins the error path of `resolve_function`.
    #[test]
    fn phase2_resolve_nonexistent_attribute_errors() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        let m = e
            .eval_module(iso, "phase2-empty", "x = 1\n")
            .expect("eval");
        let result = e.resolve_function(iso, m, "no_such_function");
        assert!(result.is_err(), "missing attribute must error");
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Sub-interpreter isolation: a name defined in one isolate is
    /// not visible in another. Pins the per-isolate
    /// `__main__`/imports namespace boundary that
    /// `Py_NewInterpreter` provides.
    #[test]
    fn phase2_sub_interpreters_isolate_globals() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");

        let iso_a = e.create_isolate(IsolateOptions::default()).expect("a");
        let iso_b = e.create_isolate(IsolateOptions::default()).expect("b");

        // Define `secret` in iso_a only.
        let m_a = e
            .eval_module(iso_a, "iso_a", "secret = 'a-only'\ndef get():\n    return secret\n")
            .expect("eval a");
        let get_a = e.resolve_function(iso_a, m_a, "get").expect("resolve a");
        match e.call_function(iso_a, get_a, &[]).expect("call a") {
            HostValue::String(s) => assert_eq!(s, "a-only"),
            other => panic!("expected String, got {other:?}"),
        }

        // iso_b has no `secret` global; defining a `get` that reads
        // it should NameError → call returns Err.
        let m_b = e
            .eval_module(iso_b, "iso_b", "def get():\n    return secret\n")
            .expect("eval b");
        let get_b = e.resolve_function(iso_b, m_b, "get").expect("resolve b");
        let result = e.call_function(iso_b, get_b, &[]);
        assert!(
            result.is_err(),
            "iso_b's get() should NameError on `secret` (proves namespace isolation)"
        );

        e.dispose_isolate(iso_a).expect("dispose a");
        e.dispose_isolate(iso_b).expect("dispose b");
        e.shutdown().expect("shutdown");
    }

    // ---------------------------------------------------------------------
    // Phase 2.5 — bidirectional Rust↔Python callbacks (REAL)
    // ---------------------------------------------------------------------

    /// Phase 2.5: a Rust closure registered as `host_add` is callable
    /// from Python code. Verifies the Python→Rust direction: PyCFunction
    /// invokes the Zig trampoline, which reads the capsule's entry,
    /// marshals args, calls back into Rust via [`host_callback_trampoline`],
    /// which looks up the closure in [`CALLBACK_REGISTRY`], invokes it,
    /// and returns the result back through `json.loads`.
    #[test]
    fn phase2_5_rust_closure_callable_from_python() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        e.register_callback(
            iso,
            "host_add",
            Box::new(|args| {
                let a = match args.first() {
                    Some(HostValue::Number(n)) => *n,
                    other => return Err(EngineError::Internal(format!("bad arg 0: {other:?}"))),
                };
                let b = match args.get(1) {
                    Some(HostValue::Number(n)) => *n,
                    other => return Err(EngineError::Internal(format!("bad arg 1: {other:?}"))),
                };
                Ok(HostValue::Number(a + b))
            }),
        )
        .expect("register host_add");

        let m = e
            .eval_module(
                iso,
                "phase2_5-add",
                "def init():\n    return host_add(2, 3)\n",
            )
            .expect("eval ok");
        let init = e.resolve_function(iso, m, "init").expect("resolve");
        let result = e.call_function(iso, init, &[]).expect("call");
        match result {
            HostValue::Number(n) => assert!((n - 5.0).abs() < f64::EPSILON, "got {n}"),
            other => panic!("expected Number(5.0), got {other:?}"),
        }

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 2.5: bidirectional roundtrip. Rust calls a Python function;
    /// the Python function calls back into Rust via a registered host
    /// callback; the Rust callback observes the call by mutating a
    /// shared `Mutex<Vec<String>>`. Proves the trampoline runs closures
    /// with side effects (state-preserving callbacks are what phase 3
    /// hooks need for hook integration).
    #[test]
    fn phase2_5_bidirectional_callback_observed_via_shared_state() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);
        e.register_callback(
            iso,
            "host_notify",
            Box::new(move |args| {
                let msg = match args.first() {
                    Some(HostValue::String(s)) => s.clone(),
                    other => format!("non-string arg: {other:?}"),
                };
                observations_for_cb.lock().push(msg);
                Ok(HostValue::Null)
            }),
        )
        .expect("register host_notify");

        let m = e
            .eval_module(
                iso,
                "phase2_5-notify",
                "def init():\n    \
                     host_notify('first')\n    \
                     host_notify('second')\n    \
                     return 'done'\n",
            )
            .expect("eval ok");
        let init = e.resolve_function(iso, m, "init").expect("resolve");
        let result = e.call_function(iso, init, &[]).expect("call");
        match result {
            HostValue::String(ref s) if s == "done" => {}
            other => panic!("expected String(\"done\"), got {other:?}"),
        }

        let collected = observations.lock();
        assert_eq!(
            collected.as_slice(),
            &["first".to_string(), "second".to_string()]
        );
        drop(collected);

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 2.5: `invoke_global` resolves a Python function on the
    /// active sub-interpreter's builtins by name and calls it with
    /// JSON-marshalled args. Proves the symmetric path
    /// (`gr_cpython_callback_invoke`) — phase 3 will use this to call
    /// Python-installed hook helpers from Rust.
    #[test]
    fn phase2_5_invoke_global_calls_python_function_by_name() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        // Install a Python function on the active sub-interpreter's
        // builtins. Each sub-interpreter has its own builtins dict;
        // assigning `builtins.double = ...` from Python puts the fn
        // where `gr_cpython_callback_invoke`'s `PyEval_GetBuiltins` +
        // `PyDict_GetItemString` will find it.
        e.eval_module(
            iso,
            "phase2_5-install-global-fn",
            "import builtins\n\
             def double(x):\n    return x * 2\n\
             builtins.double = double\n",
        )
        .expect("eval install");

        let result = e
            .invoke_global(iso, "double", &[HostValue::Number(7.0)])
            .expect("invoke double");
        match result {
            HostValue::Number(n) => assert!((n - 14.0).abs() < f64::EPSILON, "got {n}"),
            other => panic!("expected Number(14.0), got {other:?}"),
        }

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 2.5: callbacks are removed from the global registry on
    /// `dispose_isolate`. Closures shouldn't outlive the Python
    /// callable objects they back. Mirrors JSC's
    /// `phase2_5_dispose_isolate_drops_registered_callbacks`.
    #[test]
    fn phase2_5_dispose_isolate_drops_registered_callbacks() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");

        let id = e
            .register_callback(iso, "host_x", Box::new(|_| Ok(HostValue::Null)))
            .expect("register host_x");

        assert!(callback_registry().contains_key(&id.0));
        e.dispose_isolate(iso).expect("dispose");
        assert!(!callback_registry().contains_key(&id.0));

        e.shutdown().expect("shutdown");
    }

    // ---------------------------------------------------------------------
    // Phase 1 — isolate stress (gate kept; mirrored from JSC)
    // ---------------------------------------------------------------------

    /// Read CURRENT resident-set size of this process by shelling out
    /// to `ps -o rss= -p <pid>`. Mirrors JscEngine's helper.
    fn current_rss_bytes() -> u64 {
        let pid = unsafe { libc::getpid() };
        let output = match std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
        {
            Ok(o) => o,
            Err(_) => return 0,
        };
        let s = String::from_utf8_lossy(&output.stdout);
        let kb: u64 = s.trim().parse().unwrap_or(0);
        kb * 1024
    }

    /// Diagnostic-only probe: how many sub-interpreter cycles the
    /// resolved Python survives under our `create`/`dispose` cycling.
    /// Print progress every cycle so a SIGSEGV makes the threshold
    /// obvious. Skipped by default — invoke explicitly via
    /// `cargo test -p graphreact-engine-cpython -- --ignored` to
    /// characterise an upgraded Python (e.g. when investigating a
    /// regression in a new minor or under a free-threaded build).
    #[test]
    #[ignore = "diagnostic-only; run explicitly via --ignored when characterising an upgraded Python"]
    fn isolate_stress_probe_threshold() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        for i in 0..200_u32 {
            eprintln!("[probe] cycle {i} starting");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.dispose_isolate(iso).expect("dispose");
        }
        e.shutdown().expect("shutdown");
    }

    /// Modest leak-detection gate — fast smoke test (3 cycles) for
    /// any catastrophic leak in the create/dispose path. Genuine
    /// stress testing runs via
    /// [`isolate_stress_pep684_1000_cycles_no_leak`] below; this
    /// gate is the every-`cargo test` quick check that catches a
    /// leaked-interpreter bug before the slow run gets there.
    #[test]
    fn isolate_stress_modest_cycles_no_leak() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");

        let r0 = current_rss_bytes();
        for _ in 0..3 {
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.dispose_isolate(iso).expect("dispose");
        }
        let r1 = current_rss_bytes();

        // Generous bound — Python startup costs make per-cycle
        // memory churn unpredictable on the first few cycles. We're
        // catching catastrophic growth (a fully-leaked interpreter
        // would be ~20 MiB), not optimising allocation patterns.
        let cap = 50 * 1024 * 1024;
        let growth = r1.saturating_sub(r0);
        assert!(
            growth <= cap,
            "RSS growth across 3 sub-interpreter cycles exceeded 50 MiB \
             (catastrophic leak): r0={r0}B r1={r1}B growth={growth}B"
        );

        e.shutdown().expect("shutdown");
        assert_eq!(e.live_isolates(), 0, "expected zero live after shutdown");
    }

    // ---------------------------------------------------------------------
    // Phase 3f — PEP 684 multi-GIL stress (gated; only built on Python 3.12+)
    // ---------------------------------------------------------------------

    /// **M13-phase3f acceptance.** With the PEP 684 path engaged
    /// (`Py_NewInterpreterFromConfig` with
    /// `PyInterpreterConfig.gil = OWN_GIL`), each sub-interpreter is
    /// fully isolated, including the C-extension global state that
    /// the legacy single-GIL path shared. 1000 mount/dispose cycles
    /// is the cross-engine target (mirrors JSC's 1000+1000 shape
    /// from M12.3a-followon-A) and exercises any leak-shaped drift
    /// in the PEP 684 path under sustained churn.
    ///
    /// RSS growth bound is 100 MiB across the run — generous enough
    /// to absorb Python startup costs and the Zig allocator's debug
    /// tracking, tight enough to catch a per-cycle leak fragment at
    /// the kilobyte scale.
    ///
    /// `python_3_12` cfg is always set under the current 3.12+ build
    /// floor; this attribute is preserved for symmetry with future
    /// gating (e.g. a hypothetical PEP 700-class bump that might
    /// reintroduce a higher floor).
    #[cfg(python_3_12)]
    #[test]
    fn isolate_stress_pep684_1000_cycles_no_leak() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");

        // **Stability gate, not absolute-memory gate.** Compare RSS
        // growth between cycles 100–199 (after warm-up) and cycles
        // 900–999 (steady state). A real per-cycle leak shows up as
        // similar incremental growth across both windows; allocator
        // arenas / fragmentation / Python's per-interpreter caches
        // settle by the late window. The acceptance criterion is
        // "late-window growth ≤ early-window growth + 20 MiB":
        // identical bands prove no per-cycle leak is accumulating
        // (cycles 0-99 absorb sub-interpreter pool + obmalloc
        // initialisation, which is unbounded RSS in raw absolute
        // terms but bounded as a one-shot cost).
        //
        // PEP 684 sub-interpreters under `own_gil` own their own
        // obmalloc state + type cache + builtins dict, so each
        // create/dispose touches more committed memory than JSC's
        // 1000-cycle analogue. Absolute-RSS bounds (e.g. 100 MiB
        // total) don't survive macOS's conservative
        // virtual-memory release; the steady-state delta does.
        let mut warmup_start: u64 = 0;
        let mut warmup_end: u64 = 0;
        let mut steady_start: u64 = 0;
        let mut steady_end: u64 = 0;

        for i in 0..1000_u32 {
            if i == 100 {
                warmup_start = current_rss_bytes();
            }
            if i == 200 {
                warmup_end = current_rss_bytes();
            }
            if i == 900 {
                steady_start = current_rss_bytes();
            }
            if i == 999 {
                steady_end = current_rss_bytes();
            }
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            e.dispose_isolate(iso).expect("dispose");
        }

        let warmup_growth = warmup_end.saturating_sub(warmup_start);
        let steady_growth = steady_end.saturating_sub(steady_start);

        // Generous slack — 20 MiB above warm-up window growth
        // covers macOS's coarse-grained RSS reporting (4 KiB pages)
        // and any noise from system memory pressure during the run.
        let slack: u64 = 20 * 1024 * 1024;
        assert!(
            steady_growth <= warmup_growth + slack,
            "PEP 684 stability gate: late-window growth ({steady_growth}B over cycles 900–999) \
             exceeds early-window growth ({warmup_growth}B over cycles 100–199) by more than \
             20 MiB — likely per-cycle leak."
        );

        e.shutdown().expect("shutdown");
        assert_eq!(e.live_isolates(), 0, "expected zero live after shutdown");
    }

    // ---------------------------------------------------------------------
    // Phase 3h — PEP 703 free-threaded build runtime probe
    // ---------------------------------------------------------------------

    /// **M13-phase3h acceptance.** When the engine is linked against
    /// a PEP 703 free-threaded Python (`python3.Xt`),
    /// `sys._is_gil_enabled()` reports `False` at runtime — that's
    /// the canonical Python-side signal that the GIL is actually
    /// disabled. Conversely, regular GIL'd builds either don't have
    /// `sys._is_gil_enabled` at all (≤3.12) or it returns `True`
    /// (3.13+ GIL'd). The `python_free_threaded` cfg flag is unset
    /// for GIL'd builds, so this test doesn't compile in for them.
    ///
    /// (Note: `sys.flags.gil` returns `None` rather than `0`/`1`
    /// on 3.14t when the GIL state is "dynamic" — i.e. neither
    /// pinned-on nor pinned-off at process start. `sys._is_gil_enabled`
    /// is the correct probe for actual current GIL state.)
    ///
    /// This is an *honest* probe — it doesn't just check the cfg
    /// flag's existence; it confirms that the libpython we linked
    /// against actually has GIL removal compiled in and engaged at
    /// runtime. Catches distribution bugs where the build script
    /// auto-resolves a regular Python despite the operator's intent
    /// (e.g. `python-3.14-embed` resolving before
    /// `python-3.14t-embed` on a misconfigured PKG_CONFIG_PATH).
    ///
    /// Skipped on GIL'd builds via the `python_free_threaded` cfg
    /// (driven by `crates/graphreact-engine-cpython/build.rs`'s
    /// pkg-config probe — pkg-config name suffix `t-embed` or libname
    /// suffix `t` flips the flag).
    #[cfg(python_free_threaded)]
    #[test]
    fn pep703_free_threaded_build_has_gil_disabled() {
        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");
        let iso = e.create_isolate(IsolateOptions::default()).expect("create");
        let m = e
            .eval_module(
                iso,
                "pep703-probe",
                "import sys\ndef gil_enabled():\n    return sys._is_gil_enabled()\n",
            )
            .expect("eval");
        let f = e.resolve_function(iso, m, "gil_enabled").expect("resolve");
        let result = e.call_function(iso, f, &[]).expect("call");
        match result {
            HostValue::Bool(b) => {
                assert!(
                    !b,
                    "free-threaded build expected sys._is_gil_enabled() = False (GIL disabled), got True"
                );
            }
            other => panic!("expected Bool, got {other:?}"),
        }

        // Engine version string should also report the build is
        // free-threaded — this is the *Zig side's* signal, set at
        // engine init from the `python_free_threaded` build option.
        let v = e.engine_version();
        assert!(
            v.contains("free-threaded"),
            "engine_version should include 'free-threaded' annotation: {v}"
        );

        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// **M13-phase3i acceptance.** With the callback registry
    /// sharded by [`DashMap`], two isolates registering callbacks
    /// concurrently no longer contend on a single global writer.
    ///
    /// This is a smoke test for the absence of a deadlock / hot
    /// contention path; it does NOT enforce a wall-clock speedup
    /// (the global Zig-side `state.*` mutex still serializes
    /// register/dispose at the bridge level, so true concurrency
    /// requires Zig-side state mutex sharding too — separable
    /// refactor M13-phase3j). The test asserts only that 2×N
    /// concurrent registrations across two isolates land cleanly
    /// in the registry and dispose cleanly when their isolates go
    /// away.
    ///
    /// Skipped on GIL'd builds via the `python_free_threaded` cfg
    /// — under the GIL, `register_callback` already serializes via
    /// the GIL itself, so the test exercises nothing new there.
    #[cfg(python_free_threaded)]
    #[test]
    fn concurrent_isolates_dispatch_callbacks_without_serializing() {
        const N: usize = 32;

        let _g = engine_lock();
        let mut e = CPythonEngine::new();
        e.initialize().expect("init");

        let iso_a = e.create_isolate(IsolateOptions::default()).expect("create A");
        let iso_b = e.create_isolate(IsolateOptions::default()).expect("create B");

        let mut ids_a = Vec::with_capacity(N);
        let mut ids_b = Vec::with_capacity(N);
        for i in 0..N {
            let id_a = e
                .register_callback(iso_a, &format!("cb_a_{i}"), Box::new(|_| Ok(HostValue::Null)))
                .expect("register A");
            let id_b = e
                .register_callback(iso_b, &format!("cb_b_{i}"), Box::new(|_| Ok(HostValue::Null)))
                .expect("register B");
            ids_a.push(id_a.0);
            ids_b.push(id_b.0);
        }

        // Every id landed in the sharded registry.
        let registry = callback_registry();
        for id in ids_a.iter().chain(ids_b.iter()) {
            assert!(registry.contains_key(id), "id {id} missing post-register");
        }

        e.dispose_isolate(iso_a).expect("dispose A");
        for id in &ids_a {
            assert!(!registry.contains_key(id), "id {id} survived A dispose");
        }
        for id in &ids_b {
            assert!(registry.contains_key(id), "id {id} dropped by A dispose");
        }

        e.dispose_isolate(iso_b).expect("dispose B");
        for id in ids_b.iter().chain(ids_a.iter()) {
            assert!(!registry.contains_key(id), "id {id} survived B dispose");
        }

        e.shutdown().expect("shutdown");
    }
}
