// jsc.zig — JavaScriptCore engine bridge, M12.3a (closed) +
// M12.3a-followon A-F (closed 2026-05-02) + M12.3b-phase1+1.5
// (shipped 2026-05-02).
//
// Implements the C ABI surface declared by `crates/graphreact-engine-jsc/src/ffi.rs`
// against JSC's public C API on macOS (`JavaScriptCore.framework`).
// Symbols are prefixed `gr_jsc_*` to avoid collision with the C echo
// bridge's `gr_*` symbols.
//
// **Phase2 (real eval/resolve/call):** `JSEvaluateScript` for
// `eval_module`, `JSObjectGetProperty` for `resolve_function`,
// `JSObjectCallAsFunction` for `call_function` with JSON arg/result
// marshalling. Strong-reference protection (`JSValueProtect`) for
// stored module/function values so JSC's GC doesn't free them out
// from under us.
//
// **Phase2.5 (real bidirectional callbacks):** JSC's
// `JSObjectMakeFunctionWithCallback` has no private-data slot, so we
// register host functions via `JSClassDefinition` + `JSClassCreate` +
// `JSObjectMake(ctx, class, private_data)`. The C trampoline reads
// the private-data pointer (a `*CallbackEntry`), marshals args to a
// JSON array via `JSObjectMakeArray` + `JSValueCreateJSONString`,
// invokes the registered Rust trampoline with the entry's stored
// host_data (which the Rust side uses as a registry key), and parses
// the JSON result back via `JSValueMakeFromJSONString`. The function
// object is installed on the isolate's global object under the given
// name, so `host.add(2,3)`-style calls work from JS without any
// per-call wiring. `gr_jsc_callback_invoke` provides the symmetric
// path: look up a JS function by name on the global, invoke it with
// JSON-marshalled args.
//
// **Phase 3a-3e + followon A-F:** all 20 hooks real on JSC; full
// Tier 1/2/3 graph query surface; `useEdgeAcceptHandler` predicate
// vetoes Phase 9 mutations; bounded-leak-free body-time
// `__hostCallbacks` token registry. See M6.6 parity doc.
//
// **drain_microtasks** — M12.3b-phase1.5: real on macOS via the
// extern-C C++ shim at `crates/graphreact-engine-jsc/src/drain_shim.cpp`.
// The shim forward-declares `JSC::VM::drainMicrotasks()`; the C++
// compiler emits the call with the matching Itanium-mangled symbol
// (`_ZN3JSC2VM15drainMicrotasksEv`), which is exported from the
// macOS system framework binary. The shim derives `JSC::VM*` from
// `JSContextRef` via `JSContextGetGroup` + a layout-compat
// `reinterpret_cast<JSC::VM*>(OpaqueJSContextGroup*)` — the same
// pattern used internally in WebKit's `APICast.h`. M12.3b-phase4
// will rewire the shim against vendored WebKit headers for
// cross-platform support; until then, Linux/Windows builds aren't
// supported (build.rs panics).
//
// **Hook prelude — fully wired as of M12.3a-phase3e-ζ + followon
// series A-F (2026-05-02).** Every isolate has a one-time preamble
// eval that installs the JS-facing hook surface; each hook
// dispatches via the phase 2.5 callback machinery into a Rust
// closure registered by `crates/graphreact-cli/src/jsruntime.rs`'s
// `mount_with_scheduler`. Real on JSC: `useState` (3b),
// `useEffect`+cleanup (3c), `useEmit`/`useMessage` (3d),
// `useMemo`/`useCallback`/`useReducer` (3e-α),
// `useNeighbors`/`useTopology`/`useAcceptEdge` (3e-β),
// `useSpawnChild`/`useUnmountSelf`/`useCompose` (3e-γ),
// `ctx.directory.*` + `useGraphQuery.__graphQueryProxy` (3e-δ),
// `useSource` + `timerSource`/`delaySource`/`httpSource`/`watchSource`
// (3e-ε + followon-A), `useMonitor`/`useDemonitor`/`useLink`/
// `useUnlink`/`useGroupJoin`/`useGroupLeave` (3e-ζ),
// `useEffectAsync` (M12.3b-phase1 sync subset + phase1.5 genuine
// async via deferred host resolve), `useEdgeAcceptHandler` + Tier 2
// + Tier 3 query proxies (followon-E + F).
//
// Dry-mount paths (no Scheduler attached) keep no-op fallbacks so
// `.grx` component bodies that reference these hooks still execute
// without ReferenceError. Component bodies are re-callable functions
// (3a) — `ctx` is the first arg of `(function(ctx){ <body> })`, not
// a global stub. Per-instance ctx (id, props, directory,
// spawnChild/unmount/compose/acceptEdge) is constructed by the
// runtime layer and passed to `gr_jsc_function_call`.

const std = @import("std");
const builtin = @import("builtin");

const c = @cImport({
    @cInclude("JavaScriptCore/JavaScriptCore.h");
});

// Status codes — must match `crates/graphreact-engine-jsc/src/ffi.rs`.
pub const GR_OK: i32 = 0;
pub const GR_ERR_NOT_INITIALIZED: i32 = -1;
pub const GR_ERR_INVALID_ISOLATE: i32 = -2;
pub const GR_ERR_INVALID_MODULE: i32 = -3;
pub const GR_ERR_INVALID_FUNCTION: i32 = -4;
pub const GR_ERR_CALLBACK_DUPLICATE: i32 = -5;
pub const GR_ERR_CALLBACK_NOT_FOUND: i32 = -6;
pub const GR_ERR_BUFFER_TOO_SMALL: i32 = -7;
pub const GR_ERR_INTERNAL: i32 = -99;

const ENGINE_NAME: []const u8 = "jsc";
const ENGINE_VERSION: []const u8 = "jsc-0.1.0-m12.3b-phase1.5";

// Buffer size used by the JSC trampoline when calling back into the
// Rust host. Large enough to hold typical hook return values
// (numbers, short strings, small objects) without retry. On
// `GR_ERR_BUFFER_TOO_SMALL` we resize once up to `MAX_HOST_RESULT`;
// beyond that the call surfaces as a JS exception.
const HOST_RESULT_INITIAL: usize = 4096;
const HOST_RESULT_MAX: usize = 1 << 20;

