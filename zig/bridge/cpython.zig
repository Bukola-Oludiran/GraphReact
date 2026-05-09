// cpython.zig — CPython engine bridge, M13-phase1+2.
//
// Implements the C ABI surface declared by
// `crates/graphreact-engine-cpython/src/ffi.rs` against CPython's
// public C API (`Python.h`) on macOS. Symbols are prefixed
// `gr_cpython_*` to avoid collision with the `gr_*` (echo) and
// `gr_jsc_*` (jsc) bridges; all three can link into the same binary
// without conflict.
//
// **Phase scope:**
//
// - **Phase 1 (lifecycle)** — `Py_Initialize` / `Py_Finalize` for
//   the singleton main interpreter; `Py_NewInterpreter` /
//   `Py_EndInterpreter` for legacy single-GIL sub-interpreters that
//   model the [`Engine::create_isolate`] surface. Each isolate gets
//   its own `__main__` module + imports table; they all serialise on
//   the global GIL.
//
// - **Phase 2 (eval/resolve/call)** — real `PyRun_String` for
//   module evaluation against a fresh module dict; real
//   `PyObject_GetAttrString` for resolving callable attributes; real
//   `PyObject_Call` for invocation. JSON marshalling on the FFI
//   boundary uses Python's stdlib `json` module (`json.loads` on
//   args, `json.dumps` on returns) — keeps the marshalling code
//   small and lets HostValue ↔ JSON ↔ PyObject converge through one
//   well-tested intermediate. Phase 3+ may swap to direct C-API
//   marshalling if profiling demands.
//
// - **Phase 2.5** — real bidirectional Rust↔Python callbacks via
//   `PyCFunction_New` + `PyCapsule_New`. Each registered host
//   callback becomes a `PyCFunction` whose `m_self` is a
//   `PyCapsule` carrying a heap-allocated `CallbackEntry`. The
//   shared trampoline `pythonCallbackTrampoline` reads the entry,
//   marshals args via stdlib `json`, calls into Rust, and parses the
//   JSON result back. Symmetric path `gr_cpython_callback_invoke`
//   looks up a Python callable on the active sub-interpreter's
//   builtins by name and calls it via `PyObject_Call`. CPython
//   analogue of JSC's M12.3a-phase2.5 (`JSClassDefinition` + private
//   slot).
//
// - **Phase 3a / 3b / 3c / 3d-α / 3d-β / 3d-γ (shipped 2026-05-03)** —
//   Python parser gained `lang="python"` (3a); `PyRuntime`
//   composition layer at `crates/graphreact-cli/src/pyruntime.rs`
//   mounts bodies; HOOK_PRELUDE installs `use_state` + `use_effect`
//   (3b), `use_emit` + `use_message` (3c), `use_memo` + `use_callback`
//   + `use_reducer` (3d-α), `use_neighbors` + `use_topology` +
//   `use_accept_edge` (3d-β), `ctx.directory` (5 finders) +
//   `use_graph_query` (Tier 1) + `__augment_ctx` (3d-γ) on each
//   isolate's builtins. 3d-γ adds `host_directory_*` (5 finders) +
//   `host_graph_*` (4 Tier 1 accessors) host callbacks reading from
//   `CURRENT_DIRECTORY` + `CURRENT_GRAPH_SNAPSHOT` TLS slots installed
//   by `PyEngineGuard::with_directory`. Bodies see ctx.directory via
//   Rust-side `invoke_global("__augment_ctx", ...)` before each
//   body call (Python equivalent of JSC's IIFE source-wrap).
//
// - **Phase 3d-δ (shipped 2026-05-04)** — `__graph_query_proxy`
//   widened with 7 algorithm accessors (shortest_path / all_paths /
//   a_star_path / bfs / dfs / connected_components /
//   cycle_detection); 7 new host_graph_* callbacks delegate to
//   `graphreact_algorithms`. GraphSnapshot live-handle pattern from
//   3d-γ verified under post-mount mutation.
//
// - **Phase 3d-ε (shipped 2026-05-04)** — Tier 2 policy-derived
//   directed reachability via `g.tier2(msgType, opts?)`. Returns a
//   sub-proxy with 12 accessors and 5 cost-from-policy A* heuristics.
//   DirectedAdj trait + Tier2Adj struct shared with 3d-ζ Tier 3.
//
// - **Phase 3d-ζ (shipped 2026-05-04)** — `use_edge_accept_handler`
//   hook + Tier 3 `g.tier3(msgType, opts?)`. Per-target
//   EdgeAcceptPredicate filters Tier 2 candidacy at query time AND
//   vetoes AcceptEdge mutations from `use_accept_edge` (3d-β) at the
//   scheduler level. 12 new host_tier3_* callbacks reuse 3d-ε's
//   DirectedAdj algorithm helpers via Tier3Adj. Predicate dispatch
//   uses `invoke_global_static` (free FFI helper from
//   graphreact-engine-cpython) for the in-body re-entrancy path that
//   would otherwise double-borrow the engine's RefCell. Closes the
//   M13 phase-3d series.
//
// - **Phase 3e-α (shipped 2026-05-04)** — dynamic node lifecycle:
//   `use_spawn_child(spec)`, `use_unmount_self()`,
//   `use_compose(specs)`. 3 new host callbacks (host_spawn_child /
//   host_unmount_self / host_compose) + 2 helpers
//   (build_compose_spec_from_host_value / manifest_from_invoke_envelope)
//   verbatim Python port of JSC's M12.3a-phase3e-γ. Spawned bodies
//   re-enter CPython on every render via
//   `engine.invoke_global("__invoke_callback", [token, [ctx]])`;
//   ctx augmentation applied via lambda-wrap-at-registration in the
//   HOOK_PRELUDE wrappers (Python-side equivalent of JSC's IIFE
//   spec.component wrap), so spawned bodies see ctx['directory']
//   and Tier 1/2/3 proxies identically to top-level definitions.
//
// - **Phase 3e-β (shipped 2026-05-04)** — six supervision hooks:
//   `use_monitor(target_id)`, `use_demonitor(mref)`,
//   `use_link(target_id)`, `use_unlink(target_id)`,
//   `use_group_join(group)`, `use_group_leave(group)`. 6 new host
//   callbacks (host_monitor / host_demonitor / host_link /
//   host_unlink / host_group_join / host_group_leave) verbatim
//   Python port of JSC's M12.3a-phase3e-ζ. All Tier A — pure
//   HOOK_CTX-shaped reads; zero engine touch; zero token registry
//   interaction; no NodeDefinition closure capture. `MonitorRef`
//   marshalled as `{id: u64}`; Python wrapper exposes it as
//   `_SimpleNamespace(id=...)`. `__DOWN__` system-message delivery
//   already wired in graphreact-core's Scheduler; use_message from
//   3c catches it.
//
// - **Phase 3e-γ (shipped 2026-05-04)** — event-source surface:
//   `use_source(factory_fn, deps)` plus four descriptor factories
//   `timer_source(interval_ms)` / `delay_source(delay_ms)` /
//   `http_source(opts)` / `watch_source(opts)`. 1 new host callback
//   (host_use_source) verbatim Python port of JSC's
//   M12.3a-phase3e-ε + M12.3a-followon-A combined. Followon-A
//   infrastructure (`crates/graphreact-cli/src/sources.rs` with
//   tokio::net httpSource + notify-based watchSource; tokio + notify
//   deps in graphreact-cli's Cargo.toml; multi-thread tokio runtime
//   in `grx` main()) was already in place from JSC, so all four
//   source kinds ship real on the first Python slice. Descriptors
//   cross FFI as pure JSON; no native handle marshalling. Token leak
//   surface unchanged. **Closes M13 phase 3 — every hook surface
//   JSC has via M12.3a-phase3a through M12.3a-phase3e-ζ +
//   M12.3a-followon-A is now real on Python.**
//
// - **M13-followon-D analogue (shipped 2026-05-04)** — per-node
//   `__hostCallbacks` token GC on unmount. Verbatim Python port of
//   JSC's M12.3a-followon-D. Body-time tokens (use_effect /
//   use_message / use_callback / use_topology / use_edge_accept_handler
//   / use_spawn_child / use_compose) accumulated for the lifetime of
//   the isolate; with this slice they're swept on each node's
//   unmount. New `host_track_token(token)` callback appends tokens
//   into per-node `NODE_TOKENS` TLS keyed by `current_node_id()`;
//   PyRuntime::process_batch drains `pending_unmounts` (already
//   wired by 3b's topology listener) and dispatches
//   `__release_tokens([...])` via `invoke_global`. Cleanup tokens
//   not tracked (CURRENT_NODE_ID unset during flush_effects;
//   EffectCleanup already auto-releases). Closes the bounded leak
//   documented since 3b.
//
// - **Phase 3g (shipped 2026-05-04)** — `useEffectAsync` via asyncio
//   bridge. Python analogue of JSC's M12.3b-phase1 Promise → Future
//   bridge. 2 new host callbacks (host_effect_async +
//   host_effect_async_complete) plus `EffectAsyncSettlement` enum +
//   `EFFECT_ASYNC_COMPLETIONS` TLS slot + Python invoker pattern.
//   The Python invoker drives the user's coroutine to completion
//   via `asyncio.new_event_loop().run_until_complete(coro)`
//   synchronously, then dispatches `host_effect_async_complete`
//   which signals the parked tokio oneshot Sender. New
//   `PyRuntime::process_batch_async` async method for callers
//   driving from a tokio LocalSet. Token leak surface unchanged
//   (followon-D auto-tracks body tokens; cleanup tokens auto-
//   release). **No equivalent of JSC's M12.3b-phase4 vendored-WebKit
//   blocker on Python** — asyncio handles genuine async I/O
//   natively via `loop.run_until_complete`.
//
// - **Phase 3f (shipped 2026-05-05)** — PEP 684 per-interpreter GIL
//   on Python 3.12+. Build-time `python_3_12` option (driven from
//   xtask zig-build's pkg-config probe) flips
//   `gr_cpython_isolate_create` from `Py_NewInterpreter` (legacy
//   single-GIL) to `Py_NewInterpreterFromConfig` with `gil =
//   PyInterpreterConfig_OWN_GIL` so each sub-interpreter gets its
//   own GIL — concurrent isolates on different OS threads no longer
//   serialise on a process-global GIL. Same option lifts the
//   "shutdown leaves main interpreter alive" workaround:
//   `gr_cpython_engine_shutdown` calls real `Py_FinalizeEx` on 3.12+
//   so subsequent `gr_cpython_engine_initialize` calls re-bring-up
//   the main interpreter cleanly. Branches are `comptime`-gated so
//   the 3.9 header set (which lacks `PyInterpreterConfig` and
//   `Py_NewInterpreterFromConfig`) compiles unchanged when
//   `python_3_12 = false`.
//
// - **Phase 3h (shipped 2026-05-05)** — PEP 703 free-threaded build
//   linkage (`python3.Xt`). Build-time `python_free_threaded` option
//   (driven from xtask zig-build's pkg-config probe; trips when the
//   resolved pkg-config name has the `t-embed` suffix or the libname
//   ends in `t`) annotates the engine version string and surfaces a
//   `comptime` flag for future runtime branches. **No runtime path
//   changes in this slice.** PEP 703 removes the GIL but doesn't
//   change any C-API entry point we use — `Py_InitializeEx`,
//   `PyThreadState_Swap`, `Py_NewInterpreterFromConfig`, `PyRun_String`,
//   `PyObject_*` all work uniformly across GIL'd and free-threaded
//   builds. The semantics that change (multiple OS threads can hold
//   different sub-interpreters' thread states simultaneously without
//   GIL serialisation) only become visible to the runtime layer when
//   that layer adds a per-isolate concurrent-safe Zig-side mutex
//   strategy — separable refactor, not part of 3h. Today's `state.*`
//   global mutable bookkeeping is implicitly serialised through
//   Rust-side `engine_lock()` whether the GIL is enabled or not. The
//   3h cfg flag flows to the Rust runtime layer via
//   `cargo:python-free-threaded=1` → `DEP_GRAPHREACT_CPYTHON_PYTHON_FREE_THREADED`
//   → `cargo:rustc-cfg=python_free_threaded` so future code paths
//   that DO want per-isolate parallelism can branch on it without
//   another build-system pass.
//
// **GIL discipline:** every entry point that touches Python state
// swaps the active thread state to the target sub-interpreter's
// thread state via `PyThreadState_Swap`, does its work, then swaps
// back. The main interpreter's thread state is the engine's
// "between-isolates" resting state. The GIL is held for the
// duration of every entry point (CPython's C API requires this);
// concurrent calls from different OS threads will serialise on the
// GIL, which is correct for the legacy sub-interpreter model.
//
// **drainMicrotasks** is a no-op. CPython has no embedder-level
// microtask queue at the C-API level; asyncio integration is
// userspace machinery (event-loop policy, run_until_complete, etc.).
// Phase 1 returns `GR_OK` so the Engine trait surface works
// uniformly across engines without callers special-casing CPython.

const std = @import("std");
const build_opts = @import("build_opts");

const c = @cImport({
    @cInclude("Python.h");
});

// Status codes — must match `crates/graphreact-engine-cpython/src/ffi.rs`.
pub const GR_OK: i32 = 0;
pub const GR_ERR_NOT_INITIALIZED: i32 = -1;
pub const GR_ERR_INVALID_ISOLATE: i32 = -2;
pub const GR_ERR_INVALID_MODULE: i32 = -3;
pub const GR_ERR_INVALID_FUNCTION: i32 = -4;
pub const GR_ERR_CALLBACK_DUPLICATE: i32 = -5;
pub const GR_ERR_CALLBACK_NOT_FOUND: i32 = -6;
pub const GR_ERR_BUFFER_TOO_SMALL: i32 = -7;
pub const GR_ERR_INTERNAL: i32 = -99;

const ENGINE_NAME: []const u8 = "cpython";

// Engine-version string is built lazily at engine init from
// ENGINE_VERSION_BASE + the actual `Py_GetVersion()` result + a
// `free-threaded` annotation when `build_opts.python_free_threaded`
// is set, so the embedded Python ABI is visible
// (`cpython-0.1.0-m13-phase3h (Python 3.13.13)` for GIL'd builds,
//  `cpython-0.1.0-m13-phase3h (Python 3.14.4 free-threaded)` for PEP 703 builds).
const ENGINE_VERSION_BASE: []const u8 = "cpython-0.1.0-m13-phase3h";