// Hook prelude: installed via JSEvaluateScript at isolate-create
// time. Provides stubs for the GraphReact runtime hooks so `.grx`
// component bodies that freely reference `useState`, `useEmit`, etc.
// execute without ReferenceError.
//
// **ctx changed in phase 3a** — was a global stub, is now the first
// argument of the component function `(function(ctx){ <body> })`. The
// runtime layer constructs the per-instance ctx (id, props, directory,
// spawnChild/unmount/compose/acceptEdge) and passes it as the call
// argument via `gr_jsc_function_call`.
//
// **useState became real in phase 3b** — delegates to `host_state` /
// `host_state_set` callbacks registered by the runtime layer. State
// is preserved across renders by graphreact-core's hook slots (see
// `crates/graphreact-cli/src/jsruntime.rs`). The other hooks remain
// no-op stubs; phase 3c/d/e swap them in turn.
//
// **Lifecycle dependency:** `host_state` / `host_state_set` are
// expected to be registered on the isolate before any component body
// runs. The runtime layer's `mount_with_scheduler` registers them
// before `eval_component`, so the prelude can reference them. If a
// caller skips registration (e.g. legacy `dry_mount_jsc` callers
// using `eval_component` without scheduler integration), useState
// degrades by throwing a ReferenceError at call time — caught by the
// component body's exception handler.
const HOOK_PRELUDE: []const u8 =
    // JS-side callback registry — token-addressed JS function refs
    // for hooks whose callbacks need to fire LATER (useEffect,
    // useMessage, async cleanups). `__invokeCallback` returns a
    // {kind, value?, token?} envelope so Rust can detect "the
    // callback returned a function" and register it as cleanup.
    \\globalThis.__hostCallbacks = {};
    \\globalThis.__nextCallbackToken = 1;
    \\// M12.3a-followon-D: every token minted is also reported to the
    \\// host via `host_track_token(t)` so the runtime can sweep them on
    \\// node unmount via `__releaseTokens([...])`. The host-side
    \\// callback no-ops when CURRENT_NODE_ID isn't set (e.g. during
    \\// effect-fire / cleanup-fire where the cleanup-token is
    \\// auto-released after invocation), so cleanup tokens don't get
    \\// double-tracked.
    \\globalThis.__registerCallback = function(fn) {
    \\  var t = __nextCallbackToken++;
    \\  __hostCallbacks[t] = fn;
    \\  if (typeof host_track_token !== 'undefined') {
    \\    host_track_token(t);
    \\  }
    \\  return t;
    \\};
    \\globalThis.__releaseCallback = function(t) { delete __hostCallbacks[t]; };
    \\globalThis.__releaseTokens = function(arr) {
    \\  if (!arr) return;
    \\  for (var i = 0; i < arr.length; i++) {
    \\    delete __hostCallbacks[arr[i]];
    \\  }
    \\};
    \\globalThis.__invokeCallback = function(t, args) {
    \\  var fn = __hostCallbacks[t];
    \\  if (!fn) return { kind: 'missing' };
    \\  try {
    \\    var r = fn.apply(null, args || []);
    \\    if (typeof r === 'function') {
    \\      var ct = __registerCallback(r);
    \\      return { kind: 'cleanup', token: ct };
    \\    }
    \\    return { kind: 'value', value: (r === undefined ? null : r) };
    \\  } catch (e) {
    \\    return { kind: 'throw', message: String(e && e.message ? e.message : e) };
    \\  }
    \\};
    \\
    \\// --- Graph-state JS proxies (phase 3e-δ) ---
    \\// Read-only accessors backed by host_directory_* / host_graph_*
    \\// callbacks. Proxies are stable singletons; ctx.directory is set
    \\// by __augmentCtx on every body invocation.
    \\globalThis.__directoryProxy = {
    \\  findById: function(id) {
    \\    if (typeof host_directory_find_by_id === 'undefined') return null;
    \\    return host_directory_find_by_id(id);
    \\  },
    \\  findByRole: function(role) {
    \\    if (typeof host_directory_find_by_role === 'undefined') return [];
    \\    return host_directory_find_by_role(role) || [];
    \\  },
    \\  findByDepth: function(depth) {
    \\    if (typeof host_directory_find_by_depth === 'undefined') return [];
    \\    return host_directory_find_by_depth(depth) || [];
    \\  },
    \\  findInSubtree: function(ancestorId) {
    \\    if (typeof host_directory_find_in_subtree === 'undefined') return [];
    \\    return host_directory_find_in_subtree(ancestorId) || [];
    \\  },
    \\  isAncestor: function(ancestorId, descendantId) {
    \\    if (typeof host_directory_is_ancestor === 'undefined') return false;
    \\    return host_directory_is_ancestor(ancestorId, descendantId);
    \\  }
    \\};
    \\// Tier 1 = the live `CommunicationGraph` (M12.3a-followon-C lift
    \\// to live `Rc<RefCell<_>>`). All 11 host_graph_* accessors below
    \\// are Tier 1 by definition; `g.tier1()` returns this same proxy
    \\// for explicit selection.
    \\globalThis.__graphQueryProxy = {
    \\  neighbors: function(id) {
    \\    if (typeof host_graph_neighbors === 'undefined') return [];
    \\    return host_graph_neighbors(id) || [];
    \\  },
    \\  hasNode: function(id) {
    \\    if (typeof host_graph_has_node === 'undefined') return false;
    \\    return host_graph_has_node(id);
    \\  },
    \\  hasEdge: function(u, v) {
    \\    if (typeof host_graph_has_edge === 'undefined') return false;
    \\    return host_graph_has_edge(u, v);
    \\  },
    \\  nodeCount: function() {
    \\    if (typeof host_graph_node_count === 'undefined') return 0;
    \\    return host_graph_node_count();
    \\  },
    \\  // M12.3a-followon-B: algorithm surface delegating to
    \\  // graphreact-algorithms via host_graph_* callbacks. Each
    \\  // returns a "neutral" value (null / [] / {hasCycle:false})
    \\  // when its callback is missing or the underlying call errors
    \\  // — JS callers shouldn't see thrown exceptions for
    \\  // "no path / unreachable / missing node" cases.
    \\  shortestPath: function(from, to) {
    \\    if (typeof host_graph_shortest_path === 'undefined') return null;
    \\    return host_graph_shortest_path(from, to);
    \\  },
    \\  allPaths: function(from, to, maxDepth) {
    \\    if (typeof host_graph_all_paths === 'undefined') return [];
    \\    return host_graph_all_paths(from, to, (typeof maxDepth === 'number') ? maxDepth : null) || [];
    \\  },
    \\  aStarPath: function(from, to) {
    \\    if (typeof host_graph_a_star_path === 'undefined') return null;
    \\    return host_graph_a_star_path(from, to);
    \\  },
    \\  bfs: function(from, depthLimit) {
    \\    if (typeof host_graph_bfs === 'undefined') return [];
    \\    return host_graph_bfs(from, (typeof depthLimit === 'number') ? depthLimit : null) || [];
    \\  },
    \\  dfs: function(from, depthLimit) {
    \\    if (typeof host_graph_dfs === 'undefined') return [];
    \\    return host_graph_dfs(from, (typeof depthLimit === 'number') ? depthLimit : null) || [];
    \\  },
    \\  connectedComponents: function() {
    \\    if (typeof host_graph_connected_components === 'undefined') return [];
    \\    return host_graph_connected_components() || [];
    \\  },
    \\  cycleDetection: function() {
    \\    if (typeof host_graph_cycle_detection === 'undefined') {
    \\      return { hasCycle: false, cycle: null };
    \\    }
    \\    return host_graph_cycle_detection();
    \\  },
    \\  // M12.3a-followon-E: tier selectors. `g.tier1()` returns this
    \\  // same proxy (Tier 1 = live `CommunicationGraph`); `g.tier2(T,
    \\  // opts?)` returns a sub-proxy projecting the directed graph
    \\  // derived from `PermissionManifest.emits/accepts` pairs for
    \\  // message type T. Tier 2 closes followon-B's `topologicalSort`
    \\  // deferral and exposes 5 built-in cost-from-policy A*
    \\  // heuristics.
    \\  //
    \\  // M12.3a-followon-F: `g.tier3(T, opts?)` returns a sub-proxy
    \\  // over Tier 2 ∪ {(s, u) where target u's edge-accept predicate
    \\  // accepts (s, T)}. Predicates are installed by
    \\  // `useEdgeAcceptHandler(fn(sourceId, msgType) -> bool)`. Pure
    \\  // dry-run: queries do NOT trigger AcceptEdge mutations. Same
    \\  // 12 accessors + 5 heuristics + 2 flavors as Tier 2.
    \\  //
    \\  // opts.includeDirectoryShortcuts: when true, edges in tier2(T)/
    \\  // tier3(T) are every (s, u) where s.emits T and u.accepts T
    \\  // regardless of Tier 1 edge presence — the canonical
    \\  // "directory-augmented" view from the three-tier possibility
    \\  // model. Default false (structural flavor: requires Tier 1
    \\  // edge AND emit/accept match — preserves both topology AND
    \\  // policy for path-quality queries).
    \\  tier1: function() { return this; },
    \\  tier2: function(msgType, opts) { return __makeTier2Proxy(msgType, opts || {}); },
    \\  tier3: function(msgType, opts) { return __makeTier3Proxy(msgType, opts || {}); }
    \\};
    \\globalThis.__makeTier2Proxy = function(msgType, opts) {
    \\  // Captured per-proxy: every host_tier2_* call appends this as
    \\  // the trailing positional arg so the Rust decoder can branch
    \\  // between the structural and directory-shortcut adjacencies.
    \\  var ds = !!(opts && opts.includeDirectoryShortcuts);
    \\  return {
    \\    nodeCount: function() {
    \\      if (typeof host_tier2_node_count === 'undefined') return 0;
    \\      return host_tier2_node_count(msgType, ds);
    \\    },
    \\    hasNode: function(id) {
    \\      if (typeof host_tier2_has_node === 'undefined') return false;
    \\      return host_tier2_has_node(msgType, id, ds);
    \\    },
    \\    neighbors: function(id) {
    \\      if (typeof host_tier2_neighbors === 'undefined') return [];
    \\      return host_tier2_neighbors(msgType, id, ds) || [];
    \\    },
    \\    hasEdge: function(u, v) {
    \\      if (typeof host_tier2_has_edge === 'undefined') return false;
    \\      return host_tier2_has_edge(msgType, u, v, ds);
    \\    },
    \\    bfs: function(from, depthLimit) {
    \\      if (typeof host_tier2_bfs === 'undefined') return [];
    \\      return host_tier2_bfs(msgType, from, (typeof depthLimit === 'number') ? depthLimit : null, ds) || [];
    \\    },
    \\    dfs: function(from, depthLimit) {
    \\      if (typeof host_tier2_dfs === 'undefined') return [];
    \\      return host_tier2_dfs(msgType, from, (typeof depthLimit === 'number') ? depthLimit : null, ds) || [];
    \\    },
    \\    shortestPath: function(from, to) {
    \\      if (typeof host_tier2_shortest_path === 'undefined') return null;
    \\      return host_tier2_shortest_path(msgType, from, to, ds);
    \\    },
    \\    topologicalSort: function() {
    \\      if (typeof host_tier2_topological_sort === 'undefined') return null;
    \\      return host_tier2_topological_sort(msgType, ds);
    \\    },
    \\    // Cost-from-policy A*: opts.heuristic ∈ {'latency',
    \\    // 'compliance', 'hierarchy', 'trust-attenuated',
    \\    // 'prerequisite-distance'}. Falls back to BFS-equivalent
    \\    // when unrecognised.
    \\    aStarPath: function(from, to, callOpts) {
    \\      if (typeof host_tier2_a_star_path === 'undefined') return null;
    \\      var h = (callOpts && typeof callOpts.heuristic === 'string') ? callOpts.heuristic : null;
    \\      return host_tier2_a_star_path(msgType, from, to, h, ds);
    \\    },
    \\    allPaths: function(from, to, maxDepth) {
    \\      if (typeof host_tier2_all_paths === 'undefined') return [];
    \\      return host_tier2_all_paths(msgType, from, to, (typeof maxDepth === 'number') ? maxDepth : null, ds) || [];
    \\    },
    \\    // Strongly connected components (Tarjan's) over the directed
    \\    // adjacency. Each component returned as a sorted Vec<String>;
    \\    // outer Vec sorted by smallest member.
    \\    connectedComponents: function() {
    \\      if (typeof host_tier2_connected_components === 'undefined') return [];
    \\      return host_tier2_connected_components(msgType, ds) || [];
    \\    },
    \\    // Directed cycle detection via 3-color DFS. Returns
    \\    // {hasCycle, cycle: [[u,v], ...] | null} — same envelope as
    \\    // Tier 1's cycleDetection() for shape continuity.
    \\    cycleDetection: function() {
    \\      if (typeof host_tier2_cycle_detection === 'undefined') {
    \\        return { hasCycle: false, cycle: null };
    \\      }
    \\      return host_tier2_cycle_detection(msgType, ds);
    \\    }
    \\  };
    \\};
    \\// M12.3a-followon-F: Tier 3 proxy. Same shape as Tier 2 but the
    \\// underlying directed adjacency is filtered through each target
    \\// node's edge-accept predicate (installed via
    \\// `useEdgeAcceptHandler`). Predicates run in pure dry-run mode at
    \\// query time — no AcceptEdge mutation is triggered. Nodes
    \\// without an installed predicate default-accept, so Tier 3
    \\// degrades gracefully to Tier 2 when no node explicitly declines
    \\// (Tier 3 ⊇ vetoed-Tier 2; Tier 3 == Tier 2 when no predicates
    \\// installed).
    \\globalThis.__makeTier3Proxy = function(msgType, opts) {
    \\  // Captured per-proxy: every host_tier3_* call appends this as
    \\  // the trailing positional arg, mirroring Tier 2's flag.
    \\  var ds = !!(opts && opts.includeDirectoryShortcuts);
    \\  return {
    \\    nodeCount: function() {
    \\      if (typeof host_tier3_node_count === 'undefined') return 0;
    \\      return host_tier3_node_count(msgType, ds);
    \\    },
    \\    hasNode: function(id) {
    \\      if (typeof host_tier3_has_node === 'undefined') return false;
    \\      return host_tier3_has_node(msgType, id, ds);
    \\    },
    \\    neighbors: function(id) {
    \\      if (typeof host_tier3_neighbors === 'undefined') return [];
    \\      return host_tier3_neighbors(msgType, id, ds) || [];
    \\    },
    \\    hasEdge: function(u, v) {
    \\      if (typeof host_tier3_has_edge === 'undefined') return false;
    \\      return host_tier3_has_edge(msgType, u, v, ds);
    \\    },
    \\    bfs: function(from, depthLimit) {
    \\      if (typeof host_tier3_bfs === 'undefined') return [];
    \\      return host_tier3_bfs(msgType, from, (typeof depthLimit === 'number') ? depthLimit : null, ds) || [];
    \\    },
    \\    dfs: function(from, depthLimit) {
    \\      if (typeof host_tier3_dfs === 'undefined') return [];
    \\      return host_tier3_dfs(msgType, from, (typeof depthLimit === 'number') ? depthLimit : null, ds) || [];
    \\    },
    \\    shortestPath: function(from, to) {
    \\      if (typeof host_tier3_shortest_path === 'undefined') return null;
    \\      return host_tier3_shortest_path(msgType, from, to, ds);
    \\    },
    \\    topologicalSort: function() {
    \\      if (typeof host_tier3_topological_sort === 'undefined') return null;
    \\      return host_tier3_topological_sort(msgType, ds);
    \\    },
    \\    // Cost-from-policy A* — same heuristic set as Tier 2 (latency,
    \\    // compliance, hierarchy, trust-attenuated, prerequisite-distance).
    \\    aStarPath: function(from, to, callOpts) {
    \\      if (typeof host_tier3_a_star_path === 'undefined') return null;
    \\      var h = (callOpts && typeof callOpts.heuristic === 'string') ? callOpts.heuristic : null;
    \\      return host_tier3_a_star_path(msgType, from, to, h, ds);
    \\    },
    \\    allPaths: function(from, to, maxDepth) {
    \\      if (typeof host_tier3_all_paths === 'undefined') return [];
    \\      return host_tier3_all_paths(msgType, from, to, (typeof maxDepth === 'number') ? maxDepth : null, ds) || [];
    \\    },
    \\    connectedComponents: function() {
    \\      if (typeof host_tier3_connected_components === 'undefined') return [];
    \\      return host_tier3_connected_components(msgType, ds) || [];
    \\    },
    \\    cycleDetection: function() {
    \\      if (typeof host_tier3_cycle_detection === 'undefined') {
    \\        return { hasCycle: false, cycle: null };
    \\      }
    \\      return host_tier3_cycle_detection(msgType, ds);
    \\    }
    \\  };
    \\};
    \\// Augment a raw ctx object with JS-side proxies that can't be
    \\// marshalled through the JSON FFI. Called once per body
    \\// invocation by the wrap that gr_jsc_component_register installs;
    \\// also called by the JS-side spawnChild/compose wrappers so
    \\// dynamically-defined component bodies see the same surface.
    \\//
    \\// M6.6 method-style sibling API: ctx.spawnChild / ctx.compose /
    \\// ctx.unmount / ctx.acceptEdge are thin delegations to the
    \\// useX() hook accessors. They exist for ergonomic parity with
    \\// React-style "ctx-as-prop" bodies; mechanically identical to
    \\// the hook-style accessors (same HOOK_CTX, same host callbacks).
    \\globalThis.__augmentCtx = function(rawCtx) {
    \\  if (!rawCtx || typeof rawCtx !== 'object') return rawCtx;
    \\  rawCtx.directory = __directoryProxy;
    \\  rawCtx.spawnChild = function(spec) {
    \\    if (typeof useSpawnChild === 'undefined') return;
    \\    return useSpawnChild()(spec);
    \\  };
    \\  rawCtx.compose = function(specs) {
    \\    if (typeof useCompose === 'undefined') return;
    \\    return useCompose()(specs);
    \\  };
    \\  rawCtx.unmount = function() {
    \\    if (typeof useUnmountSelf === 'undefined') return;
    \\    return useUnmountSelf()();
    \\  };
    \\  rawCtx.acceptEdge = function(targetId) {
    \\    if (typeof useAcceptEdge === 'undefined') return;
    \\    return useAcceptEdge()(targetId);
    \\  };
    \\  // M-Prod sub-track #3 Phase B Part 1: ctx.secret(name).
    \\  // Resolves to the registered host_secret BoxCallback result
    \\  // (UTF-8 string) when the active component's manifest
    \\  // needs_secret allowlist contains the name; default-deny
    \\  // returns null. See ADR-0007.
    \\  rawCtx.secret = function(name) {
    \\    if (typeof host_secret === 'undefined') return null;
    \\    if (typeof name !== 'string') return null;
    \\    return host_secret(name);
    \\  };
    \\  return rawCtx;
    \\};
    \\
    \\globalThis.useState = function(initial) {
    \\  if (typeof host_state === 'undefined') return [initial, function() {}];
    \\  var r = host_state(initial);
    \\  return [r.value, function(v) { host_state_set(r.id, v); }];
    \\};
    \\globalThis.useEmit = function() {
    \\  return function(type, payload, opts) {
    \\    if (typeof host_emit === 'undefined') return;
    \\    host_emit(
    \\      type,
    \\      payload === undefined ? null : payload,
    \\      opts === undefined ? null : opts
    \\    );
    \\  };
    \\};
    \\globalThis.useMessage = function(type, handler) {
    \\  if (typeof host_message === 'undefined') return;
    \\  if (typeof handler !== 'function') return;
    \\  var token = __registerCallback(handler);
    \\  host_message(type, token);
    \\};
    \\globalThis.useEffect = function(fn, deps) {
    \\  if (typeof host_effect === 'undefined') return;
    \\  var token = __registerCallback(fn);
    \\  host_effect(token, deps || []);
    \\};
    \\globalThis.useMemo = function(fn, deps) {
    \\  if (typeof host_memo_check === 'undefined') {
    \\    try { return fn(); } catch (e) { return undefined; }
    \\  }
    \\  var checked = host_memo_check(deps || []);
    \\  if (checked && checked.cached) return checked.value;
    \\  var result;
    \\  try { result = fn(); } catch (e) { result = undefined; }
    \\  host_memo_store(result === undefined ? null : result);
    \\  return result;
    \\};
    \\globalThis.useCallback = function(fn, deps) {
    \\  if (typeof host_memo_check === 'undefined') return fn;
    \\  var checked = host_memo_check(deps || []);
    \\  if (checked && checked.cached) {
    \\    var prevToken = checked.value;
    \\    if (prevToken !== null && __hostCallbacks[prevToken]) {
    \\      return __hostCallbacks[prevToken];
    \\    }
    \\  }
    \\  if (checked && checked.previous !== undefined && checked.previous !== null) {
    \\    __releaseCallback(checked.previous);
    \\  }
    \\  var token = __registerCallback(fn);
    \\  host_memo_store(token);
    \\  return fn;
    \\};
    \\globalThis.useReducer = function(reducer, initial) {
    \\  var pair = useState(initial);
    \\  var state = pair[0];
    \\  var setState = pair[1];
    \\  return [state, function(action) { setState(reducer(state, action)); }];
    \\};
    \\globalThis.useNeighbors = function(filter) {
    \\  if (typeof host_neighbors === 'undefined') return [];
    \\  var all = host_neighbors() || [];
    \\  if (typeof filter === 'function') {
    \\    return all.filter(filter);
    \\  }
    \\  return all;
    \\};
    \\globalThis.useGraphQuery = function(queryFn, deps) {
    \\  // Wraps useMemo over a graph-accessor proxy. The proxy itself
    \\  // (`__graphQueryProxy`) is populated by phase 3e-δ; until then
    \\  // queries safely degrade to `null` (the proxy is undefined).
    \\  return useMemo(function() {
    \\    if (typeof __graphQueryProxy === 'undefined') return null;
    \\    try { return queryFn(__graphQueryProxy); } catch (e) { return null; }
    \\  }, deps || []);
    \\};
    \\globalThis.useTopology = function(handler, filter) {
    \\  if (typeof host_topology === 'undefined') return;
    \\  if (typeof handler !== 'function') return;
    \\  var token = __registerCallback(handler);
    \\  host_topology(token, filter === undefined ? null : filter);
    \\};
    \\globalThis.useSource = function(factoryFn, deps) {
    \\  if (typeof host_use_source === 'undefined') return;
    \\  if (typeof factoryFn !== 'function') return;
    \\  var descriptor;
    \\  try { descriptor = factoryFn(); } catch (e) { return; }
    \\  if (!descriptor || typeof descriptor !== 'object') return;
    \\  host_use_source(descriptor, deps || []);
    \\};
    \\globalThis.timerSource = function(intervalMs) {
    \\  return { kind: 'timer', intervalMs: intervalMs };
    \\};
    \\globalThis.delaySource = function(delayMs) {
    \\  return { kind: 'delay', delayMs: delayMs };
    \\};
    \\// httpSource + watchSource: descriptor factories. Rust-side
    \\// firing landed in M12.3a-followon-A (2026-05-02) — `tokio::net`
    \\// for http + `notify` for watch are now real in
    \\// `crates/graphreact-cli/src/sources.rs`, dispatched by
    \\// `host_use_source`'s `"http"` / `"watch"` arms. End-to-end on
    \\// `grx run --runtime=jsc`.
    \\globalThis.httpSource = function(opts) {
    \\  return { kind: 'http', opts: opts || {} };
    \\};
    \\globalThis.watchSource = function(opts) {
    \\  return { kind: 'watch', opts: opts || {} };
    \\};
    \\globalThis.useAcceptEdge = function() {
    \\  return function(target) {
    \\    if (typeof host_accept_edge === 'undefined') return;
    \\    host_accept_edge(target);
    \\  };
    \\};
    \\// M12.3a-followon-F: install the active node's edge-accept
    \\// predicate. Called from the body with a function
    \\// `(sourceId, msgType) -> bool`. The runtime invokes the
    \\// predicate from two paths:
    \\//   * `drain_pending_mutations::AcceptEdge` (Phase 9 mutation)
    \\//     — vetoes the edge if the predicate returns false. Phase 9
    \\//     is channel-shaped so msgType is the empty string here;
    \\//     predicates that care about msgType-specific decisions
    \\//     should consult Tier 3 queries instead.
    \\//   * Tier 3 graph queries (`g.tier3(msgType)`) — restricts the
    \\//     directed reachability set to (s, u) pairs the target u
    \\//     would accept.
    \\// Re-installed every render; conditional hook calls revert to
    \\// default-accept on the next render where the call is omitted.
    \\globalThis.useEdgeAcceptHandler = function(predicate) {
    \\  if (typeof host_edge_accept_handler === 'undefined') return;
    \\  if (typeof predicate !== 'function') return;
    \\  var token = __registerCallback(predicate);
    \\  host_edge_accept_handler(token);
    \\};
    \\// --- Supervision hooks (phase 3e-ζ) ---
    \\globalThis.useMonitor = function(targetId) {
    \\  if (typeof host_monitor === 'undefined') return null;
    \\  return host_monitor(targetId);
    \\};
    \\globalThis.useDemonitor = function(mref) {
    \\  if (typeof host_demonitor === 'undefined') return;
    \\  if (!mref || typeof mref.id !== 'number') return;
    \\  host_demonitor(mref.id);
    \\};
    \\globalThis.useLink = function(targetId) {
    \\  if (typeof host_link === 'undefined') return false;
    \\  return host_link(targetId) === true;
    \\};
    \\globalThis.useUnlink = function(targetId) {
    \\  if (typeof host_unlink === 'undefined') return;
    \\  host_unlink(targetId);
    \\};
    \\globalThis.useGroupJoin = function(group) {
    \\  if (typeof host_group_join === 'undefined') return;
    \\  host_group_join(group);
    \\};
    \\globalThis.useGroupLeave = function(group) {
    \\  if (typeof host_group_leave === 'undefined') return;
    \\  host_group_leave(group);
    \\};
    \\// M12.3b-phase1: useEffectAsync — real on macOS via JS Promise →
    \\// Rust Future bridge. The body wraps the user's asyncFn in
    \\// Promise.resolve().then(...) and reports settlement via
    \\// host_effect_async_complete(completionId, kind, payload). On
    \\// the Rust side, host_effect_async parks a one-shot Sender keyed
    \\// by completionId and awaits the receiver from
    \\// flush_effects_async.
    \\//
    \\// For asyncFns that resolve synchronously (no real I/O),
    \\// JSC's auto-drain at the JSObjectCallAsFunction boundary
    \\// processes the then-chain inside the same invoke_global call —
    \\// the Sender is signaled before the Rust .await observes it.
    \\// For genuine async I/O (real setTimeout / fetch / etc.) the
    \\// promise chain only resolves when something else triggers a
    \\// JSC microtask drain, which the macOS public C API doesn't
    \\// expose. M12.3b-phase4 (vendored WebKit + JSC::VM::drainMicrotasks
    \\// C++ shim) lifts that constraint cross-platform.
    \\globalThis.useEffectAsync = function(asyncFn, deps) {
    \\  if (typeof host_effect_async === 'undefined') return;
    \\  if (typeof asyncFn !== 'function') return;
    \\  var bodyToken = __registerCallback(function(completionId) {
    \\    Promise.resolve().then(function() { return asyncFn(); }).then(
    \\      function(result) {
    \\        if (typeof result === 'function') {
    \\          host_effect_async_complete(completionId, 'cleanup', __registerCallback(result));
    \\        } else {
    \\          host_effect_async_complete(completionId, 'value', 0);
    \\        }
    \\      },
    \\      function(err) {
    \\        host_effect_async_complete(completionId, 'throw', String(err));
    \\      }
    \\    );
    \\  });
    \\  host_effect_async(bodyToken, deps || []);
    \\};
    \\globalThis.useSpawnChild = function() {
    \\  return function(spec) {
    \\    if (typeof host_spawn_child === 'undefined') return;
    \\    if (!spec || typeof spec.component !== 'function') return;
    \\    var rawComponent = spec.component;
    \\    // Wrap so child bodies receive augmented ctx (directory etc.)
    \\    var wrappedComponent = function(rawCtx) {
    \\      return rawComponent(__augmentCtx(rawCtx));
    \\    };
    \\    var token = __registerCallback(wrappedComponent);
    \\    host_spawn_child({
    \\      id: spec.id,
    \\      componentToken: token,
    \\      props: spec.props || {},
    \\      connectTo: spec.connectTo || []
    \\    });
    \\  };
    \\};
    \\globalThis.useUnmountSelf = function() {
    \\  return function() {
    \\    if (typeof host_unmount_self === 'undefined') return;
    \\    host_unmount_self();
    \\  };
    \\};
    \\globalThis.useCompose = function() {
    \\  return function(specs) {
    \\    if (typeof host_compose === 'undefined') return;
    \\    if (!Array.isArray(specs)) return;
    \\    var marshalled = [];
    \\    for (var i = 0; i < specs.length; i++) {
    \\      var s = specs[i];
    \\      if (!s || typeof s.component !== 'function') continue;
    \\      var rawComponent = s.component;
    \\      var wrappedComponent = (function(fn) {
    \\        return function(rawCtx) { return fn(__augmentCtx(rawCtx)); };
    \\      })(rawComponent);
    \\      marshalled.push({
    \\        id: s.id,
    \\        componentToken: __registerCallback(wrappedComponent),
    \\        props: s.props || {},
    \\        connectTo: s.connectTo || []
    \\      });
    \\    }
    \\    host_compose(marshalled);
    \\  };
    \\};