// Result-buffer sizing for host_callback_trampoline (mirrors JSC's
// HOST_RESULT_INITIAL/MAX). On `GR_ERR_BUFFER_TOO_SMALL` from the Rust
// side we resize once up to HOST_RESULT_MAX before giving up.
const HOST_RESULT_INITIAL: usize = 4096;
const HOST_RESULT_MAX: usize = 1 << 20;

/// Stable name used by every host-callback capsule. The capsule
/// identity is process-global (capsule names are matched as C-string
/// pointers OR by strcmp depending on the CPython version, so a
/// constant-address literal is the safe choice).
const CAPSULE_NAME: [*:0]const u8 = "graphreact.host_callback_entry";

// ---------------------------------------------------------------------
// Per-isolate state
// ---------------------------------------------------------------------

const ModuleEntry = struct {
    handle: u64,
    /// Owned `PyObject*` reference (Py_INCREF on store, Py_DECREF on
    /// dispose). The module is the dict produced by evaluating user
    /// source — `PyObject_GetAttrString` reads from it.
    module: *c.PyObject,
};

const FunctionEntry = struct {
    handle: u64,
    /// Owned `PyObject*` reference to a callable. Resolved from a
    /// stored module via `PyObject_GetAttrString`.
    function: *c.PyObject,
};

const Isolate = struct {
    handle: u64,
    /// Sub-interpreter thread state created by `Py_NewInterpreter`.
    /// The main interpreter's thread state is held in the State
    /// struct; per-isolate work swaps to this thread state and back.
    thread_state: *c.PyThreadState,
    modules: std.array_list.Managed(ModuleEntry),
    functions: std.array_list.Managed(FunctionEntry),
    next_module: u64,
    next_function: u64,
};

const State = struct {
    initialized: bool,
    /// Main interpreter's thread state captured after
    /// `Py_Initialize`. Used as the "resting" state between isolate
    /// operations.
    main_thread_state: ?*c.PyThreadState,
    isolates: std.array_list.Managed(?*Isolate),
    next_handle: u64,
    /// Cached `Py_GetVersion()` result baked into the engine version
    /// string at init time. Static lifetime within the process.
    version_string: ?[]u8,
};

var gpa_state: std.heap.DebugAllocator(.{}) = .{};
var allocator: std.mem.Allocator = undefined;
var state: State = .{
    .initialized = false,
    .main_thread_state = null,
    .isolates = undefined,
    .next_handle = 1,
    .version_string = null,
};

inline fn ensureInitialized() i32 {
    return if (state.initialized) GR_OK else GR_ERR_NOT_INITIALIZED;
}

fn isolateById(handle: u64) ?*Isolate {
    if (handle == 0) return null;
    for (state.isolates.items) |slot| {
        if (slot) |iso| {
            if (iso.handle == handle) return iso;
        }
    }
    return null;
}

fn moduleEntry(iso: *Isolate, handle: u64) ?*ModuleEntry {
    for (iso.modules.items) |*entry| {
        if (entry.handle == handle) return entry;
    }
    return null;
}

fn functionEntry(iso: *Isolate, handle: u64) ?*FunctionEntry {
    for (iso.functions.items) |*entry| {
        if (entry.handle == handle) return entry;
    }
    return null;
}

fn writeStringOut(text: []const u8, out_buf: ?[*]u8, out_len: ?*usize) i32 {
    const len_ptr = out_len orelse return GR_ERR_INTERNAL;
    const cap = len_ptr.*;
    len_ptr.* = text.len;
    if (cap < text.len) return GR_ERR_BUFFER_TOO_SMALL;
    const buf = out_buf orelse return GR_ERR_BUFFER_TOO_SMALL;
    @memcpy(buf[0..text.len], text);
    return GR_OK;
}

fn writeBytesOut(bytes: []const u8, out_buf: ?[*]u8, cap: usize, out_len: ?*usize) i32 {
    const len_ptr = out_len orelse return GR_ERR_INTERNAL;
    len_ptr.* = bytes.len;
    if (bytes.len > cap) return GR_ERR_BUFFER_TOO_SMALL;
    const buf = out_buf orelse return GR_ERR_BUFFER_TOO_SMALL;
    if (bytes.len > 0) {
        @memcpy(buf[0..bytes.len], bytes);
    }
    return GR_OK;
}

// ---------------------------------------------------------------------
// Engine lifecycle (M13-phase1)
// ---------------------------------------------------------------------

export fn gr_cpython_engine_initialize() i32 {
    if (state.initialized) return GR_OK;

    // First-ever init in this process: bring up CPython + Zig
    // allocator + isolate registry + cached version string.
    // Subsequent inits (after a `shutdown()`) skip all of this and
    // just flip the flag — the main interpreter remains alive
    // (process-singleton, see `gr_cpython_engine_shutdown` doc).
    if (state.version_string == null) {
        allocator = gpa_state.allocator();
        state.isolates = std.array_list.Managed(?*Isolate).init(allocator);

        // `Py_InitializeEx(0)` skips the default signal handler
        // installation — important so we don't hijack SIGINT etc.
        // from the calling process.
        c.Py_InitializeEx(0);
        state.main_thread_state = c.PyThreadState_Get();

        // Bake the version string. `Py_GetVersion()` returns a
        // static C string like "3.9.6 (default, Mar 31 2024,
        // 23:11:33)" — grab everything before the first space so
        // the user-visible form is
        // "cpython-0.1.0-m13-phase1+2 (Python 3.9.6)".
        const py_version_cstr: [*:0]const u8 = c.Py_GetVersion();
        const py_version_full = std.mem.span(py_version_cstr);
        const space_idx = std.mem.indexOfScalar(u8, py_version_full, ' ');
        const py_version_short =
            if (space_idx) |i| py_version_full[0..i] else py_version_full;

        // M13-phase3h: append " free-threaded" when the build was
        // linked against a PEP 703 build (`python3.Xt`). Comptime-
        // gated so the GIL'd build's version string stays unchanged.
        const version = if (comptime build_opts.python_free_threaded)
            std.fmt.allocPrint(
                allocator,
                "{s} (Python {s} free-threaded)",
                .{ ENGINE_VERSION_BASE, py_version_short },
            ) catch {
                return GR_ERR_INTERNAL;
            }
        else
            std.fmt.allocPrint(
                allocator,
                "{s} (Python {s})",
                .{ ENGINE_VERSION_BASE, py_version_short },
            ) catch {
                return GR_ERR_INTERNAL;
            };
        state.version_string = version;
    }

    state.initialized = true;
    return GR_OK;
}

export fn gr_cpython_engine_shutdown() i32 {
    if (!state.initialized) return GR_OK;

    // Tear down every still-live isolate so disposed handles return
    // the right error code on subsequent calls.
    for (state.isolates.items) |slot| {
        if (slot) |iso| {
            disposeIsolateInner(iso);
        }
    }
    state.isolates.clearRetainingCapacity();

    // **M13-phase3f branch.** On Python 3.12+, PEP 684 +
    // `Py_FinalizeEx` lift the historical "main interpreter is
    // process-singleton, never finalize" workaround — main can be
    // finalized cleanly and re-`Py_InitializeEx`'d on the next
    // `gr_cpython_engine_initialize` call. On ≤3.11 the legacy
    // shape stays: `Py_FinalizeEx` followed by `Py_Initialize` in
    // the same process is unsupported (the symbol-table and
    // C-extension state isn't fully reset, so subsequent
    // `Py_NewInterpreter` calls SIGSEGV on stale globals — see
    // https://docs.python.org/3.9/c-api/init.html), so we treat
    // shutdown as "clean up isolates, leave main alive" and let the
    // process exit reclaim everything.
    //
    // `gr_cpython_engine_initialize`'s "first-ever init" gate is
    // `state.version_string == null`, so on 3.12+ shutdown nulls
    // version_string + tears down isolates list + state.allocator-
    // owned data so the next initialize() does a real bring-up.
    if (comptime build_opts.python_3_12) {
        if (state.main_thread_state) |ts| {
            _ = c.PyThreadState_Swap(ts);
        }
        _ = c.Py_FinalizeEx();
        state.main_thread_state = null;
        state.isolates.deinit();
        state.isolates = std.array_list.Managed(?*Isolate).init(allocator);
        if (state.version_string) |v| {
            allocator.free(v);
            state.version_string = null;
        }
    }
    // ≤3.11: keep main interpreter, version string, allocator, and
    // isolates list alive. `state.initialized` flips back to false
    // so the public `Engine` trait contract is preserved.

    state.initialized = false;
    return GR_OK;
}

export fn gr_cpython_engine_name(out_buf: ?[*]u8, out_len: ?*usize) i32 {
    return writeStringOut(ENGINE_NAME, out_buf, out_len);
}

export fn gr_cpython_engine_version(out_buf: ?[*]u8, out_len: ?*usize) i32 {
    if (state.version_string) |v| {
        return writeStringOut(v, out_buf, out_len);
    }
    return writeStringOut(ENGINE_VERSION_BASE, out_buf, out_len);
}

export fn gr_cpython_engine_live_isolates() u64 {
    var count: u64 = 0;
    for (state.isolates.items) |slot| {
        if (slot != null) count += 1;
    }
    return count;
}

// ---------------------------------------------------------------------
// Isolates — sub-interpreters via `Py_NewInterpreter`
// ---------------------------------------------------------------------

export fn gr_cpython_isolate_create(out_handle: ?*u64) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const handle_out = out_handle orelse return GR_ERR_INTERNAL;

    // Ensure we're on the main thread state before creating the
    // sub-interpreter — `Py_NewInterpreter` /
    // `Py_NewInterpreterFromConfig` both operate relative to
    // whatever thread state is current.
    if (state.main_thread_state) |ts| {
        _ = c.PyThreadState_Swap(ts);
    }

    // **M13-phase3f branch.** On Python 3.12+, request a per-
    // interpreter GIL (PEP 684) by routing through
    // `Py_NewInterpreterFromConfig` with
    // `PyInterpreterConfig.gil = PyInterpreterConfig_OWN_GIL`. The
    // legacy `Py_NewInterpreter` shape (every sub-interpreter shares
    // the process-global GIL) is retained for ≤3.11 builds because
    // `PyInterpreterConfig` and `Py_NewInterpreterFromConfig` don't
    // exist in those header sets. Comptime branch keeps the legacy
    // path compiling against 3.9 headers.
    var new_ts: *c.PyThreadState = undefined;
    if (comptime build_opts.python_3_12) {
        // OWN_GIL config (PEP 684):
        //   - use_main_obmalloc = 0 — required; sub-interpreter must
        //     own its allocator state.
        //   - check_multi_interp_extensions = 1 — required; rejects
        //     C extensions that haven't opted in to multi-interp.
        //   - gil = OWN_GIL — the load-bearing field.
        //   - allow_fork / allow_exec = 1 — preserves the legacy
        //     behavior; users who don't fork/exec from sub-interps
        //     pay nothing.
        //   - allow_threads = 1 / allow_daemon_threads = 1 — sub-
        //     interpreters can spawn their own Python threads.
        var config: c.PyInterpreterConfig = .{
            .use_main_obmalloc = 0,
            .allow_fork = 1,
            .allow_exec = 1,
            .allow_threads = 1,
            .allow_daemon_threads = 1,
            .check_multi_interp_extensions = 1,
            .gil = c.PyInterpreterConfig_OWN_GIL,
        };
        var tstate_out: ?*c.PyThreadState = null;
        const status = c.Py_NewInterpreterFromConfig(&tstate_out, &config);
        if (c.PyStatus_IsError(status) != 0 or tstate_out == null) {
            // Restore main thread state on failure so subsequent
            // isolate creates aren't stuck on a half-initialised
            // sub-interpreter.
            if (state.main_thread_state) |ts| {
                _ = c.PyThreadState_Swap(ts);
            }
            return GR_ERR_INTERNAL;
        }
        new_ts = tstate_out.?;
    } else {
        new_ts = c.Py_NewInterpreter() orelse return GR_ERR_INTERNAL;
    }
    // Both Py_NewInterpreter and Py_NewInterpreterFromConfig leave
    // new_ts as current. Restore main so subsequent isolate creates
    // start from a clean state.
    if (state.main_thread_state) |ts| {
        _ = c.PyThreadState_Swap(ts);
    }

    const iso = allocator.create(Isolate) catch {
        // Roll back the sub-interpreter we just created.
        _ = c.PyThreadState_Swap(new_ts);
        c.Py_EndInterpreter(new_ts);
        if (state.main_thread_state) |ts| {
            _ = c.PyThreadState_Swap(ts);
        }
        return GR_ERR_INTERNAL;
    };
    iso.* = .{
        .handle = state.next_handle,
        .thread_state = new_ts,
        .modules = std.array_list.Managed(ModuleEntry).init(allocator),
        .functions = std.array_list.Managed(FunctionEntry).init(allocator),
        .next_module = 1,
        .next_function = 1,
    };
    state.next_handle += 1;

    state.isolates.append(iso) catch {
        _ = c.PyThreadState_Swap(new_ts);
        c.Py_EndInterpreter(new_ts);
        if (state.main_thread_state) |ts| {
            _ = c.PyThreadState_Swap(ts);
        }
        iso.modules.deinit();
        iso.functions.deinit();
        allocator.destroy(iso);
        return GR_ERR_INTERNAL;
    };

    handle_out.* = iso.handle;
    return GR_OK;
}