;

// ---------------------------------------------------------------------
// Per-isolate state
// ---------------------------------------------------------------------

const ModuleEntry = struct {
    handle: u64,
    /// Strong-protected JSValueRef (the manifest object returned by
    /// the IIFE-wrapped component body). Released on dispose.
    value: c.JSValueRef,
};

const FunctionEntry = struct {
    handle: u64,
    /// Strong-protected JSValueRef pointing at a JS function (from
    /// JSObjectGetProperty on a manifest, e.g. `manifest.init`).
    value: c.JSValueRef,
};

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

/// Per-callback entry carried as JSC private data. The trampoline
/// reads this off the function object's private-data slot to figure
/// out which Rust closure to dispatch to. `host_data` is whatever
/// Rust passed at registration time and is forwarded back verbatim;
/// the Rust side uses it as a registry key into its global closure
/// map.
const CallbackEntry = struct {
    id: u64,
    name: []u8,
    host_fn: HostCallbackFn,
    host_data: ?*anyopaque,
};

const Isolate = struct {
    handle: u64,
    /// JSC global context for this isolate. Released on dispose.
    context: c.JSGlobalContextRef,
    /// True once HOOK_PRELUDE has been evaluated on this context.
    /// Re-evaluating the prelude is harmless (it just re-binds
    /// globals), but skipping it is cheaper.
    prelude_loaded: bool,
    modules: std.array_list.Managed(ModuleEntry),
    functions: std.array_list.Managed(FunctionEntry),
    /// Heap-allocated callback entries kept alive for the lifetime of
    /// the isolate. JSC's GC frees the function objects when the
    /// context dies; we free the private-data structs here on
    /// dispose. (The class def's `finalize` callback would also
    /// work, but JSC may invoke finalizers asynchronously after the
    /// context is released — deterministic per-isolate cleanup is
    /// simpler.)
    callbacks: std.array_list.Managed(*CallbackEntry),
    next_module: u64,
    next_function: u64,
    next_callback: u64,
};

const State = struct {
    initialized: bool,
    context_group: c.JSContextGroupRef,
    isolates: std.array_list.Managed(?*Isolate),
    next_handle: u64,
    /// Lazily-created class def for host callback objects. Shared
    /// across all isolates (the class def is process-global; private
    /// data per-instance is what carries identity). Released on
    /// `gr_jsc_engine_shutdown`.
    callback_class: c.JSClassRef,
};

var gpa_state: std.heap.DebugAllocator(.{}) = .{};
var allocator: std.mem.Allocator = undefined;
var state: State = .{
    .initialized = false,
    .context_group = null,
    .isolates = undefined,
    .next_handle = 1,
    .callback_class = null,
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

fn moduleValue(iso: *Isolate, module: u64) ?c.JSValueRef {
    for (iso.modules.items) |entry| {
        if (entry.handle == module) return entry.value;
    }
    return null;
}

fn functionValue(iso: *Isolate, function: u64) ?c.JSValueRef {
    for (iso.functions.items) |entry| {
        if (entry.handle == function) return entry.value;
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

// ---------------------------------------------------------------------
// JSStringRef <-> Zig conversion helpers
// ---------------------------------------------------------------------

/// Zig string → JSStringRef. Caller must `JSStringRelease`. Returns
/// null on allocation failure.
fn zigToJsString(text: []const u8) ?c.JSStringRef {
    // JSC's UTF-8 constructor expects a null-terminated C string.
    // Allocate a temporary buffer to add the terminator.
    const buf = allocator.alloc(u8, text.len + 1) catch return null;
    defer allocator.free(buf);
    @memcpy(buf[0..text.len], text);
    buf[text.len] = 0;
    return c.JSStringCreateWithUTF8CString(@ptrCast(buf.ptr));
}

/// JSStringRef → owned Zig slice (allocated). Caller frees with
/// `allocator.free`.
fn jsStringToOwned(js_str: c.JSStringRef) ?[]u8 {
    if (js_str == null) return null;
    const max_size = c.JSStringGetMaximumUTF8CStringSize(js_str);
    if (max_size == 0) {
        return allocator.alloc(u8, 0) catch null;
    }
    const buf = allocator.alloc(u8, max_size) catch return null;
    const written = c.JSStringGetUTF8CString(js_str, @ptrCast(buf.ptr), max_size);
    // `written` includes the null terminator; trim it.
    if (written == 0) {
        allocator.free(buf);
        return allocator.alloc(u8, 0) catch null;
    }
    const actual = written - 1;
    return allocator.realloc(buf, actual) catch buf[0..actual];
}

/// JSValueRef containing a string → owned Zig slice.
fn jsValueToOwnedString(ctx: c.JSGlobalContextRef, value: c.JSValueRef) ?[]u8 {
    var exception: c.JSValueRef = null;
    const js_str = c.JSValueToStringCopy(ctx, value, &exception);
    if (js_str == null) return null;
    defer c.JSStringRelease(js_str);
    return jsStringToOwned(js_str);
}

// ---------------------------------------------------------------------
// Engine-global lifecycle
// ---------------------------------------------------------------------

export fn gr_jsc_engine_initialize() i32 {
    if (state.initialized) return GR_OK;
    allocator = gpa_state.allocator();
    state.context_group = c.JSContextGroupCreate();
    if (state.context_group == null) return GR_ERR_INTERNAL;
    state.isolates = std.array_list.Managed(?*Isolate).init(allocator);
    state.initialized = true;
    state.next_handle = 1;
    return GR_OK;
}

export fn gr_jsc_engine_shutdown() i32 {
    if (!state.initialized) return GR_OK;
    for (state.isolates.items) |slot| {
        if (slot) |iso| disposeIsolate(iso);
    }
    state.isolates.deinit();
    if (state.callback_class != null) {
        c.JSClassRelease(state.callback_class);
        state.callback_class = null;
    }
    if (state.context_group != null) {
        c.JSContextGroupRelease(state.context_group);
        state.context_group = null;
    }
    state.initialized = false;
    return GR_OK;
}

export fn gr_jsc_engine_name(out_buf: ?[*]u8, out_len: ?*usize) i32 {
    return writeStringOut(ENGINE_NAME, out_buf, out_len);
}

export fn gr_jsc_engine_version(out_buf: ?[*]u8, out_len: ?*usize) i32 {
    return writeStringOut(ENGINE_VERSION, out_buf, out_len);
}

export fn gr_jsc_engine_live_isolates() u64 {
    if (!state.initialized) return 0;
    var n: u64 = 0;
    for (state.isolates.items) |slot| {
        if (slot != null) n += 1;
    }
    return n;
}

// ---------------------------------------------------------------------
// Isolate lifecycle
// ---------------------------------------------------------------------

fn disposeIsolate(iso: *Isolate) void {
    // Unprotect every stored value so JSC's GC can reclaim them
    // before the context itself goes away.
    if (iso.context != null) {
        for (iso.modules.items) |entry| {
            c.JSValueUnprotect(iso.context, entry.value);
        }
        for (iso.functions.items) |entry| {
            c.JSValueUnprotect(iso.context, entry.value);
        }
    }
    iso.modules.deinit();
    iso.functions.deinit();
    // Free callback entries. The JS function objects themselves are
    // owned by the global context and die with it; we just need to
    // free the heap-allocated private-data structs.
    for (iso.callbacks.items) |entry| {
        allocator.free(entry.name);
        allocator.destroy(entry);
    }
    iso.callbacks.deinit();
    if (iso.context != null) {
        c.JSGlobalContextRelease(iso.context);
        iso.context = null;
    }
    allocator.destroy(iso);
}

export fn gr_jsc_isolate_create(out_handle: ?*u64) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const out = out_handle orelse return GR_ERR_INTERNAL;

    const ctx = c.JSGlobalContextCreateInGroup(state.context_group, null);
    if (ctx == null) return GR_ERR_INTERNAL;

    const iso = allocator.create(Isolate) catch {
        c.JSGlobalContextRelease(ctx);
        return GR_ERR_INTERNAL;
    };
    iso.* = .{
        .handle = state.next_handle,
        .context = ctx,
        .prelude_loaded = false,
        .modules = std.array_list.Managed(ModuleEntry).init(allocator),
        .functions = std.array_list.Managed(FunctionEntry).init(allocator),
        .callbacks = std.array_list.Managed(*CallbackEntry).init(allocator),
        .next_module = 1,
        .next_function = 1,
        .next_callback = 1,
    };
    state.next_handle += 1;
    state.isolates.append(iso) catch {
        c.JSGlobalContextRelease(ctx);
        allocator.destroy(iso);
        return GR_ERR_INTERNAL;
    };
    out.* = iso.handle;
    return GR_OK;
}

export fn gr_jsc_isolate_dispose(handle: u64) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    var i: usize = 0;
    while (i < state.isolates.items.len) : (i += 1) {
        if (state.isolates.items[i]) |iso| {
            if (iso.handle == handle) {
                disposeIsolate(iso);
                state.isolates.items[i] = null;
                return GR_OK;
            }
        }
    }
    return GR_ERR_INVALID_ISOLATE;
}

// ---------------------------------------------------------------------
// Hook-prelude installation
// ---------------------------------------------------------------------

fn ensurePrelude(iso: *Isolate) i32 {
    if (iso.prelude_loaded) return GR_OK;
    const js_src = zigToJsString(HOOK_PRELUDE) orelse return GR_ERR_INTERNAL;
    defer c.JSStringRelease(js_src);
    var exception: c.JSValueRef = null;
    const result = c.JSEvaluateScript(iso.context, js_src, null, null, 0, &exception);
    if (exception != null) {
        // Prelude failed to evaluate — should never happen unless
        // JSC itself is broken.
        return GR_ERR_INTERNAL;
    }
    _ = result;
    iso.prelude_loaded = true;
    return GR_OK;
}

// ---------------------------------------------------------------------
// Module evaluation (real)
// ---------------------------------------------------------------------

/// Compile + evaluate a component body and store the resulting
/// manifest as a strong reference. The body is wrapped in an IIFE so
/// `return ...` at the top is valid:
///
///     (function(){ <body> })()
///
/// The IIFE's return value (typically the component's manifest
/// `{ accepts, emits, init, ... }`) is what we store.
export fn gr_jsc_module_register(
    isolate: u64,
    module_name: ?[*]const u8,
    module_name_len: usize,
    source: ?[*]const u8,
    source_len: usize,
    out_module: ?*u64,
) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const out = out_module orelse return GR_ERR_INTERNAL;

    if (ensurePrelude(iso) != GR_OK) return GR_ERR_INTERNAL;

    // Slice source bytes safely.
    const source_slice: []const u8 = if (source) |p| p[0..source_len] else &[_]u8{};
    const name_slice: []const u8 = if (module_name) |p| p[0..module_name_len] else &[_]u8{};
    _ = name_slice;

    // Wrap in IIFE. `{{ ... }}` are literal braces in Zig fmt strings.
    const wrapped = std.fmt.allocPrint(
        allocator,
        "(function(){{ {s} }})()",
        .{source_slice},
    ) catch return GR_ERR_INTERNAL;
    defer allocator.free(wrapped);

    const js_src = zigToJsString(wrapped) orelse return GR_ERR_INTERNAL;
    defer c.JSStringRelease(js_src);

    var exception: c.JSValueRef = null;
    const value = c.JSEvaluateScript(iso.context, js_src, null, null, 0, &exception);
    if (exception != null) {
        return GR_ERR_INTERNAL;
    }
    if (value == null) return GR_ERR_INTERNAL;

    c.JSValueProtect(iso.context, value);
    iso.modules.append(.{ .handle = iso.next_module, .value = value }) catch {
        c.JSValueUnprotect(iso.context, value);
        return GR_ERR_INTERNAL;
    };
    out.* = iso.next_module;
    iso.next_module += 1;
    return GR_OK;
}