/// Inner disposal — assumes the caller has already nulled the slot
/// in `state.isolates`. Drops Python refs, ends the sub-interpreter,
/// frees the Zig-side allocations.
fn disposeIsolateInner(iso: *Isolate) void {
    // Swap to the isolate's thread state so all PyObject ref-drops
    // and Py_EndInterpreter operate against it.
    _ = c.PyThreadState_Swap(iso.thread_state);

    // Drop module/function references (Py_DECREF). `c.Py_DecRef` is
    // a macro, so we use `Py_XDECREF` via the wrapping helper.
    for (iso.functions.items) |entry| {
        c.Py_DecRef(entry.function);
    }
    for (iso.modules.items) |entry| {
        c.Py_DecRef(entry.module);
    }
    iso.modules.deinit();
    iso.functions.deinit();

    c.Py_EndInterpreter(iso.thread_state);

    if (state.main_thread_state) |ts| {
        _ = c.PyThreadState_Swap(ts);
    }
    allocator.destroy(iso);
}

export fn gr_cpython_isolate_dispose(handle: u64) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    if (handle == 0) return GR_ERR_INVALID_ISOLATE;

    var i: usize = 0;
    while (i < state.isolates.items.len) : (i += 1) {
        if (state.isolates.items[i]) |iso| {
            if (iso.handle == handle) {
                state.isolates.items[i] = null;
                disposeIsolateInner(iso);
                return GR_OK;
            }
        }
    }
    return GR_ERR_INVALID_ISOLATE;
}

// ---------------------------------------------------------------------
// Eval / resolve / call (M13-phase2)
// ---------------------------------------------------------------------

/// RAII-style scope guard that swaps to a target thread state on
/// construction and restores the previous one on `release()`. Use as:
///   var guard = ThreadStateGuard.enter(iso.thread_state);
///   defer guard.release();
const ThreadStateGuard = struct {
    previous: ?*c.PyThreadState,

    fn enter(target: *c.PyThreadState) ThreadStateGuard {
        const prev = c.PyThreadState_Swap(target);
        return .{ .previous = prev };
    }

    fn release(self: *ThreadStateGuard) void {
        _ = c.PyThreadState_Swap(self.previous);
    }
};

/// Drain Python's pending exception, return its `repr()` as a Zig
/// slice owned by `allocator`. Caller frees. Returns null if no
/// exception is pending or repr fails (in which case GR_ERR_INTERNAL
/// should be propagated regardless).
fn formatPendingException() ?[]u8 {
    if (c.PyErr_Occurred() == null) return null;

    var ptype: ?*c.PyObject = null;
    var pvalue: ?*c.PyObject = null;
    var ptraceback: ?*c.PyObject = null;
    c.PyErr_Fetch(&ptype, &pvalue, &ptraceback);
    c.PyErr_NormalizeException(&ptype, &pvalue, &ptraceback);

    defer {
        if (ptype) |p| c.Py_DecRef(p);
        if (pvalue) |p| c.Py_DecRef(p);
        if (ptraceback) |p| c.Py_DecRef(p);
    }

    const value = pvalue orelse return null;
    const str_obj = c.PyObject_Str(value) orelse return null;
    defer c.Py_DecRef(str_obj);

    const cstr: [*:0]const u8 = c.PyUnicode_AsUTF8(str_obj) orelse return null;
    const slice = std.mem.span(cstr);
    const owned = allocator.alloc(u8, slice.len) catch return null;
    @memcpy(owned, slice);
    return owned;
}

/// Stash a stringified Python exception on stderr for ops visibility.
/// Phase 3+ will route this through a structured error channel; for
/// phase 2, eprintln-equivalent is good enough.
fn logException(context: []const u8) void {
    if (formatPendingException()) |msg| {
        defer allocator.free(msg);
        std.debug.print("[graphreact-cpython] {s}: {s}\n", .{ context, msg });
    } else {
        std.debug.print("[graphreact-cpython] {s}: <unknown error>\n", .{context});
    }
}

export fn gr_cpython_module_register(
    isolate: u64,
    module_name_ptr: [*]const u8,
    module_name_len: usize,
    source_ptr: [*]const u8,
    source_len: usize,
    out_module: ?*u64,
) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const handle_out = out_module orelse return GR_ERR_INTERNAL;

    var guard = ThreadStateGuard.enter(iso.thread_state);
    defer guard.release();

    // Build a fresh module object so each `eval_module` call is
    // isolated from previous ones within the same sub-interpreter.
    // Use a stable name derived from the caller-provided slice
    // (Python wants a NUL-terminated string).
    const name_buf = allocator.alloc(u8, module_name_len + 1) catch return GR_ERR_INTERNAL;
    defer allocator.free(name_buf);
    @memcpy(name_buf[0..module_name_len], module_name_ptr[0..module_name_len]);
    name_buf[module_name_len] = 0;

    const module_obj = c.PyImport_AddModule(@as([*:0]const u8, @ptrCast(name_buf.ptr))) orelse {
        logException("PyImport_AddModule");
        return GR_ERR_INTERNAL;
    };
    // PyImport_AddModule returns a borrowed reference; INCREF before
    // we store it.
    c.Py_IncRef(module_obj);

    const module_dict = c.PyModule_GetDict(module_obj) orelse {
        c.Py_DecRef(module_obj);
        return GR_ERR_INTERNAL;
    };

    // PyRun_String wants a NUL-terminated source. Allocate a temp.
    const src_buf = allocator.alloc(u8, source_len + 1) catch {
        c.Py_DecRef(module_obj);
        return GR_ERR_INTERNAL;
    };
    defer allocator.free(src_buf);
    @memcpy(src_buf[0..source_len], source_ptr[0..source_len]);
    src_buf[source_len] = 0;

    const result = c.PyRun_StringFlags(
        @as([*:0]const u8, @ptrCast(src_buf.ptr)),
        c.Py_file_input, // exec mode (no return value)
        module_dict,
        module_dict,
        null,
    );
    if (result == null) {
        logException("PyRun_String");
        c.Py_DecRef(module_obj);
        return GR_ERR_INTERNAL;
    }
    // PyRun_String returns Py_None on success in file_input mode;
    // drop that ref.
    c.Py_DecRef(result);

    const entry: ModuleEntry = .{
        .handle = iso.next_module,
        .module = module_obj,
    };
    iso.modules.append(entry) catch {
        c.Py_DecRef(module_obj);
        return GR_ERR_INTERNAL;
    };
    iso.next_module += 1;
    handle_out.* = entry.handle;
    return GR_OK;
}

export fn gr_cpython_function_resolve(
    isolate: u64,
    module: u64,
    name_ptr: [*]const u8,
    name_len: usize,
    out_function: ?*u64,
) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const mod_entry = moduleEntry(iso, module) orelse return GR_ERR_INVALID_MODULE;
    const handle_out = out_function orelse return GR_ERR_INTERNAL;

    var guard = ThreadStateGuard.enter(iso.thread_state);
    defer guard.release();

    const name_buf = allocator.alloc(u8, name_len + 1) catch return GR_ERR_INTERNAL;
    defer allocator.free(name_buf);
    @memcpy(name_buf[0..name_len], name_ptr[0..name_len]);
    name_buf[name_len] = 0;

    const fn_obj = c.PyObject_GetAttrString(
        mod_entry.module,
        @as([*:0]const u8, @ptrCast(name_buf.ptr)),
    ) orelse {
        // PyObject_GetAttrString sets AttributeError on miss; clear
        // it so subsequent calls don't see a stale exception.
        logException("PyObject_GetAttrString");
        c.PyErr_Clear();
        return GR_ERR_INVALID_FUNCTION;
    };

    if (c.PyCallable_Check(fn_obj) == 0) {
        c.Py_DecRef(fn_obj);
        return GR_ERR_INVALID_FUNCTION;
    }

    const entry: FunctionEntry = .{
        .handle = iso.next_function,
        .function = fn_obj,
    };
    iso.functions.append(entry) catch {
        c.Py_DecRef(fn_obj);
        return GR_ERR_INTERNAL;
    };
    iso.next_function += 1;
    handle_out.* = entry.handle;
    return GR_OK;
}