// ---------------------------------------------------------------------
// Component registration (M12.3a-phase3a)
// ---------------------------------------------------------------------
//
// Diverges from `gr_jsc_module_register` (which wraps as IIFE → invokes
// once → stores the manifest). For phase-3 hook semantics the body
// must be re-callable across renders so `useState` etc. fire on every
// evaluation. We wrap as `(function(ctx){ <body> })` (function
// expression, not invoked), evaluate it to get a JS function value,
// and store that value in the per-isolate functions table — meaning
// the existing `gr_jsc_function_call` drives it without changes.
//
// Caller invokes via `gr_jsc_function_call(iso, returned_handle,
// args_json="[ctx_json]")` per render.

export fn gr_jsc_component_register(
    isolate: u64,
    component_name: ?[*]const u8,
    component_name_len: usize,
    source: ?[*]const u8,
    source_len: usize,
    out_function: ?*u64,
) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const out = out_function orelse return GR_ERR_INTERNAL;

    if (ensurePrelude(iso) != GR_OK) return GR_ERR_INTERNAL;

    const source_slice: []const u8 = if (source) |p| p[0..source_len] else &[_]u8{};
    const name_slice: []const u8 = if (component_name) |p| p[0..component_name_len] else &[_]u8{};
    _ = name_slice; // reserved for source-map / error-formatting (phase 3b)

    // Wrap as function expression (parenthesised so the parser treats
    // it as expression, not declaration). `{{ ... }}` are literal
    // braces in Zig fmt strings; `{s}` is the substitution.
    //
    // The body's `ctx` parameter is run through `__augmentCtx`
    // (defined in HOOK_PRELUDE, phase 3e-δ) so JS-side proxies like
    // `ctx.directory` are populated without needing per-call HostValue
    // marshalling. Backwards-compat: pre-3e-δ isolates without
    // `__augmentCtx` get the raw ctx unchanged.
    const wrapped = std.fmt.allocPrint(
        allocator,
        "(function(__rawCtx){{ var ctx = (typeof __augmentCtx === 'function') ? __augmentCtx(__rawCtx) : __rawCtx; {s} }})",
        .{source_slice},
    ) catch return GR_ERR_INTERNAL;
    defer allocator.free(wrapped);

    const js_src = zigToJsString(wrapped) orelse return GR_ERR_INTERNAL;
    defer c.JSStringRelease(js_src);

    var exception: c.JSValueRef = null;
    const value = c.JSEvaluateScript(iso.context, js_src, null, null, 0, &exception);
    if (exception != null) return GR_ERR_INTERNAL;
    if (value == null) return GR_ERR_INTERNAL;
    if (!c.JSValueIsObject(iso.context, value)) return GR_ERR_INVALID_FUNCTION;

    const fn_obj = c.JSValueToObject(iso.context, value, &exception);
    if (exception != null or fn_obj == null) return GR_ERR_INVALID_FUNCTION;
    if (!c.JSObjectIsFunction(iso.context, fn_obj)) return GR_ERR_INVALID_FUNCTION;

    c.JSValueProtect(iso.context, value);
    iso.functions.append(.{ .handle = iso.next_function, .value = value }) catch {
        c.JSValueUnprotect(iso.context, value);
        return GR_ERR_INTERNAL;
    };
    out.* = iso.next_function;
    iso.next_function += 1;
    return GR_OK;
}

// ---------------------------------------------------------------------
// Function resolution + invocation (real)
// ---------------------------------------------------------------------

/// Look up a named property on a stored module's manifest. Returns
/// `GR_ERR_INVALID_FUNCTION` if the property is missing or not a
/// function.
export fn gr_jsc_function_resolve(
    isolate: u64,
    module: u64,
    name: ?[*]const u8,
    name_len: usize,
    out_function: ?*u64,
) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const out = out_function orelse return GR_ERR_INTERNAL;
    const manifest = moduleValue(iso, module) orelse return GR_ERR_INVALID_MODULE;

    if (!c.JSValueIsObject(iso.context, manifest)) return GR_ERR_INVALID_FUNCTION;

    const name_slice: []const u8 = if (name) |p| p[0..name_len] else &[_]u8{};
    const js_name = zigToJsString(name_slice) orelse return GR_ERR_INTERNAL;
    defer c.JSStringRelease(js_name);

    var exception: c.JSValueRef = null;
    const obj_ref = c.JSValueToObject(iso.context, manifest, &exception);
    if (exception != null or obj_ref == null) return GR_ERR_INVALID_FUNCTION;

    const value = c.JSObjectGetProperty(iso.context, obj_ref, js_name, &exception);
    if (exception != null or value == null) return GR_ERR_INVALID_FUNCTION;
    if (!c.JSValueIsObject(iso.context, value)) return GR_ERR_INVALID_FUNCTION;

    const fn_obj = c.JSValueToObject(iso.context, value, &exception);
    if (exception != null or fn_obj == null) return GR_ERR_INVALID_FUNCTION;
    if (!c.JSObjectIsFunction(iso.context, fn_obj)) return GR_ERR_INVALID_FUNCTION;

    c.JSValueProtect(iso.context, value);
    iso.functions.append(.{ .handle = iso.next_function, .value = value }) catch {
        c.JSValueUnprotect(iso.context, value);
        return GR_ERR_INTERNAL;
    };
    out.* = iso.next_function;
    iso.next_function += 1;
    return GR_OK;
}

/// Call a stored function with JSON-marshalled arguments and return
/// the result as a JSON string. Args JSON must be a JSON array; the
/// array is destructured into individual JS arguments. Result is
/// re-serialised to JSON for crossing the FFI.
export fn gr_jsc_function_call(
    isolate: u64,
    function: u64,
    args_json: ?[*]const u8,
    args_len: usize,
    out_result: ?[*]u8,
    out_result_cap: usize,
    out_result_len: ?*usize,
) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const fn_value = functionValue(iso, function) orelse return GR_ERR_INVALID_FUNCTION;

    var exception: c.JSValueRef = null;
    const fn_obj = c.JSValueToObject(iso.context, fn_value, &exception);
    if (exception != null or fn_obj == null) return GR_ERR_INVALID_FUNCTION;

    // Parse args JSON → JSValueRef (an array).
    const args_slice: []const u8 = if (args_json) |p| p[0..args_len] else "[]";
    const js_args_str = zigToJsString(args_slice) orelse return GR_ERR_INTERNAL;
    defer c.JSStringRelease(js_args_str);
    const args_value = c.JSValueMakeFromJSONString(iso.context, js_args_str);
    if (args_value == null) return GR_ERR_INTERNAL;
    if (!c.JSValueIsArray(iso.context, args_value)) return GR_ERR_INTERNAL;

    const args_obj = c.JSValueToObject(iso.context, args_value, &exception);
    if (exception != null or args_obj == null) return GR_ERR_INTERNAL;

    // Read array length.
    const length_str = c.JSStringCreateWithUTF8CString("length");
    defer c.JSStringRelease(length_str);
    const length_value = c.JSObjectGetProperty(iso.context, args_obj, length_str, &exception);
    if (exception != null or length_value == null) return GR_ERR_INTERNAL;
    const length_f64 = c.JSValueToNumber(iso.context, length_value, &exception);
    if (exception != null) return GR_ERR_INTERNAL;
    const arg_count: usize = @intFromFloat(length_f64);

    // Build JSValueRef array from the args array.
    var args_buf = allocator.alloc(c.JSValueRef, @max(arg_count, 1)) catch return GR_ERR_INTERNAL;
    defer allocator.free(args_buf);
    var i: usize = 0;
    while (i < arg_count) : (i += 1) {
        args_buf[i] = c.JSObjectGetPropertyAtIndex(iso.context, args_obj, @intCast(i), &exception);
        if (exception != null) return GR_ERR_INTERNAL;
    }

    // Invoke. `thisObject = null` means use the global object.
    const result = c.JSObjectCallAsFunction(
        iso.context,
        fn_obj,
        null,
        arg_count,
        if (arg_count > 0) @ptrCast(args_buf.ptr) else null,
        &exception,
    );
    if (exception != null) {
        // Best-effort: stringify the exception message into the
        // result buffer so the caller can surface it.
        const msg_owned = jsValueToOwnedString(iso.context, exception);
        if (msg_owned) |m| {
            defer allocator.free(m);
            const wrapped = std.fmt.allocPrint(
                allocator,
                "{{\"__jsc_exception__\":{s}}}",
                .{quoteJsonString(m) orelse "\"<exception>\""},
            ) catch return GR_ERR_INTERNAL;
            defer allocator.free(wrapped);
            return writeStringOut(wrapped, out_result, out_result_len);
        }
        return GR_ERR_INTERNAL;
    }
    if (result == null) return GR_ERR_INTERNAL;

    // Serialise result to JSON.
    return writeJsValueAsJson(iso.context, result, out_result, out_result_cap, out_result_len);
}

fn writeJsValueAsJson(
    ctx: c.JSGlobalContextRef,
    value: c.JSValueRef,
    out_result: ?[*]u8,
    out_result_cap: usize,
    out_result_len: ?*usize,
) i32 {
    _ = out_result_cap;
    // `undefined` doesn't survive a round-trip through JSON. Map to
    // `null` (which is the de-facto "no value" representation our
    // HostValue layer expects).
    if (c.JSValueIsUndefined(ctx, value)) {
        return writeStringOut("null", out_result, out_result_len);
    }
    var exception: c.JSValueRef = null;
    const json_str = c.JSValueCreateJSONString(ctx, value, 0, &exception);
    if (exception != null or json_str == null) return GR_ERR_INTERNAL;
    defer c.JSStringRelease(json_str);
    const owned = jsStringToOwned(json_str) orelse return GR_ERR_INTERNAL;
    defer allocator.free(owned);
    return writeStringOut(owned, out_result, out_result_len);
}