/// Lazily-imported `json` module + cached `loads` / `dumps`
/// references per sub-interpreter. Since each sub-interpreter has
/// its own imports table, we can't share these across isolates.
/// Phase 2 imports per call (cheap — Python caches inside
/// `sys.modules`); phase 3 may add a per-isolate cache if profiling
/// shows it matters.
fn jsonLoads(json_ptr: [*]const u8, json_len: usize) ?*c.PyObject {
    const json_buf = allocator.alloc(u8, json_len + 1) catch return null;
    defer allocator.free(json_buf);
    @memcpy(json_buf[0..json_len], json_ptr[0..json_len]);
    json_buf[json_len] = 0;

    const json_mod = c.PyImport_ImportModule("json") orelse return null;
    defer c.Py_DecRef(json_mod);

    const py_str = c.PyUnicode_FromStringAndSize(@as([*c]const u8, @ptrCast(json_buf.ptr)), @intCast(json_len)) orelse return null;
    defer c.Py_DecRef(py_str);

    const result = c.PyObject_CallMethod(json_mod, "loads", "O", py_str);
    return result;
}

fn jsonDumps(value: *c.PyObject) ?[]u8 {
    const json_mod = c.PyImport_ImportModule("json") orelse return null;
    defer c.Py_DecRef(json_mod);

    // Use explicit `PyObject_GetAttrString` + `PyObject_Call(args_tuple)`
    // instead of `PyObject_CallMethod(json_mod, "dumps", "O", value)` —
    // when `value` is itself a tuple (e.g., the args tuple a METH_VARARGS
    // PyCFunction receives), CallMethod's "wrap if not tuple" logic
    // sees the tuple-shape and uses it directly as the call's args
    // tuple, unpacking `(2, 3)` into `dumps(2, 3)` and triggering
    // "dumps() takes 1 positional argument but 2 were given". Building
    // the 1-tuple manually avoids the unpack footgun and is uniform
    // across all value shapes.
    const dumps_fn = c.PyObject_GetAttrString(json_mod, "dumps") orelse return null;
    defer c.Py_DecRef(dumps_fn);

    const call_args = c.PyTuple_New(1) orelse return null;
    defer c.Py_DecRef(call_args);
    c.Py_IncRef(value); // PyTuple_SetItem steals the ref.
    if (c.PyTuple_SetItem(call_args, 0, value) != 0) return null;

    const result = c.PyObject_Call(dumps_fn, call_args, null) orelse return null;
    defer c.Py_DecRef(result);

    const cstr: [*:0]const u8 = c.PyUnicode_AsUTF8(result) orelse return null;
    const slice = std.mem.span(cstr);
    const owned = allocator.alloc(u8, slice.len) catch return null;
    @memcpy(owned, slice);
    return owned;
}

export fn gr_cpython_function_call(
    isolate: u64,
    function: u64,
    args_json_ptr: [*]const u8,
    args_json_len: usize,
    out_result: ?[*]u8,
    out_result_cap: usize,
    out_result_len: ?*usize,
) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const fn_entry = functionEntry(iso, function) orelse return GR_ERR_INVALID_FUNCTION;

    var guard = ThreadStateGuard.enter(iso.thread_state);
    defer guard.release();

    // Decode args JSON → Python list. Caller passes `[]` for no
    // args, `[a, b, c]` for positional args.
    const args_list = jsonLoads(args_json_ptr, args_json_len) orelse {
        logException("json.loads(args)");
        c.PyErr_Clear();
        return GR_ERR_INTERNAL;
    };
    defer c.Py_DecRef(args_list);

    // Convert the list into a tuple (PyObject_Call wants a tuple).
    // PyList_AsTuple sets a TypeError + returns null if the argument
    // isn't actually a list, so we don't need a separate PyList_Check
    // — the explicit check would also be a macro expansion that
    // touches `_Py_atomic_*` helpers in PEP 703 free-threaded builds
    // (which translate-c can't inline; the symbols aren't exported
    // by libpython3.Xt). Same reasoning as M13-phase3f's
    // `Py_INCREF` → `Py_IncRef` swap, applied to the type-check
    // macros.
    const args_tuple = c.PyList_AsTuple(args_list) orelse return GR_ERR_INTERNAL;
    defer c.Py_DecRef(args_tuple);

    const result = c.PyObject_Call(fn_entry.function, args_tuple, null) orelse {
        logException("PyObject_Call");
        c.PyErr_Clear();
        return GR_ERR_INTERNAL;
    };
    defer c.Py_DecRef(result);

    const result_json = jsonDumps(result) orelse {
        logException("json.dumps(result)");
        c.PyErr_Clear();
        return GR_ERR_INTERNAL;
    };
    defer allocator.free(result_json);

    return writeBytesOut(result_json, out_result, out_result_cap, out_result_len);
}

// ---------------------------------------------------------------------
// Microtasks — no-op
// ---------------------------------------------------------------------

export fn gr_cpython_microtasks_drain(isolate: u64) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    _ = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    // CPython has no embedder-level microtask queue. asyncio
    // integration is userspace machinery (event-loop policy +
    // run_until_complete) and lands at a later phase analogous to
    // M12.3b-phase1's JS Promise → Rust Future bridge. Phase 1
    // returns OK so `drain_microtasks` works uniformly across
    // engines.
    return GR_OK;
}

// ---------------------------------------------------------------------
// Callbacks — M13-phase2.5: real bidirectional Rust↔Python routing
// ---------------------------------------------------------------------
//
// CPython analogue of JSC's M12.3a-phase2.5. Each registered host
// callback becomes a `PyCFunction` whose `m_self` slot is a
// `PyCapsule` carrying a pointer to a heap-allocated `CallbackEntry`.
// The shared `pythonCallbackTrampoline` is invoked by Python on call;
// it reads the entry from the capsule, marshals args via stdlib
// `json`, invokes the Rust trampoline (which routes by id into the
// process-global `CALLBACK_REGISTRY` on the Rust side), and parses the
// JSON result back into a `PyObject*`.
//
// The capsule destructor frees the entry deterministically when the
// hosting sub-interpreter is torn down (`Py_EndInterpreter` clears
// the builtins dict → PyCFunction GC'd → capsule GC'd → destructor
// runs). Symmetric path: `gr_cpython_callback_invoke` looks up a
// Python callable on the active sub-interpreter's builtins by name
// and calls it via `PyObject_Call`.