/// Minimal JSON-string quoter for surfacing exception text. Returns
/// null on alloc failure. Caller frees.
fn quoteJsonString(text: []const u8) ?[]u8 {
    var out = std.array_list.Managed(u8).init(allocator);
    out.append('"') catch {
        out.deinit();
        return null;
    };
    for (text) |ch| {
        switch (ch) {
            '"' => out.appendSlice("\\\"") catch return null,
            '\\' => out.appendSlice("\\\\") catch return null,
            '\n' => out.appendSlice("\\n") catch return null,
            '\r' => out.appendSlice("\\r") catch return null,
            '\t' => out.appendSlice("\\t") catch return null,
            else => out.append(ch) catch return null,
        }
    }
    out.append('"') catch return null;
    return out.toOwnedSlice() catch null;
}

// ---------------------------------------------------------------------
// Microtasks — M12.3b-phase1.5: real drain via extern-C C++ shim
// ---------------------------------------------------------------------
//
// Phase1 left this as a documented no-op (auto-drain at
// JSObjectCallAsFunction boundary covered the synchronous Promise
// use case). Phase1.5 wires the real `JSC::VM::drainMicrotasks()` via
// `crates/graphreact-engine-jsc/src/drain_shim.cpp` — a thin C++ TU
// that forward-declares `JSC::VM` and lets the compiler emit the
// mangled call (`_ZN3JSC2VM15drainMicrotasksEv`), resolved at link
// time against the macOS system framework. Genuine async I/O (Rust
// tokio task → JS Promise resolution → microtask continuation now
// observable without another Rust→JS call) lifts on macOS.
//
// On non-macOS this file isn't compiled (build.rs panics); the same
// extern declaration is in scope but the symbol is absent at link
// time on those targets. M12.3b-phase4 rewires the shim against
// vendored WebKit headers for cross-platform support.

extern fn gr_jsc_drain_microtasks_via_ctx(ctx: c.JSContextRef) void;

export fn gr_jsc_microtasks_drain(isolate: u64) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    // Route through the extern-C C++ shim. JSGlobalContextRef is a
    // subtype of JSContextRef in JSC's C API (the global-context type
    // forwards to the public-context type), so the cast is implicit.
    gr_jsc_drain_microtasks_via_ctx(iso.context);
    return GR_OK;
}

// ---------------------------------------------------------------------
// Callbacks — real bidirectional Rust↔JS routing (M12.3a-phase2.5)
// ---------------------------------------------------------------------
//
// Design: each registered host callback becomes a JSObject built from
// a shared `JSClassDefinition` whose `callAsFunction` slot points at
// `jscCallbackTrampoline`. Per-callback identity lives in the
// object's private-data slot as a `*CallbackEntry`. JSC invokes the
// trampoline whenever JS code calls the function; the trampoline
// reads the entry, marshals args to JSON, calls back into Rust via
// `entry.host_fn`, and parses the JSON result into a JSValueRef.
//
// The class def is created lazily on first registration and released
// on engine shutdown. The function objects themselves are owned by
// the global object they're installed on, so they're freed when the
// isolate's context is released.

/// Lazily create the shared callback class. Returns the class ref or
/// null on allocation failure.
fn ensureCallbackClass() c.JSClassRef {
    if (state.callback_class != null) return state.callback_class;
    var def: c.JSClassDefinition = std.mem.zeroes(c.JSClassDefinition);
    def.className = "GraphReactHostCallback";
    def.callAsFunction = jscCallbackTrampoline;
    state.callback_class = c.JSClassCreate(&def);
    return state.callback_class;
}

/// JSC `callAsFunction` trampoline. Routes a JS-side invocation
/// through the entry's stored Rust function pointer.
fn jscCallbackTrampoline(
    ctx: c.JSContextRef,
    function: c.JSObjectRef,
    this_object: c.JSObjectRef,
    argument_count: usize,
    arguments: [*c]const c.JSValueRef,
    exception: [*c]c.JSValueRef,
) callconv(.c) c.JSValueRef {
    _ = this_object;
    const private = c.JSObjectGetPrivate(function);
    if (private == null) {
        setException(ctx, exception, "host callback: missing private data");
        return c.JSValueMakeUndefined(ctx);
    }
    const entry: *CallbackEntry = @ptrCast(@alignCast(private));

    // Marshal args → JSON array string. Even with zero args we send
    // `[]` so the Rust trampoline can use the same parser uniformly.
    const args_json = marshalArgsToJson(ctx, argument_count, arguments) orelse {
        setException(ctx, exception, "host callback: failed to marshal args");
        return c.JSValueMakeUndefined(ctx);
    };
    defer allocator.free(args_json);

    // Allocate result buffer, retry once on too-small.
    var cap: usize = HOST_RESULT_INITIAL;
    var result_json: ?[]u8 = null;
    while (true) {
        var buf = allocator.alloc(u8, cap) catch {
            setException(ctx, exception, "host callback: out of memory");
            return c.JSValueMakeUndefined(ctx);
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
        setException(ctx, exception, "host callback: rust trampoline returned error");
        return c.JSValueMakeUndefined(ctx);
    }

    const json = result_json orelse {
        setException(ctx, exception, "host callback: no result");
        return c.JSValueMakeUndefined(ctx);
    };
    defer allocator.free(json);

    // Empty or "null" → undefined-equivalent (JSValueMakeFromJSONString
    // would return null on empty input).
    if (json.len == 0) return c.JSValueMakeUndefined(ctx);

    const json_jsstr = zigToJsString(json) orelse {
        setException(ctx, exception, "host callback: result allocation");
        return c.JSValueMakeUndefined(ctx);
    };
    defer c.JSStringRelease(json_jsstr);
    const result_value = c.JSValueMakeFromJSONString(ctx, json_jsstr);
    if (result_value == null) {
        setException(ctx, exception, "host callback: invalid json result");
        return c.JSValueMakeUndefined(ctx);
    }
    return result_value;
}

/// Build a JSON-array string from `argument_count` JSValueRefs.
/// Caller frees the returned slice with `allocator.free`.
fn marshalArgsToJson(
    ctx: c.JSContextRef,
    argument_count: usize,
    arguments: [*c]const c.JSValueRef,
) ?[]u8 {
    var exception: c.JSValueRef = null;
    const array_obj = if (argument_count == 0)
        c.JSObjectMakeArray(ctx, 0, null, &exception)
    else
        c.JSObjectMakeArray(ctx, argument_count, arguments, &exception);
    if (exception != null or array_obj == null) return null;
    const json_str = c.JSValueCreateJSONString(ctx, array_obj, 0, &exception);
    if (exception != null or json_str == null) return null;
    defer c.JSStringRelease(json_str);
    return jsStringToOwned(json_str);
}

/// Set a JSC exception out-pointer to a string-valued exception with
/// `msg` as the message. Best-effort — on allocation failure the
/// exception slot is left null and the caller still returns
/// `undefined`.
fn setException(ctx: c.JSContextRef, exception: [*c]c.JSValueRef, msg: []const u8) void {
    if (exception == null) return;
    const js_str = zigToJsString(msg) orelse return;
    defer c.JSStringRelease(js_str);
    const value = c.JSValueMakeString(ctx, js_str);
    exception.* = value;
}

export fn gr_jsc_callback_register(
    isolate: u64,
    name: ?[*]const u8,
    name_len: usize,
    cb_fn: ?HostCallbackFn,
    cb_data: ?*anyopaque,
    out_callback_id: ?*u64,
) i32 {
    _ = cb_data; // The Zig side overrides host_data with a globally-
    //                unique id (see below). The Rust-supplied cb_data
    //                slot is reserved for future use (e.g., engine
    //                identity) and intentionally unused for phase 2.5.
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;
    const out = out_callback_id orelse return GR_ERR_INTERNAL;
    const fn_ptr = cb_fn orelse return GR_ERR_INTERNAL;

    const klass = ensureCallbackClass();
    if (klass == null) return GR_ERR_INTERNAL;

    const name_slice: []const u8 = if (name) |p| p[0..name_len] else &[_]u8{};
    if (name_slice.len == 0) return GR_ERR_INTERNAL;

    // Globally-unique callback id. Encoded as a fake pointer in
    // `host_data` so the trampoline can pass it back to the Rust
    // side as a registry key. Spans isolates so the Rust registry
    // doesn't need per-isolate keying.
    const id = nextGlobalCallbackId();

    // Duplicate the name so we own the storage independently of the
    // caller's lifetime.
    const owned_name = allocator.alloc(u8, name_slice.len) catch return GR_ERR_INTERNAL;
    @memcpy(owned_name, name_slice);

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

    // Build the JS function object with the entry as private data.
    const fn_obj = c.JSObjectMake(iso.context, klass, entry);
    if (fn_obj == null) {
        allocator.destroy(entry);
        allocator.free(owned_name);
        return GR_ERR_INTERNAL;
    }

    // Install on the isolate's global object under the given name.
    const global = c.JSContextGetGlobalObject(iso.context);
    const name_js = zigToJsString(name_slice) orelse {
        allocator.destroy(entry);
        allocator.free(owned_name);
        return GR_ERR_INTERNAL;
    };
    defer c.JSStringRelease(name_js);

    var exception: c.JSValueRef = null;
    c.JSObjectSetProperty(
        iso.context,
        global,
        name_js,
        fn_obj,
        c.kJSPropertyAttributeNone,
        &exception,
    );
    if (exception != null) {
        allocator.destroy(entry);
        allocator.free(owned_name);
        return GR_ERR_INTERNAL;
    }

    iso.callbacks.append(entry) catch {
        allocator.destroy(entry);
        allocator.free(owned_name);
        return GR_ERR_INTERNAL;
    };

    iso.next_callback += 1; // kept for diagnostics / parity with handle counters
    out.* = id;
    return GR_OK;
}

var global_callback_counter: u64 = 0;

fn nextGlobalCallbackId() u64 {
    // Single-threaded JSC use: the static engine_lock() mutex on the
    // Rust side serialises all access. A non-atomic counter is
    // sufficient. If/when JSC is driven from multiple threads, swap
    // for `std.atomic.Atomic(u64)`.
    global_callback_counter += 1;
    return global_callback_counter;
}

/// Look up a JS function by name on the isolate's global object and
/// invoke it with JSON-marshalled args. Symmetric to
/// `gr_jsc_callback_register` (which makes Rust callable from JS):
/// this makes a JS function callable from Rust by name. Useful for
/// hooks that the runtime installs JS-side and needs to invoke later
/// (e.g., `host_dispatch`).
export fn gr_jsc_callback_invoke(
    isolate: u64,
    name: ?[*]const u8,
    name_len: usize,
    args_json: ?[*]const u8,
    args_len: usize,
    out_result: ?[*]u8,
    out_result_cap: usize,
    out_result_len: ?*usize,
) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(isolate) orelse return GR_ERR_INVALID_ISOLATE;

    const name_slice: []const u8 = if (name) |p| p[0..name_len] else &[_]u8{};
    if (name_slice.len == 0) return GR_ERR_CALLBACK_NOT_FOUND;

    const global = c.JSContextGetGlobalObject(iso.context);
    const name_js = zigToJsString(name_slice) orelse return GR_ERR_INTERNAL;
    defer c.JSStringRelease(name_js);

    var exception: c.JSValueRef = null;
    const value = c.JSObjectGetProperty(iso.context, global, name_js, &exception);
    if (exception != null or value == null) return GR_ERR_CALLBACK_NOT_FOUND;
    if (!c.JSValueIsObject(iso.context, value)) return GR_ERR_CALLBACK_NOT_FOUND;
    const fn_obj = c.JSValueToObject(iso.context, value, &exception);
    if (exception != null or fn_obj == null) return GR_ERR_CALLBACK_NOT_FOUND;
    if (!c.JSObjectIsFunction(iso.context, fn_obj)) return GR_ERR_CALLBACK_NOT_FOUND;

    // Parse args JSON into a JSValueRef array.
    const args_slice: []const u8 = if (args_json) |p| p[0..args_len] else "[]";
    const js_args_str = zigToJsString(args_slice) orelse return GR_ERR_INTERNAL;
    defer c.JSStringRelease(js_args_str);
    const args_value = c.JSValueMakeFromJSONString(iso.context, js_args_str);
    if (args_value == null) return GR_ERR_INTERNAL;
    if (!c.JSValueIsArray(iso.context, args_value)) return GR_ERR_INTERNAL;
    const args_obj = c.JSValueToObject(iso.context, args_value, &exception);
    if (exception != null or args_obj == null) return GR_ERR_INTERNAL;

    const length_str = c.JSStringCreateWithUTF8CString("length");
    defer c.JSStringRelease(length_str);
    const length_value = c.JSObjectGetProperty(iso.context, args_obj, length_str, &exception);
    if (exception != null or length_value == null) return GR_ERR_INTERNAL;
    const length_f64 = c.JSValueToNumber(iso.context, length_value, &exception);
    if (exception != null) return GR_ERR_INTERNAL;
    const arg_count: usize = @intFromFloat(length_f64);

    var args_buf = allocator.alloc(c.JSValueRef, @max(arg_count, 1)) catch return GR_ERR_INTERNAL;
    defer allocator.free(args_buf);
    var i: usize = 0;
    while (i < arg_count) : (i += 1) {
        args_buf[i] = c.JSObjectGetPropertyAtIndex(iso.context, args_obj, @intCast(i), &exception);
        if (exception != null) return GR_ERR_INTERNAL;
    }

    const result = c.JSObjectCallAsFunction(
        iso.context,
        fn_obj,
        null,
        arg_count,
        if (arg_count > 0) @ptrCast(args_buf.ptr) else null,
        &exception,
    );
    if (exception != null) {
        const msg_owned = jsValueToOwnedString(iso.context, exception);
        if (msg_owned) |m| {
            defer allocator.free(m);
            const wrapped = std.fmt.allocPrint(
                allocator,
                "{{\"__jsc_exception__\":{s}}}",
                .{quoteJsonString(m) orelse "\"<exception>\""},
            ) catch return GR_ERR_INTERNAL;
            defer allocator.free(wrapped);
            return writeStringOut(wrapped, out_result, out_result_len);
        }
        return GR_ERR_INTERNAL;
    }
    if (result == null) return GR_ERR_INTERNAL;
    return writeJsValueAsJson(iso.context, result, out_result, out_result_cap, out_result_len);
}