/// Host-callback function pointer. Matches the C ABI declared by
/// `gr_host_callback_fn` on the Rust side.
const HostCallbackFn = *const fn (
    cb_data: ?*anyopaque,
    args_json: ?[*]const u8,
    args_len: usize,
    out_result: ?[*]u8,
    out_result_cap: usize,
    out_result_len: ?*usize,
) callconv(.c) i32;

/// Per-callback entry pointed at by every PyCapsule we hand to
/// PyCFunction_New. `host_data` is the registry id encoded as a fake
/// pointer — the Rust trampoline reads it back as a u64 and uses it
/// to look up the closure in `CALLBACK_REGISTRY`. `name` is owned
/// (allocator-allocated) for diagnostic visibility; freed when the
/// capsule destructor runs.
const CallbackEntry = struct {
    id: u64,
    name: []u8,
    host_fn: HostCallbackFn,
    host_data: ?*anyopaque,
};

/// Single shared `PyMethodDef` used by every host callback. Identity
/// lives in the capsule, not the methoddef — so one static is enough
/// for the entire process. `ml_name` shows up in `repr(host_xyz)`;
/// the generic name is fine for phase 2.5 (cosmetic only).
var SHARED_METHOD_DEF: c.PyMethodDef = .{
    .ml_name = "graphreact_host_callback",
    .ml_meth = pythonCallbackTrampoline,
    .ml_flags = c.METH_VARARGS,
    .ml_doc = null,
};

/// Process-global counter for callback ids. Encoded as a fake pointer
/// in `entry.host_data` and passed to the Rust trampoline as the
/// `CALLBACK_REGISTRY` key. Spans isolates so the Rust registry
/// doesn't need per-isolate keying. Single-threaded under the Rust
/// `engine_lock()` mutex; non-atomic counter is sufficient.
var global_callback_counter: u64 = 0;

fn nextGlobalCallbackId() u64 {
    global_callback_counter += 1;
    return global_callback_counter;
}

/// Capsule destructor. Runs when the capsule's refcount hits zero —
/// either explicit `Py_DECREF` (we never do this) or, more commonly,
/// during sub-interpreter teardown when the builtins dict is cleared
/// and the wrapping `PyCFunction` is freed. The active thread state
/// is whichever sub-interpreter is being torn down; the GIL is held.
/// Frees the heap-allocated `CallbackEntry`.
fn callbackEntryCapsuleDestructor(capsule: ?*c.PyObject) callconv(.c) void {
    if (capsule == null) return;
    const entry_ptr = c.PyCapsule_GetPointer(capsule, CAPSULE_NAME);
    if (entry_ptr == null) {
        // Wrong-name or already-freed; clear the pending exception so
        // the GC continues cleanly.
        c.PyErr_Clear();
        return;
    }
    const entry: *CallbackEntry = @ptrCast(@alignCast(entry_ptr));
    allocator.free(entry.name);
    allocator.destroy(entry);
}

/// Marshal a Python object (typically a tuple of args, or any return
/// value) to a JSON string via `json.dumps`. Caller frees with
/// `allocator.free`. Returns null + sets/leaves a pending Python
/// exception on failure.
fn jsonDumpsToOwnedSlice(value: *c.PyObject) ?[]u8 {
    return jsonDumps(value);
}

/// Parse a JSON byte slice into a Python object via `json.loads`.
/// Caller owns the resulting reference (`Py_DECREF` when done).
/// Returns null + sets/leaves a pending Python exception on failure.
fn jsonLoadsFromBytes(bytes_ptr: [*]const u8, bytes_len: usize) ?*c.PyObject {
    return jsonLoads(bytes_ptr, bytes_len);
}

/// PyCFunction `(self, args)` callback. Reads the entry from the
/// capsule passed as `self`, marshals `args` (a tuple) to JSON, calls
/// the Rust trampoline, and parses the JSON result back into a Python
/// object. Sets a Python exception on any failure; returns NULL in
/// that case so Python propagates the error.
fn pythonCallbackTrampoline(
    self: ?*c.PyObject,
    args: ?*c.PyObject,
) callconv(.c) ?*c.PyObject {
    const capsule = self orelse {
        c.PyErr_SetString(c.PyExc_RuntimeError, "host callback: missing self capsule");
        return null;
    };
    const entry_ptr = c.PyCapsule_GetPointer(capsule, CAPSULE_NAME);
    if (entry_ptr == null) {
        // PyCapsule_GetPointer sets an exception; surface it as-is.
        return null;
    }
    const entry: *CallbackEntry = @ptrCast(@alignCast(entry_ptr));

    // Marshal args (a tuple) to a JSON array string. `json.dumps` on
    // a tuple emits a JSON array, which is the same shape JSC sends.
    const args_obj = args orelse {
        c.PyErr_SetString(c.PyExc_RuntimeError, "host callback: missing args tuple");
        return null;
    };
    const args_json = jsonDumpsToOwnedSlice(args_obj) orelse {
        // jsonDumps already left a Python exception pending (or set
        // its own). If somehow not, set a generic one.
        if (c.PyErr_Occurred() == null) {
            c.PyErr_SetString(c.PyExc_RuntimeError, "host callback: failed to marshal args");
        }
        return null;
    };
    defer allocator.free(args_json);

    // Allocate result buffer; retry once on too-small (mirrors JSC's
    // pattern in `jscCallbackTrampoline`).
    var cap: usize = HOST_RESULT_INITIAL;
    var result_json: ?[]u8 = null;
    while (true) {
        var buf = allocator.alloc(u8, cap) catch {
            c.PyErr_SetString(c.PyExc_MemoryError, "host callback: out of memory");
            return null;
        };
        var written: usize = cap;
        const rc = entry.host_fn(
            entry.host_data,
            args_json.ptr,
            args_json.len,
            buf.ptr,
            cap,
            &written,
        );
        if (rc == GR_OK) {
            result_json = if (written < cap)
                (allocator.realloc(buf, written) catch buf[0..written])
            else
                buf;
            break;
        }
        if (rc == GR_ERR_BUFFER_TOO_SMALL and written > cap and cap < HOST_RESULT_MAX) {
            allocator.free(buf);
            cap = @min(@max(written, cap * 2), HOST_RESULT_MAX);
            continue;
        }
        allocator.free(buf);
        c.PyErr_SetString(c.PyExc_RuntimeError, "host callback: rust trampoline returned error");
        return null;
    }

    const json = result_json orelse {
        c.PyErr_SetString(c.PyExc_RuntimeError, "host callback: no result");
        return null;
    };
    defer allocator.free(json);

    // Empty payload → return Python None (the closure returned
    // `HostValue::Null` which serialises to "null"; "null".len > 0,
    // so this branch only handles a true empty-buffer pathology).
    if (json.len == 0) {
        const none = c.Py_None();
        c.Py_IncRef(none);
        return none;
    }

    // Parse the JSON result back to a Python value.
    const result_obj = jsonLoadsFromBytes(json.ptr, json.len) orelse {
        if (c.PyErr_Occurred() == null) {
            c.PyErr_SetString(c.PyExc_RuntimeError, "host callback: invalid json result");
        }
        return null;
    };
    return result_obj;
}