// ---------------------------------------------------------------------
// M16-Backend Phase 9 — JSC Web Inspector / Axis B per-body debugger
// ---------------------------------------------------------------------
//
// Marks a JSGlobalContext as remotely inspectable and starts the
// system RemoteInspector listener. On macOS, discovery is via
// lockdownd: an enabled JSContext appears in
// `Safari → Develop → [Hostname] → JSContext`, where the operator
// opens Web Inspector and steps through the body source.
//
// macOS-only by deliberate design. JSC's native debug protocol is
// WebKit's RemoteInspector — consumed natively by Safari Web
// Inspector, the JSC-native IDE on macOS. CDP-over-TCP attach
// (VS Code's JS debugger / Chrome DevTools / WebStorm) requires a
// vendored-WebKit `RemoteInspectorServer` plus a CDP shim — that's
// M-Prod scope and aligns with V8's eventual return (V8 soft-demoted
// per decision 0001; CDP-shaped clients defer with V8). For the
// symmetric design see [feedback_axis_b_no_browser.md] — Python
// bodies use debugpy + VS Code/PyCharm, JSC bodies use
// RemoteInspector + Safari, JVM bodies will use JDWP + IntelliJ.
//
// **Symbols**: the three SPI calls below are exported by the macOS
// system framework binary (verified via `nm -gU` on
// `/System/Library/Frameworks/JavaScriptCore.framework/JavaScriptCore`)
// but NOT declared in the public SDK headers. Declaring our own
// extern prototypes matching the C ABI is sound — the linker
// resolves them against `-framework JavaScriptCore` at final link
// time, the same pattern used by `drain_shim.cpp` for
// `JSC::VM::drainMicrotasks()`.
//
// **Port parameter**: informational on macOS — lockdownd discovery
// does not bind a TCP listener; Safari finds the JSContext by
// process traversal. The port exists for forward-compat with
// M-Prod's vendored RemoteInspectorServer (TCP-bound) and for
// symmetry with `--debug=cpython:<port>` (which IS port-bound via
// debugpy). Today the JSC bridge ignores it.

extern "c" fn JSGlobalContextSetRemoteInspectionEnabled(
    ctx: c.JSGlobalContextRef,
    enabled: bool,
) void;

extern "c" fn JSGlobalContextGetRemoteInspectionEnabled(
    ctx: c.JSGlobalContextRef,
) bool;

extern "c" fn JSRemoteInspectorStart() void;

/// **M16-Backend P9.** Mark the isolate's JSGlobalContext as remotely
/// inspectable and start the system RemoteInspector listener. After
/// this call returns `GR_OK`, the isolate appears in
/// `Safari → Develop → [Hostname] → JSContext` and the operator can
/// attach Web Inspector to step through the body source.
///
/// Idempotent at the framework level: `JSRemoteInspectorStart` is a
/// singleton-init; repeat calls are no-ops. The Rust facade
/// (`JscEngine::enable_debugger`) layers an `AtomicBool` over this
/// for cheaper short-circuit on repeat invocations from the same
/// engine instance.
///
/// Returns `GR_ERR_NOT_INITIALIZED` if the engine isn't initialised,
/// `GR_ERR_INVALID_ISOLATE` if the handle doesn't resolve. The
/// `port` parameter is informational on macOS (see header comment
/// above the extern decls).
export fn gr_jsc_isolate_enable_debugger(handle: u64, port: u16) i32 {
    _ = port; // informational on macOS lockdownd discovery
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(handle) orelse return GR_ERR_INVALID_ISOLATE;
    JSGlobalContextSetRemoteInspectionEnabled(iso.context, true);
    JSRemoteInspectorStart();
    return GR_OK;
}

/// **M16-Backend P9.** Verification round-trip for the headless
/// smoke test. Returns 1 if `JSGlobalContextGetRemoteInspectionEnabled`
/// reads back `true` for the given isolate, 0 if `false`, negative on
/// error. Lets the test confirm `enable_debugger` actually toggled
/// the flag without needing to attach Safari Web Inspector.
export fn gr_jsc_isolate_inspection_enabled(handle: u64) i32 {
    if (ensureInitialized() != GR_OK) return GR_ERR_NOT_INITIALIZED;
    const iso = isolateById(handle) orelse return GR_ERR_INVALID_ISOLATE;
    return if (JSGlobalContextGetRemoteInspectionEnabled(iso.context)) 1 else 0;
}

// ---------------------------------------------------------------------
// Build-time platform check
// ---------------------------------------------------------------------

comptime {
    if (builtin.os.tag != .macos and builtin.os.tag != .ios and builtin.os.tag != .tvos) {
        @compileError("graphreact-engine-jsc currently supports macOS only. " ++
            "Linux/Windows via vendored WebKit is M12.3b — see the strategic plan.");
    }
}