export fn gr_cpython_callback_register(
    isolate: u64,
    name_ptr: [*]const u8,
    name_len: usize,
    cb_fn: ?HostCallbackFn,
    cb_data: ?*anyopaque,
    out_callback_id: ?*u64,
) i32 {
    _ = cb_data; // The Rust-supplied cb_data slot is reserved for
    // future engine-identity routing; identity is carried in the
    // entry's host_data (the registry id encoded as a fake pointer).
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const out = out_callback_id orelse return GR_ERR_INTERNAL;
    const fn_ptr = cb_fn orelse return GR_ERR_INTERNAL;

    if (name_len == 0) return GR_ERR_INTERNAL;
    const name_slice: []const u8 = name_ptr[0..name_len];

    // Switch to the target sub-interpreter's thread state; every
    // PyObject we touch below belongs to its module/builtins graph.
    var guard = ThreadStateGuard.enter(iso.thread_state);
    defer guard.release();

    // Allocate name with a NUL terminator so PyDict_SetItemString can
    // consume it. Owned by the entry; freed in the capsule destructor.
    const owned_name = allocator.alloc(u8, name_slice.len + 1) catch return GR_ERR_INTERNAL;
    @memcpy(owned_name[0..name_slice.len], name_slice);
    owned_name[name_slice.len] = 0;

    const id = nextGlobalCallbackId();
    const entry = allocator.create(CallbackEntry) catch {
        allocator.free(owned_name);
        return GR_ERR_INTERNAL;
    };
    entry.* = .{
        .id = id,
        .name = owned_name,
        .host_fn = fn_ptr,
        .host_data = @ptrFromInt(id),
    };

    // PyCapsule_New takes ownership of the pointer; the destructor
    // frees the entry when the capsule is GC'd.
    const capsule = c.PyCapsule_New(
        @as(?*anyopaque, @ptrCast(entry)),
        CAPSULE_NAME,
        callbackEntryCapsuleDestructor,
    ) orelse {
        logException("PyCapsule_New");
        c.PyErr_Clear();
        allocator.free(owned_name);
        allocator.destroy(entry);
        return GR_ERR_INTERNAL;
    };
    // capsule refcount = 1 here.

    // Use PyCFunction_NewEx (explicit form) instead of PyCFunction_New
    // (inline wrapper) — Zig 0.16's stricter type checker refuses
    // the wrapper because cimport's `NULL` macro types as `?*anyopaque`
    // and won't coerce to `[*c]PyObject` inside the generic instantiation.
    const py_callable = c.PyCFunction_NewEx(&SHARED_METHOD_DEF, capsule, null) orelse {
        logException("PyCFunction_NewEx");
        c.PyErr_Clear();
        // Drop the capsule (its destructor will free the entry).
        c.Py_DecRef(capsule);
        return GR_ERR_INTERNAL;
    };
    // PyCFunction_New INCREF'd the capsule; we can drop our ref so
    // the capsule's lifetime is tied to the PyCFunction.
    c.Py_DecRef(capsule);

    // Install on the active sub-interpreter's builtins so callers
    // can invoke `host_xyz(...)` from any module without import.
    const builtins = c.PyEval_GetBuiltins();
    if (builtins == null) {
        c.Py_DecRef(py_callable);
        return GR_ERR_INTERNAL;
    }
    // PyEval_GetBuiltins returns a borrowed reference; PyDict_SetItemString
    // INCREFs the value. `owned_name` is NUL-terminated above.
    const set_rc = c.PyDict_SetItemString(
        builtins,
        @as([*:0]const u8, @ptrCast(owned_name.ptr)),
        py_callable,
    );
    // PyDict_SetItemString INCREFs the value; drop our ref.
    c.Py_DecRef(py_callable);
    if (set_rc != 0) {
        logException("PyDict_SetItemString(builtins)");
        c.PyErr_Clear();
        // The PyCFunction has already been GC'd (refcount hit 0 on
        // the DECREF above since SetItemString didn't take a ref).
        // The capsule went with it; its destructor freed the entry.
        return GR_ERR_INTERNAL;
    }

    out.* = id;
    return GR_OK;
}

export fn gr_cpython_callback_invoke(
    isolate: u64,
    name_ptr: [*]const u8,
    name_len: usize,
    args_json_ptr: [*]const u8,
    args_json_len: usize,
    out_result: ?[*]u8,
    out_result_cap: usize,
    out_result_len: ?*usize,
) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;

    if (name_len == 0) return GR_ERR_CALLBACK_NOT_FOUND;
    const name_slice: []const u8 = name_ptr[0..name_len];

    var guard = ThreadStateGuard.enter(iso.thread_state);
    defer guard.release();

    // NUL-terminate the name for PyDict_GetItemString.
    const name_buf = allocator.alloc(u8, name_slice.len + 1) catch return GR_ERR_INTERNAL;
    defer allocator.free(name_buf);
    @memcpy(name_buf[0..name_slice.len], name_slice);
    name_buf[name_slice.len] = 0;

    const builtins = c.PyEval_GetBuiltins();
    if (builtins == null) return GR_ERR_INTERNAL;

    // PyDict_GetItemString returns a BORROWED reference (or null +
    // no exception if missing). We INCREF below if found.
    const fn_obj_borrowed = c.PyDict_GetItemString(
        builtins,
        @as([*:0]const u8, @ptrCast(name_buf.ptr)),
    );
    if (fn_obj_borrowed == null) return GR_ERR_CALLBACK_NOT_FOUND;
    if (c.PyCallable_Check(fn_obj_borrowed) == 0) return GR_ERR_CALLBACK_NOT_FOUND;
    c.Py_IncRef(fn_obj_borrowed);
    const fn_obj = fn_obj_borrowed;
    defer c.Py_DecRef(fn_obj);

    // Decode args JSON → Python list → tuple (PyObject_Call wants a
    // tuple). Caller passes "[]" for no args.
    const args_slice_ptr: [*]const u8 = if (args_json_len == 0) blk: {
        const fallback = "[]";
        break :blk fallback;
    } else args_json_ptr;
    const args_slice_len: usize = if (args_json_len == 0) 2 else args_json_len;

    const args_list = jsonLoads(args_slice_ptr, args_slice_len) orelse {
        logException("json.loads(args)");
        c.PyErr_Clear();
        return GR_ERR_INTERNAL;
    };
    defer c.Py_DecRef(args_list);

    // PyList_Check elided — PyList_AsTuple sets TypeError on non-list
    // (same M13-phase3h reasoning as the symmetric site above).
    const args_tuple = c.PyList_AsTuple(args_list) orelse return GR_ERR_INTERNAL;
    defer c.Py_DecRef(args_tuple);

    const result = c.PyObject_Call(fn_obj, args_tuple, null) orelse {
        logException("PyObject_Call");
        c.PyErr_Clear();
        return GR_ERR_INTERNAL;
    };
    defer c.Py_DecRef(result);

    const result_json = jsonDumps(result) orelse {
        logException("json.dumps(result)");
        c.PyErr_Clear();
        return GR_ERR_INTERNAL;
    };
    defer allocator.free(result_json);

    return writeBytesOut(result_json, out_result, out_result_cap, out_result_len);
}
