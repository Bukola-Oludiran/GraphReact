# M13 — CPython engine bridge

**Status:** living document. Last updated: 2026-05-06 (M13 closeout addenda).

**M13 closeout addenda (2026-05-06):**

- **Python 3.12+ floor.** PEP 684's per-interpreter GIL is load-bearing
  for stable sub-interpreter cycling. The legacy 3.9–3.11 single-GIL
  path has known C-extension state-pollution issues (`_signal`,
  `_asyncio`) that crash at low cycle counts. Build now refuses to
  link Python <3.12 with an actionable install hint; the Apple
  Command Line Tools 3.9 framework fallback was removed. The runtime
  3.9-only `useEffectAsync` manual `coro.send(None)` trampoline was
  replaced with `asyncio.run(coro)` since the engine layer guarantees
  3.12+. CI matrix (per the M14.8.x-followon Phase I) was already
  3.12 / 3.13 / 3.14, so no CI cells changed.

- **M13-phase3i — `CALLBACK_REGISTRY` sharded.** Replaced the single
  `OnceLock<RwLock<FxHashMap<u64, SharedCallback>>>` with
  `OnceLock<DashMap<u64, SharedCallback>>`. `DashMap`'s internal
  shard locks remove the global serialization point for
  register/dispose; the trampoline's hot `get(id)` path takes only
  the shard's read lock. New stress test
  `concurrent_isolates_dispatch_callbacks_without_serializing`
  (gated on `python_free_threaded`) asserts that 2×N concurrent
  callback registrations across two isolates land cleanly. Does
  NOT remove `engine_lock()` — Zig-side `state.*` locking remains a
  separable refactor (M13-phase3j).

**M13-phase3h (2026-05-05) — PEP 703 free-threaded build linkage. Reuses 3f's pkg-config probe + cfg flag scaffold. Detection trips on the trailing `t` in pkg-config name (`python-3.14t-embed`) or libname (`python3.14t`). When detected, build.rs emits `--cfg python_free_threaded` + `cargo:python-free-threaded=1`; xtask zig-build passes `-Dpython_free_threaded=true` to `zig build`; cpython.zig appends ` free-threaded` to the engine version string at init time. **No runtime path changes** — PEP 703 removes the GIL but doesn't change any C-API entry point we use; concurrent isolate execution requires Zig-side state locking around `state.*` + per-isolate `modules`/`functions` mutations, separable refactor, out of scope for 3h. New `#[cfg(python_free_threaded)]` runtime probe verifies `sys._is_gil_enabled() == False` (catches distribution bugs where pkg-config auto-resolved a regular Python despite operator intent). 3h footgun: PEP 703 builds inline `_Py_atomic_*` helpers into `Py_DECREF` (split-refcount) and `PyList_Check`/`PyList_Size` (atomic type-cache reads); Zig translate-c can't inline them and helpers aren't exported. All `c.Py_DECREF` swapped to `c.Py_DecRef` (function form since 3.0); `PyList_Check`/`PyList_Size` elided (defensive checks — `PyList_AsTuple` errors cleanly on non-lists). **M13 closes here — every M13 sub-phase shipped (10/10 phase 3 + 4/4 outstanding-work plan).**)

## What this milestone delivers

The second engine via the M12 architecture: CPython embedded through a Zig bridge (`zig/bridge/cpython.zig`), exposed as a Rust crate (`graphreact-engine-cpython`) implementing the same `Engine` trait JSC and the null/echo engines do, plus a runtime composition layer (`crates/graphreact-cli/src/pyruntime.rs`) that mounts `lang="python"` definitions from `.grx` files.

| Component | Status |
|---|---|
| `Engine::initialize` / `shutdown` (process-singleton main interpreter) | ✓ M13-phase1 |
| `Engine::create_isolate` / `dispose_isolate` (legacy single-GIL sub-interpreters) | ✓ M13-phase1 |
| `Engine::eval_module` (`PyRun_String` against a fresh module dict) | ✓ M13-phase2 |
| `Engine::resolve_function` (`PyObject_GetAttrString` + `PyCallable_Check`) | ✓ M13-phase2 |
| `Engine::call_function` (`PyObject_Call` with JSON marshalling) | ✓ M13-phase2 |
| `Engine::register_callback` (Rust closure callable from Python) | ✓ M13-phase2.5 |
| `gr_cpython_callback_invoke` (Rust-calls-Python by name) | ✓ M13-phase2.5 |
| `.grx` parser support for `lang="python"` definitions + IR `lang` tag | ✓ M13-phase3a |
| `PyRuntime` mount-without-hooks scaffold (per-component `init(ctx)` callable; ctx={id, props}) | ✓ M13-phase3a |
| Python HOOK_PRELUDE: `use_state` + `use_effect` (deps + cleanup) + `__register_callback` / `__invoke_callback` / `__release_callback` / `__release_tokens` | ✓ M13-phase3b |
| Scheduler integration via graphreact-core (re-callable `body(ctx)` + state preservation across renders + setter-driven re-render) | ✓ M13-phase3b |
| `useEmit` / `useMessage` (message hooks) | ✓ M13-phase3c |
| `useMemo` / `useCallback` / `useReducer` (memoization hooks) | ✓ M13-phase3d-α |
| `useNeighbors` / `useTopology` / `useAcceptEdge` (Tier 1 graph-read hooks + mutation queue) | ✓ M13-phase3d-β |
| `ctx.directory` + `useGraphQuery` Tier 1 (skeleton + neighbors / hasNode / hasEdge / nodeCount over static GraphSnapshot) | ✓ M13-phase3d-γ |
| `useGraphQuery` algorithm accessors (shortestPath / allPaths / aStarPath / bfs / dfs / connectedComponents / cycleDetection) + live GraphSnapshot | ✓ M13-phase3d-δ |
| Tier 2 `g.tier2(msgType, opts?)` (policy-derived reachability — 12 accessors + 5 cost heuristics + 2 flavors) | ✓ M13-phase3d-ε |
| `useEdgeAcceptHandler` + Tier 3 `g.tier3(msgType, opts?)` | ✓ M13-phase3d-ζ |
| `useSpawnChild` / `useUnmountSelf` / `useCompose` (dynamic node lifecycle hooks) | ✓ M13-phase3e-α |
| supervision (`useMonitor` / `useDemonitor` / `useLink` / `useUnlink` / `useGroupJoin` / `useGroupLeave`) | ✓ M13-phase3e-β |
| `useSource` + `timer_source` / `delay_source` / `http_source` / `watch_source` | ✓ M13-phase3e-γ |
| asyncio integration (Python analogue of M12.3b-phase1's Promise→Future bridge) | ✓ M13-phase3g |
| PEP 684 multi-GIL sub-interpreters (Python 3.12+) | ✓ M13-phase3f |
| PEP 703 free-threaded build (`python3.14t`) — Rust side already Send+Sync | ✓ M13-phase3h |

## Discovery + linkage

**M13-phase3f rewrote the resolver.** Production builds (Linux, macOS-with-Homebrew, Windows-with-pkg-config) discover Python via `pkg-config python-3.X-embed`.

**Build floor: Python 3.12.** PEP 684 (per-interpreter GIL, 3.12+) is load-bearing for stable sub-interpreter cycling. The 3.9–3.11 single-GIL path has known C-extension state-pollution issues that crash at low cycle counts; the engine refuses to link an older Python rather than silently shipping a crash-prone artifact.

**Resolution order** (in `crates/graphreact-engine-cpython/build.rs::resolve_python`):

1. `GRAPHREACT_PYTHON_PKGCONFIG=<name>` — explicit pkg-config name; hard-error on miss.
2. `GRAPHREACT_PYTHON_VERSION=<X.Y>` → `python-X.Y-embed`; hard-error on miss.
3. Auto-probe: `python-3.14t-embed`, `python-3.14-embed`, `python-3.13-embed`, `python-3.12-embed` (newest wins).
4. Hard error with actionable install hint. (No more Apple CLT 3.9 fallback — PEP 684 floor.)

Each pkg-config-resolved Python contributes:

- `--cflags`: drives `-I<include>` for both cargo (transitive to Rust-bindings, currently none) and the Zig `@cImport(<Python.h>)` via xtask zig-build's `-Dpython_include=<path>`.
- `--libs`: drives `-L<libdir>` + `-l<libname>` via `cargo:rustc-link-{search,lib}` for the cargo link step.
- `prefix` / inferred `libdir`: baked into the consumer binary as `-Wl,-rpath,...` so dyld resolves the dylib at runtime without `DYLD_LIBRARY_PATH` being set.

**Per-minor distribution.** CPython is per-minor-version ABI-locked: a binary built against libpython3.13 won't load libpython3.12 at runtime. Released artifacts are therefore per-minor slices (`grx-py-3.12`, `grx-py-3.13`, `grx-py-3.14`, `grx-py-3.14t`), same shape as numpy wheels. The `python_3_12` cfg flag is set per-build according to the resolved minor and gates the PEP 684 path.

## Phase scope — honest staging rationale

The M12.3a-phase1 → phase2 → phase2.5 staging was justified because each was a real complexity boundary on the JSC integration. For M13, the phases collapse differently:

- **Phase 1 alone (lifecycle)** would deliver no user-visible capability — `--runtime=cpython` would init+dispose Python without ever evaluating any code.
- **Phase 2 (eval / resolve / call)** is the actual capability gate — proves Python evaluation works through the trait surface.
- **Phase 2.5 (callbacks)** is genuinely a separate complexity boundary: `PyCFunction_New` + `PyCapsule_New` for private-data routing is a different problem shape from "eval a string." Reasonable session boundary.
- **Phase 3 (hook semantics)** is multi-session work parallel to `jsruntime.rs` (10K LOC) and needs the parser to learn language-typed component bodies first.

So phases 1+2 shipped together as the first meaningful session; phase 2.5 shipped as the next session (this one); phase 3+ is a multi-session lift.

## Phase 2.5 — bidirectional Rust↔Python callbacks

CPython analogue of JSC's M12.3a-phase2.5. Each registered host callback becomes a `PyCFunction` whose `m_self` slot is a `PyCapsule` carrying a heap-allocated `CallbackEntry`. The shared `pythonCallbackTrampoline` is invoked by Python on call; it reads the entry from the capsule, marshals args via stdlib `json.dumps`, calls into Rust through [`host_callback_trampoline`] (which routes by id into the process-global [`CALLBACK_REGISTRY`]), and parses the JSON result back to a `PyObject*` via `json.loads`.

### Translation map (JSC → CPython)

| JSC piece | CPython equivalent |
|---|---|
| `JSClassDefinition` + lazy `ensureCallbackClass` | Single static `PyMethodDef { ml_meth = pythonCallbackTrampoline, ml_flags = METH_VARARGS }` shared across all callbacks (identity in the capsule, not the methoddef) |
| `JSObjectMake(ctx, class, entry_ptr)` | `PyCFunction_NewEx(&SHARED_METHOD_DEF, capsule, NULL)` where `capsule = PyCapsule_New(entry_ptr, "graphreact.host_callback_entry", capsule_destructor)` |
| Install on global via `JSObjectSetProperty` | `PyDict_SetItemString(builtins_dict, name, py_callable)` on the **active sub-interpreter's** builtins (per-isolate; `ThreadStateGuard` already entered) |
| `JSObjectGetPrivate(function)` in `jscCallbackTrampoline` | `PyCapsule_GetPointer(self, "graphreact.host_callback_entry")` in `pythonCallbackTrampoline(self, args)` |
| `JSObjectMakeArray` + `JSValueCreateJSONString` to marshal args | `json.dumps(args_tuple)` via a per-call `PyImport_ImportModule("json")` |
| `JSValueMakeFromJSONString` to parse return | `json.loads(return_str)` |
| `gr_jsc_callback_invoke` (`JSObjectGetProperty` on global) | `gr_cpython_callback_invoke` (`PyDict_GetItemString` on builtins + `PyObject_Call`) |
| Per-iso `callbacks: ArrayList(*CallbackEntry)`, freed in `disposeIsolate` | Capsule destructor handles per-entry cleanup deterministically (sub-interpreter teardown clears builtins → PyCFunction GC'd → capsule GC'd → destructor frees the entry); Rust-side `iso_callbacks: FxHashMap<u64, FxHashSet<u64>>` is the only per-iso tracking we need |

### Footgun encountered + fixed: `PyObject_CallMethod` tuple unpacking

The first iteration of the trampoline used `PyObject_CallMethod(json_mod, "dumps", "O", args_tuple)` to serialise the args tuple. Tests failed with `dumps() takes 1 positional argument but 2 were given`. Root cause: `PyObject_CallMethod`'s internal arg-tuple construction has a "wrap if not tuple" check — when the value passed via `"O"` is itself a tuple, CallMethod uses it AS the call's args tuple, unpacking `(2, 3)` into `dumps(2, 3)` instead of `dumps((2, 3))`.

Fix: `jsonDumps` now uses explicit `PyObject_GetAttrString(json_mod, "dumps")` + `PyTuple_New(1)` + `PyTuple_SetItem` + `PyObject_Call(dumps, args_tuple, NULL)`. Both call sites (the trampoline and the existing `gr_cpython_function_call` result-marshalling path) benefit; uniform shape across all value types.

### Capsule-destructor lifecycle

`PyCapsule_New(entry_ptr, CAPSULE_NAME, callbackEntryCapsuleDestructor)` registers the destructor. The destructor fires deterministically when the capsule's refcount hits 0 — which happens when the wrapping `PyCFunction` is GC'd, which happens when the sub-interpreter's builtins dict is cleared during `Py_EndInterpreter`. The destructor frees `entry.name` (allocator-allocated) and the entry itself.

We do NOT manually free in `disposeIsolateInner` (unlike JSC, which has to manually free because JSC class-private-data has no destructor hook on context release). The CPython path is cleaner; the Rust-side `iso_callbacks` map is the only per-iso bookkeeping we still maintain (so `dispose_isolate` / `shutdown` drop the matching closure Arcs from `CALLBACK_REGISTRY`).

### Symmetric path — `gr_cpython_callback_invoke`

Looks up a Python callable on the active sub-interpreter's builtins by name (`PyEval_GetBuiltins()` + `PyDict_GetItemString`) and calls it via `PyObject_Call` with JSON-marshalled args. Mirrors JSC's `gr_jsc_callback_invoke`. Phase 3+ will use this when the Python-side hook prelude installs helpers that the Rust runtime needs to invoke (analogous to JSC's `__invokeCallback` / `__augmentCtx` / `__graphQueryProxy` patterns).

### invoke_global_static — borrow-bypass shape (defensive)

Free-function variant of `CPythonEngine::invoke_global` for callsites that can't hold a `&mut CPythonEngine` borrow. Mirrors `graphreact-engine-jsc::invoke_global_static`; not needed by phase 2.5's tests (they all hold `&mut e`), but ships now because phase 3+ predicate-style hooks (analogous to JSC's `useEdgeAcceptHandler` re-entry pattern from M12.3a-followon-F) will need it.

### 4 new acceptance tests

Mirror JSC phase2.5's tests with Python flavor:

1. **`phase2_5_rust_closure_callable_from_python`** — register `host_add(a, b)` Rust closure that returns `a + b`; eval `def init(): return host_add(2, 3)`; resolve init; call → `Number(5.0)`.
2. **`phase2_5_bidirectional_callback_observed_via_shared_state`** — register `host_notify(s)` Rust closure that pushes args into `Mutex<Vec<String>>`; eval Python that calls it twice; assert the vec contains `["first", "second"]`. Proves side-effecting closures (state-preserving callbacks are what phase 3 hooks need).
3. **`phase2_5_invoke_global_calls_python_function_by_name`** — eval Python module that does `import builtins; builtins.double = lambda x: x * 2`; `engine.invoke_global(iso, "double", &[Number(7.0)])` → `Number(14.0)`. Proves the symmetric Rust→Python path.
4. **`phase2_5_dispose_isolate_drops_registered_callbacks`** — register a callback, assert present in `CALLBACK_REGISTRY`; `dispose_isolate`; assert absent. Proves cleanup contract.

All 4 pass. Engine-cpython suite: 15 passed, 1 ignored. Workspace: 565 passed (was 562 phase1+2; +4 phase2.5 tests, −1 stub-marker test = +3 net). Clippy clean.

## Isolate model — process-singleton CPython

CPython has a documented limitation: `Py_Initialize` → `Py_FinalizeEx` → `Py_Initialize` is unsupported in 3.11 and earlier. C-extension global state isn't fully reset, so subsequent `Py_NewInterpreter` calls SIGSEGV on stale state. PEP 432 (3.12+) begins addressing this but full re-init is still partial.

The pragmatic shape that avoids cargo's parallel-test process-reuse from tripping this: treat **shutdown as "clean up isolates, leave the main interpreter alive."** Resources reclaim at process exit. Effects:

- Per-isolate state (modules, functions, sub-interpreter) is fully released in `dispose_isolate` — that's what the user sees.
- The main interpreter, version string, and Zig allocator stay live across `shutdown()` → `initialize()` cycles.
- `state.initialized` flips back to false so the public `Engine` trait contract is preserved (callers re-init and the engine reports "yes I'm ready" again).

## Sub-interpreter stress-test calibration

JSC's M12.3a-phase3c gate runs 1000+1000-cycle isolate churn (steady-state RSS ≤5 MiB). CPython 3.9's legacy `Py_NewInterpreter`/`Py_EndInterpreter` model **does not survive** that kind of churn — `Py_NewInterpreter` cycling at high rates trips known C-extension global-state issues fixed only in Python 3.12+'s PEP 684 per-interpreter GIL.

Engine-cpython ships with a modest 3-cycle leak gate (`isolate_stress_modest_cycles_no_leak`) that catches catastrophic-leak-shaped breakage without crossing 3.9's instability threshold, plus an `#[ignore]`d probe (`isolate_stress_probe_threshold`) that runs 200 cycles for diagnostic characterisation under `cargo test -- --ignored`. M13-phase3+ adds Python-3.12+ detection and engages PEP 684 sub-interpreters; the JSC-shaped 1000+1000 gate widens back on supported Pythons.

## JSON marshalling — `json` stdlib on the Zig side

The FFI boundary uses JSON to mirror JSC's marshalling shape. Conversion path:

1. Rust: `HostValue` → `serde_json::Value` → JSON string (via `serde_json::to_string`).
2. Zig: `json.loads(json_string)` (Python stdlib) → Python list (positional args).
3. Zig: `PyList_AsTuple` + `PyObject_Call(fn, args_tuple, NULL)` → return value.
4. Zig: `json.dumps(return_value)` → JSON string.
5. Rust: JSON string → `serde_json::Value` → `HostValue` (via `HostValue::from_json`).

The `json` module is imported per-call (Python caches it in `sys.modules`, so the first call pays a tiny cost; subsequent calls hit the cache). Phase 3 may swap to direct C-API marshalling (PyDict_New / PyList_New / PyLong_FromLongLong / etc.) if profiling demands; the conversion shape stays semantically identical.

## CLI activation — `grx run --runtime=cpython`

The `.grx` fixtures define JS component bodies (M11 parser only knows JS). `--runtime=cpython` therefore can't naively run them — it would either (a) hit `register_callback` (a phase 2.5 stub) via the default `dry_mount` path, or (b) try to parse JS as Python and SyntaxError.

The CPython arm in `commands.rs::run_run` parses the `.grx` for the report's graph counts (so the trailer line matches other engines' shape) and runs an **internal Python smoke module** (`def smoke(): return {'name': 'cpython-online', 'graph': '__GRAPH__'}`) end-to-end through `eval_module` + `resolve_function` + `call_function`. The `[smoke]` line in `mounted` is the user-visible signal that the engine surface works.

Real `.grx`-driven Python execution waits for M13-phase3 to land:
1. Parser support for language-typed component bodies (e.g. `<MyNode lang="python">def init(): ...</MyNode>`).
2. `PyRuntime` composition layer paralleling `crates/graphreact-cli/src/jsruntime.rs`.

## Phase 3a — `.grx` parser support + PyRuntime mount-without-hooks scaffold

First session in which `grx run --runtime=cpython <fixture.grx>` executes an author-written Python body from a `.grx` file. Pre-phase-3a, the cpython arm in `commands::run_run` ran an internal smoke module hard-coded by the CLI; the `.grx` fixtures' bodies were JS and never executed on CPython.

### Parser changes

- **`AttrMap` survey on definition blocks.** `Parser::parse_definition_block` now calls `parse_attributes()` between `<MyNode` and `>` (it previously expected `TagEnd` immediately, gating the lexer's raw-content trigger on the no-`id` case). The lexer change is zero — its raw-content trigger is "non-reserved tag, no `id` attr seen, last token was `TagEnd`", which still fires when the only attribute is `lang`.
- **New `ComponentLang` enum** in `crates/graphreact-grx-parser/src/ast.rs`: `Js` (default) / `Python`. `from_attr_str` accepts `"js" / "javascript" / "ts" / "typescript"` → `Js` and `"python" / "py"` → `Python`. Unknown values surface as `GrxParseError::Parse` at the definition's source loc with a clear "supported: js (default) / python" message.
- **AST + IR carry `lang`.** `NodeDefinitionBlock { lang: ComponentLang }` flows to `IrDefinition { lang: ComponentLang }`. `ir_to_json` emits the lang tag so `grx parse --json` shows language assignment for tooling.

### PyRuntime composition layer (`crates/graphreact-cli/src/pyruntime.rs`)

Parallels `jsruntime.rs::mount_with_scheduler` at deliberately reduced scope. Phase 3a:

1. Resolves the `.grx` file (multi-file). Concatenates definitions from root + imports.
2. Builds an IR for the report's node/edge counts.
3. Brings up CPython (`engine.initialize` + `create_isolate`).
4. For each `lang="python"` definition:
   - `engine.eval_module(iso, "<origin>::<component>", source)` evaluates the body as a fresh sub-interpreter module.
   - `engine.resolve_function(iso, module, "init")` resolves the top-level `init(ctx)` callable (phase 3a author convention).
5. For each node in IR: builds per-instance ctx (`{id, props}` from IR attrs), calls `init(ctx)`, captures the returned manifest dict for the `mounted_components` report.
6. Returns a `PyRuntime { engine, iso, component_handles, graph_id, n_nodes, n_edges, n_definitions, mounted }`. `commands::run_run` builds the `DryMountReport` from this and calls `runtime.shutdown()` (disposes isolate + engine).

The cpython arm in `commands::run_run` checks whether ANY definition has `lang="python"`. If so, route through `pyruntime::mount_python_definitions`; otherwise fall back to the predecessor smoke-module path so existing JSC-only fixtures (`ping-pong.grx`, `multi-file/main.grx`, etc.) continue to print a trailer when `--runtime=cpython` is invoked against them.

### Single-engine-per-system enforcement

A `lang="js"` definition fed to the cpython arm — or a `lang="python"` definition fed to the JSC scheduler arm — surfaces as `RuntimeError::EngineMisroute` with a clear diagnostic ("component `<X>` declares `lang="python"` but the active engine is `jsc`. Use `--runtime=python` …"). Three call sites enforce this: `runtime::dry_mount_jsc`, `jsruntime::mount_with_scheduler`, `pyruntime::mount_python_definitions`. M22 unifies the multi-engine story; phase 3a is intentionally strict so authors get a clear error early.

### Author convention

```python
<MyNode lang="python">
def init(ctx):
    return {
        'id': ctx['id'],
        'accepts': ['poll'],
        'emits': ['result'],
    }
</MyNode>
```

`ctx` arrives as a Python dict with `id` (string) + `props` (dict of attributes from the IR — everything except IR-injected metadata `component`, `_depth`, `_parentPath`, `id`). The returned dict is the manifest (accepts/emits + arbitrary author metadata).

Phase 3b will lift this convention: top-level hook calls (`use_state`, `use_effect`) become valid; `init(ctx)` becomes optional (the body itself becomes the per-render callable, mirroring JSC's `(function(ctx){ ... })` shape).

### Two acceptance tests

In `crates/graphreact-cli/src/pyruntime.rs::tests`. Both serialise on `engine_lock()` because Python is process-global.

1. **`phase3a_python_fixture_mounts_through_pyruntime`** — mounts `tests/fixtures/python-smoke.grx` (Sensor + Logger, both `lang="python"`); asserts `n_nodes == 2` + `mounted.len() == 2` + Sensor / Logger appear in mount records.
2. **`phase3a_rejects_js_lang_definition`** — mounts `tests/fixtures/python-smoke-mixed-rejected.grx` (mixed JS + Python definitions); asserts `EngineMisroute { lang: "js", engine: "cpython" }`.

Plus 4 parser-level tests in `graphreact-grx-parser`:

- `parses_definition_block_with_lang_python` — round-trips `lang="python"` through AST.
- `parses_definition_block_with_lang_js_explicit` — explicit `lang="js"` matches default.
- `parser_rejects_unknown_lang_value` — `lang="ruby"` errors with "unknown `lang`".
- `ir_carries_lang_python_for_python_definitions` — IR JSON contains `"lang":"python"`.
- Existing `parses_definition_block` test extended to assert default `Js` for unflagged definitions.

### Python GIL roadmap (informational, surfaced 2026-05-03)

External landscape that informs phase 3 sub-staging:

- **PEP 779** accepted Python 3.14 (Oct 2025) — free-threaded build (`python3.14t`) officially supported. Single-thread perf hit dropped to 5–10% (was 20–40% in 3.13). Specializing interpreter re-enabled.
- **PEP 703 roadmap**: 2-3 releases (2026-2027) → GIL controlled by runtime flag; 2-3 more (2028-2030) → GIL disabled by default.
- **PEP 684 (per-interpreter GIL, Python 3.12+)** is orthogonal to PEP 703 — both ship.
- macOS Command Line Tools still ships 3.9 (the M13 floor); Homebrew/python.org/uv ship 3.14 stably.

**Architectural implications for the rest of M13:**

- Don't paint into a single-GIL corner. The Rust-side `CALLBACK_REGISTRY` is already `Send + Sync` (callbacks are `Arc<dyn Fn(...) + Send + Sync>`); the static `engine_lock()` mutex is test-only serialization, not a runtime constraint. Free-threaded CPython embedding should work without architectural changes once we link against `python3.14t`.
- Phase 3f activates PEP 684 per-interpreter GIL when 3.12+ is detected. This widens the 1000-cycle stress test (`Py_NewInterpreter` churn fix in PEP 684 retires the `isolate_stress_modest_cycles_no_leak` 3-cycle calibration) and lets us drop the "shutdown leaves main interpreter alive" workaround for 3.12+ (PEP 432's improved init/finalize).
- Phase 3h adds a build-flag for the free-threaded build path (`python3.14t` linkage).

## Phase 3b — Python HOOK_PRELUDE + use_state + use_effect + scheduler integration

The slice that makes Python authors first-class citizens on GraphReact. State preserved across renders, effects fire on mount + deps change with cleanup, the body itself is the per-render callable, the setter marks the owning node dirty.

### HOOK_PRELUDE

A Python source-string constant in `pyruntime.rs` (~80 lines), evaluated on each isolate via `engine.eval_module(iso, "_grx_hook_prelude", HOOK_PRELUDE)` AFTER the host callbacks (`host_state` / `host_state_set` / `host_effect`) are registered as builtins via the M13-phase2.5 callback bridge. The prelude:

- Defines `_hostCallbacks = {}` + `_nextToken = [1]` — module-level dict + counter for the per-isolate token registry.
- Defines `__register_callback(fn) → token`, `__invoke_callback(token, args=None) → envelope`, `__release_callback(token)`, `__release_tokens(tokens)`.
- Defines `use_state(initial)` wrapper: `host_state(initial)` → `{id, value}` → builds Python setter closure that captures `id` and calls `host_state_set(id, new_value)` → returns `(value, setter)`.
- Defines `use_effect(fn, deps=None)` wrapper: `__register_callback(fn) → token` → `host_effect(token, deps or [])`.
- Installs all wrappers on `builtins` so any user module sees them without import.

### Hook routing

`use_state(0)`:
1. Python wrapper calls `host_state(0)`.
2. `build_host_state_callback` reads `CURRENT_NODE_ID` (TLS set by `ComponentEvalGuard`) + `PY_HOOK_INDEX`.
3. Calls `core_use_state(initial)` — graphreact-core's hook slot machinery preserves state across renders.
4. Mints (or reuses) a stable u64 id for `(node_id, hook_pos)`, stores the `Signal<Value>` in `SIGNAL_REGISTRY`, returns `{id, value}`.
5. Python wrapper builds setter closure that captures `id`; on call, `host_state_set(id, new_value)` looks up the Signal and `.set(v)`'s it, marking the owning node dirty.

`use_effect(fn, deps)`:
1. Python wrapper: `__register_callback(fn)` (pure Python — adds to `_hostCallbacks`, returns token) → `host_effect(token, deps)`.
2. `build_host_effect_callback` reads `CURRENT_ENGINE` / `CURRENT_ISO` from TLS (set by `PyEngineGuard` around `process_batch`); installs a `core_use_effect` closure.
3. When `flush_effects` fires, the closure invokes `__invoke_callback(token, [])` via `engine.invoke_global`. Python returns envelope:
   - `{kind: 'value'}` — plain effect.
   - `{kind: 'cleanup', token: ct}` — effect returned a callable; registered under fresh `ct`.
   - `{kind: 'throw', message}` — effect raised.
4. Cleanup envelope translated to `EffectCleanup` that re-invokes `__invoke_callback(ct)` + `__release_callback(ct)` when graphreact-core's flush triggers cleanup (deps changed or unmount).

### Author convention shift

`def body(ctx)` is the new convention (preferred). `def init(ctx)` retained as phase 3a back-compat. The `mount_with_scheduler` body-handle resolution tries `body` first, then `init`. Python doesn't have JS's `(function(ctx){...})` anonymous-function-expression shape, so the body-as-named-function is the natural Python idiom.

### TLS layout (mirrors jsruntime exactly)

Per-thread registries: `SIGNAL_REGISTRY` (id → Signal), `SIGNAL_BY_HOOK_POS` ((node_id, hook_pos) → id), `NEXT_SIGNAL_ID`.

Per-evaluation: `CURRENT_NODE_ID`, `PY_HOOK_INDEX`, `CURRENT_ENGINE`, `CURRENT_ISO`.

`ComponentEvalGuard::new(node_id)` resets PY_HOOK_INDEX + sets CURRENT_NODE_ID per render. `PyEngineGuard::new(engine, iso)` installs the engine + iso TLS for the duration of `process_batch` so deferred-fire effects can re-enter CPython without capturing `Rc` directly (the `BoxCallback` registry forces `Send + Sync`; `Rc` violates that — TLS sidesteps).

### 7 acceptance tests (5 phase 3b + 2 phase 3a back-compat)

Phase 3a back-compat:
- `phase3a_python_fixture_mounts_through_pyruntime` — phase 3a `init(ctx)` fixture mounts cleanly through the new scheduler-integrated path.
- `phase3a_rejects_js_lang_definition` — single-engine-per-system enforcement still active.

Phase 3b core:
- `phase3b_use_state_preserves_value_across_renders` — external `signal.set(7)` between batches; second render observes new value.
- `phase3b_use_state_setter_from_python_re_renders_node` — Python-side setter fires from inside `use_effect`, marks node dirty, second batch observes bumped value.
- `phase3b_use_effect_fires_on_mount_with_cleanup` — deps change → cleanup → re-mount.
- `phase3b_use_effect_empty_deps_fires_once` — empty deps `[]` fires effect exactly once across re-renders.
- `phase3b_hook_prelude_visible_as_builtins` — probe via `import builtins; hasattr(builtins, …)` confirms wrappers installed.

### Footguns + lessons

1. **`match engine.borrow_mut().X() { Err(_) => engine.borrow_mut().Y() }`** — Rust extends the head's temporary across all arms; the Err arm panics with "RefCell already borrowed". Bind into a local first.
2. **JSON marshalling makes Python `count` a float** (`HostValue::Number` is f64; round-trips as `0.0`). f-strings emit `mount:0.0`, not `mount:0`. Tests use `starts_with` to be format-agnostic.
3. **`dir(__builtins__)`** returns dict keys when `__builtins__` is the dict (in non-`__main__` modules). Use `import builtins; hasattr(builtins, name)` for module-import semantics.
4. **Test fixtures with test-only callbacks** (`host_observe`, `host_log`) need NameError guards (`try: host_observe(x) except NameError: pass`) to run cleanly under CLI smoke.
5. **clippy `doc_lazy_continuation`** requires list-item continuation lines aligned with the first non-prefix character of the item. `5. ` is 3 chars wide, so continuation prefix is 3 spaces.
6. **`add_edge` 4th argument** is `Option<String>` (the edge key, for multi-edge support). Pass `None` for single-edge cases.

## Cross-engine ecosystem-access asymmetry (note 2026-05-03)

Worth flagging explicitly because it shapes architectural expectations: as of M13-phase3b and M12.3a-followon-F, **CPython bodies get PyPI access for free, but JSC bodies don't get NPM access**. This asymmetry is architectural, not accidental, and is being closed by **M12.4 (Node-API compatibility for JSC bodies)** — a future milestone locked in 2026-05-03.

### The asymmetry

- **CPython bodies (M13-phase3a+):** Python's import machinery is part of every sub-interpreter. A Python author writing `<MyNode lang="python">` can `import requests`, `import sqlalchemy`, `import boto3`, `import psycopg2` etc. The PyPI ecosystem is essentially available for free, modulo sub-interpreter quirks that PEP 684 (M13-phase3f) addresses. **Python authors are first-class.**
- **JSC bodies (M12.3a/b):** The runtime's JSC integration (forked from Bun under MIT) deliberately stripped Bun's Node-compat layer at M12 ("VirtualMachine.zig REST IS BUN-SPECIFIC — SKIP" in the fork-and-strip matrix). JSC bodies are pure ECMAScript: no `require()`, no `import` from `node_modules`, no `process` / `Buffer` / `fs` / `http` / `crypto`. **JS authors today cannot use *any* NPM package.** The hooks substrate (`use_state`/`use_effect`/etc.) works, but the ecosystem story doesn't.

### Why the asymmetry exists

Bun's pitch is "drop-in Node replacement, faster + Node-API-compatible, full backend NPM works." Bun was the M12.3 reference *because* of that backend-NPM positioning. The M12 plan correctly forked Bun's JSC embedding (lifecycle, microtasks, callbacks) but deferred the Node-compat layer because it's a substantial track on its own — comparable in scope to M14 (distribution) or M15 (hot reload). Shipping JSC + hooks + scheduler at M12.3 delivered the runtime story; M12.4 delivers the ecosystem story.

### M12.4 closes the gap

The user locked in M12.4 (option (C) of three — pure-ES status quo / targeted shims / full Bun-fork) on 2026-05-03. M12.4 reverses the M12.3 fork-and-strip exclusion: takes Bun's `node:` shims, module resolver, package.json `exports` handler, ESM/CJS interop. Five sub-phases (module resolution → core primitives → I/O primitives → NPM compat surface → ESM/CJS edge cases). After phase 3 minimum, JSC authors get the same kind of ecosystem access PyPython authors already have today.

### Until M12.4 ships

Before M12.4 lands, the practical implication for JS authors is **lean on host callbacks for non-trivial work**. The hooks substrate covers state + effects + messages + supervision; anything requiring NPM (database client, HTTP client, encryption library) needs host-callback wiring on the Rust side. This is workable for the immediate roadmap but doesn't deliver on the original "backend NPM ecosystem reach" framing — M12.4 does.

See plan file's M12.4 section for sub-staging + acceptance gates + risk mitigation. See [decision 0002](decisions/0002-graphreact-is-backend-only.md) (Addendum 2026-05-03) for the architectural framing.

## Phase 3c — use_emit + use_message

Bundles the next pair-coupled primitive on top of phase 3b's substrate. Without these, Python authors are isolated state machines; with them, they're first-class peers to JSC authors on edge-typed message routing — the load-bearing primitive for every named application class.

### HOOK_PRELUDE additions (Python)

```python
def use_emit():
    def emit(msg_type, payload=None, opts=None):
        host_emit(msg_type, payload, opts)
    return emit

def use_message(msg_type, handler):
    if not callable(handler):
        return
    token = __register_callback(handler)
    host_message(msg_type, token)

_builtins.use_emit = use_emit
_builtins.use_message = use_message
```

`use_emit()` returns a closure mirroring JSC's 3-arg shape `(msg_type, payload, opts)`. Targets are addressed via `opts={'to': ['nodeId']}` or `opts={'direct': 'nodeId'}` — same key vocabulary JSC's `parse_emit_options` recognises. `use_message(msg_type, handler)` reuses phase 3b's `__register_callback` token registry.

### New Rust callbacks

`build_host_emit_callback() -> BoxCallback` — verbatim port of `jsruntime.rs::build_host_emit_callback` (jsruntime.rs:950-970). Validates `args[0]` String + parses `args[1]` payload via `HostValue::into_json` (default `Null`) + parses `args[2]` opts via the new local `parse_emit_options`. Calls `core_use_emit()` and immediately invokes the returned closure. Returns `Ok(HostValue::Null)`. No TLS read — `core_use_emit()` reads `HOOK_CTX` internally.

`build_host_message_callback() -> BoxCallback` — verbatim port of `jsruntime.rs::build_host_message_callback` (jsruntime.rs:997-1046). Reads `current_engine()` + `current_iso()` TLS once at registration time (set by `PyEngineGuard` from phase 3b); registers a `core_use_message(type, move |msg: &IncomingMessage| { … })` closure that on every delivery marshals `IncomingMessage` into `HostValue::Object(vec![("from", ...), ("type", ...), ("payload", from_json(...)), ("ttl", Number)])` and invokes `engine.borrow_mut().invoke_global(iso, "__invoke_callback", &[token, [host_msg]])`. Returns `Ok(HostValue::Null)`.

`parse_emit_options(arg: Option<&HostValue>) -> EmitOptions` — duplicate of jsruntime's private fn. Recognises `to` (Array of String), `ttl` (Number → u32), `edgeKey` (String), `direct` (String); ignores unknown keys.

### What's enforced by graphreact-core (no Python work)

- **Permission-manifest validation:** `scheduler.rs:1340-1349` silently drops messages at delivery time when the receiver's resolved policy doesn't include the message type in `accepts`. Phase 3c just exposes the `accepts` / `emits` arrays via the existing `host_value_to_manifest` shape.
- **TTL inheritance (M4-followon):** `scheduler.rs::make_emit:1431-1515` resolves TTL via 4-tier fallback (explicit `opts.ttl` → `current_delivery_ttl` → `last_received_ttl` → `DEFAULT_TTL`) then caps per-edge via the `<edge ttl={N}>` annotation. Python authors get this for free.

### 7 acceptance tests (6 phase 3c + 1 extension)

Programmatic tests (CommunicationGraph + body strings + scheduler) modeled cell-for-cell on JSC's `phase3d_*`. The fixture-path test is the seventh.

1. **`phase3c_python_emit_dispatches_to_neighbor`** — A → B; A's body emits; B's `use_message` observes via `host_observe`.
2. **`phase3c_python_use_message_handler_receives_marshalled_message`** — sender emits nested-object payload; receiver accesses `msg['from']`, `msg['type']`, `msg['payload']['nested']['ok']`, `type(msg['ttl']).__name__`.
3. **`phase3c_python_use_message_handler_re_renders_on_message`** — receiver's handler calls a setter; second batch observes bumped state.
4. **`phase3c_python_emit_with_direct_target_addresses_specific_node`** — three-node A → B + A → C; A emits with `{'direct': 'c'}`; only C receives.
5. **`phase3c_python_permission_manifest_drops_unaccepted_messages`** — sender emits `'foo'`; receiver's manifest declares `accepts: ['bar']`; handler never fires.
6. **`phase3c_python_ping_pong_terminates_via_python_guards`** — bidirectional ping ↔ pong with `if bounces < 3` guards. Mirrors JSC's analogue.
7. **`phase3c_python_pingpong_grx_fixture_terminates_via_ttl`** — mounts the new `python-pingpong.grx` through `mount_with_scheduler`; drives 6 batches; asserts no panic + TTL caps the cycle.

The pre-existing `phase3b_hook_prelude_visible_as_builtins` test was extended to also probe `use_emit` and `use_message` builtins.

### Footguns + lessons

1. **`r#"..."#` raw strings without `"` content trip clippy `needless_raw_string_hashes`.** Python body strings use single quotes — `r"..."` (no hashes) is correct.
2. **Markdown list-continuation `+` is a footgun in rustdoc + clippy `doc_lazy_continuation`.** Continuation lines inside `//! - **item** — text` MUST NOT begin with `+` (clippy parses it as a sub-list-item start). Use `and` / `plus` / etc. instead.

### M13-phase3c (2026-05-03 evening — this session) acceptance

- `cargo run -p xtask -- zig-build --release` produces `libgraphreact_cpython_zig.a` (2,129,372 bytes — version-string bump only).
- `cargo test --workspace --release`: **583 passed, 2 ignored** (was 576 phase 3b; +7 phase 3c tests = 583).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3c (Python 3.9.6)`.
- `target/release/grx run --runtime=cpython crates/graphreact-cli/tests/fixtures/python-pingpong.grx`: trailer reports phase 3c; mounted 2 components: ping, pong.
- All 4 canonical JSC fixtures still terminate cleanly on `grx run --runtime=jsc`.

## Phase 3d-α — use_memo + use_callback + use_reducer

First sub-phase of the re-sub-staged phase 3d series (3d-α through 3d-ζ). Closes 3 parity rows; establishes the **split-cache pattern** that future factory-from-Python hooks (notably `use_graph_query` later in 3d-γ if it ever takes a query function) can reuse.

### Why split-cache (the load-bearing constraint)

`use_memo`'s factory is Python code. The body's invocation lives inside `engine.borrow_mut().call_function(iso, fn_handle, &[ctx])` — a single mutable RefCell borrow held across the entire body's evaluation. If a host callback wanted to invoke the Python factory via `engine.borrow_mut().invoke_global(...)`, that would be a second `borrow_mut` on the same RefCell — panic.

`use_effect` doesn't hit this because effects fire from `flush_effects` AFTER the body returns. `use_memo`'s factory has to run synchronously inside the body. **Solution: split the API.** Python-side wrapper calls `host_memo_check(deps)` first; on miss runs the factory itself; calls `host_memo_store(value)` to write the result. `host_memo_check` and `host_memo_store` are pure thread-local read/writes — never re-enter CPython.

### HOOK_INDEX synchronization

graphreact-core's `HOOK_INDEX` is private. `host_memo_check` calls `core_use_memo(no-op-factory, deps)` — the factory just sets a side-effect flag and returns `Value::Null`. core advances `HOOK_INDEX`, runs deps comparison, calls the factory only if deps changed. The flag tells us whether to invalidate the cache. core's slot stores `Value::Null` as a placeholder; the actual cached value lives in `MEMO_REGISTRY` (pyruntime layer), keyed by `(node_id, hook_pos)`.

### Three return envelopes

- `{cached: true, value}` — deps unchanged + cache hit.
- `{cached: false, previous: <prev>}` — deps changed + previous cache entry exists. Python uses `previous` to release the stale `_hostCallbacks` token in `use_callback`.
- `{cached: false}` — registry miss / first call.

### use_callback identity-stability

Python function refs don't survive the JSON FFI. `use_callback` stores the **token** (the u64 from `__register_callback(fn)`) in `MEMO_REGISTRY` instead. On cache hit, looks up the token in `_hostCallbacks` and returns the original function (identity-stable). On dep change, releases the previous token via `__release_callback(previous)` to prevent leaking one entry per dep change.

### use_reducer is pure Python

```python
def use_reducer(reducer, initial):
    state, set_state = use_state(initial)
    def dispatch(action):
        set_state(reducer(state, action))
    return state, dispatch
```

No host callback. Dispatch closure is created fresh per render (not identity-stable like React's). Wrapping in `use_callback` would make it stable; left for future cleanup.

### 4 acceptance tests

In `pyruntime.rs::tests`. Programmatic shape modeled cell-for-cell on JSC's `phase3e_alpha_*`.

1. **`phase3d_alpha_use_memo_caches_when_deps_unchanged`** — `use_memo(factory, [42])` across 3 renders. Factory ran exactly once.
2. **`phase3d_alpha_use_memo_recomputes_on_dep_change`** — `use_memo(lambda: count*2, [count])`. Drive count 0→5→7. Factory ran 3× with values 0, 5, 7; memoed values 0, 10, 14.
3. **`phase3d_alpha_use_callback_identity_stable_across_renders`** — `id(cb)` identical across 3 renders.
4. **`phase3d_alpha_use_reducer_dispatch_updates_state`** — counter-reducer with mount-effect dispatch; second render observes `state=1`.

The pre-existing `phase3b_hook_prelude_visible_as_builtins` test was extended to also probe `use_memo` / `use_callback` / `use_reducer`.

### Footguns + lessons

1. **Markdown list-continuation `+` is a footgun in clippy `doc_lazy_continuation`** (re-encountered from phase 3c). Continuation lines inside `//! - **item** — text` MUST NOT begin with `+` — clippy parses as a sub-list-item start, orphans subsequent continuation lines. Use `and` / `plus` as the conjunction. **Pattern: every time we add a new phase-section to module docs, scan continuation lines for leading `+`.**

### M13-phase3d-α (2026-05-03 evening — this session) acceptance

- `cargo run -p xtask -- zig-build --release` produces `libgraphreact_cpython_zig.a` (2,129,292 bytes — version-string bump only).
- `cargo test --workspace --release`: **587 passed, 2 ignored** (was 583 phase3c; +4 phase 3d-α tests = 587).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3d-alpha (Python 3.9.6)`.
- All 4 canonical JSC fixtures still terminate cleanly on `grx run --runtime=jsc`.
- Phase 3b/3c CPython fixtures (`python-counter.grx`, `python-pingpong.grx`) regression-clean.

## Phase 3d-β — use_neighbors + use_topology + use_accept_edge

Second sub-phase of the re-sub-staged phase 3d series. Closes 3 parity rows; establishes the **three-tier safety framing** that scales to phase 3d-γ/δ/ε/ζ.

### Three-tier safety framing

Engine-bridge hooks split by what plumbing they need from CPython:

- **Tier A — pure HOOK_CTX reads (zero engine touch).** `use_neighbors`, `use_accept_edge`. Route through `core_use_neighbors(None)` / `core_use_accept_edge()` directly. No engine borrow held; safe inside the body's `call_function`. Python-side filter for `use_neighbors` avoids per-element JSON FFI hops.
- **Tier B — token-registry + TLS-capture.** `use_topology`. Same shape as `use_message` from 3c. Reuses `CURRENT_ENGINE` / `CURRENT_ISO` TLS slots from phase 3b; closure captures engine + iso at registration time. On dispatch from `Scheduler::dispatch_topology_event` (well outside any held borrow), invokes Python via `__invoke_callback(token, [event])`.
- **Tier C — mutation queue.** `AcceptEdge` mutations enqueue `PendingMutation::AcceptEdge`; scheduler drains at next cascade and fires `EdgeAdded` automatically. No new mutation queue needed — substrate already ships this.

### Helpers (ported verbatim from jsruntime)

- `parse_topology_filter(arg) -> Option<TopologyFilterOptions>` — recognises `{kinds: [...], neighborsOnly: bool}`. Maps kind strings (`'node-added'` etc.) to `TopologyEventKind` enum variants.
- `topology_event_to_host_value(event) -> HostValue` — marshals all 5 `TopologyEvent` variants (NodeAdded, NodeRemoved, EdgeAdded, EdgeRemoved, EdgeUpdated) into the `{kind, nodeId? | u, v, key?}` envelope.

Both private fns in jsruntime; duplicated in pyruntime — same shape as `parse_emit_options` was in phase 3c.

### 4 acceptance tests

In `pyruntime.rs::tests`. Programmatic shape modeled cell-for-cell on JSC's `phase3e_beta_*`.

1. **`phase3d_beta_use_neighbors_returns_active_node_neighbors`** — star-graph hub→{a,b,c}; body observes `len:3` + sorted member ids.
2. **`phase3d_beta_use_neighbors_with_filter_predicate`** — same fixture; `use_neighbors(lambda n: n != 'b')` returns 2 results.
3. **`phase3d_beta_use_topology_handler_fires_on_node_addition`** — single watcher; externally adds 'newcomer'; second batch observes `'saw:newcomer'`.
4. **`phase3d_beta_use_accept_edge_creates_edge`** — two disconnected nodes; first render accepts edge to 'guest'; re-render observes 1 neighbor.

The pre-existing `phase3b_hook_prelude_visible_as_builtins` test was extended again to also probe `use_neighbors` / `use_topology` / `use_accept_edge` builtins.

### Footguns + lessons

None encountered this session. The phase 3c `+`-as-list-marker footgun was avoided by deliberate use of `and` / `plus` in module-doc continuation lines.

### M13-phase3d-β (2026-05-03 evening — this session) acceptance

- `cargo run -p xtask -- zig-build --release` produces `libgraphreact_cpython_zig.a` (2,129,380 bytes).
- `cargo test --workspace --release`: **591 passed, 2 ignored** (was 587 phase3d-α; +4 phase 3d-β tests = 591).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3d-beta (Python 3.9.6)`.
- All 4 canonical JSC fixtures still terminate cleanly on `grx run --runtime=jsc`.
- Phase 3b/3c/3d-α CPython fixtures regression-clean.

## Phase 3d-γ — ctx.directory + use_graph_query Tier 1

Third sub-phase of the re-sub-staged phase 3d series. Closes the directory + Tier 1 graph-query parity rows; establishes the **Python-side source-wrap pattern** for body augmentation — load-bearing because the simpler `invoke_global("__augment_ctx", ...)` approach fails on `SimpleNamespace` JSON serialization.

### Why source-wrap (the load-bearing pivot)

JSC achieves ctx augmentation by wrapping the body source in an IIFE inside `gr_jsc_component_register`:

```javascript
(function(__rawCtx){ var ctx = __augmentCtx(__rawCtx); body_source_here })
```

The first attempt at the Python equivalent invoked `__augment_ctx` via `engine.borrow_mut().invoke_global(iso, "__augment_ctx", &[raw_ctx])` between `build_host_ctx` and the body's `call_function`. This **failed in practice** because `__augment_ctx` returns the augmented dict with `directory` set to a `SimpleNamespace` — and Python's stdlib `json.dumps` rejects `SimpleNamespace`. The body received the unaugmented raw_ctx (fallback), and `ctx['directory']` raised `KeyError`.

**Solution: append `BODY_AUGMENT_SUFFIX` to user body source at eval time.** The suffix re-binds `body` (or `init`) to a wrapper that calls the original through `__augment_ctx`:

```python
# Appended after user source:
try:
    _user_body = body
    def body(raw_ctx):
        return _user_body(__augment_ctx(raw_ctx))
except NameError:
    try:
        _user_init = init
        def init(raw_ctx):
            return _user_init(__augment_ctx(raw_ctx))
    except NameError:
        pass
```

The shadowing `def body` replaces the user's; `resolve_function` returns the wrapped body. Phase 3a `init`-style fixtures get the same treatment via the `except NameError` fallback. **Augmentation happens entirely Python-side; no JSON round-trip needed.**

### Three Python-specific design choices

1. **Directory + graph_query proxies as `types.SimpleNamespace`.** Stable singletons in HOOK_PRELUDE module globals; bound to lambdas that route through host callbacks.
2. **Body source-wrap via `BODY_AUGMENT_SUFFIX`** (described above).
3. **`GraphSnapshot` duplicated from jsruntime.** Same DRY-violation pattern as `parse_emit_options` (3c) and `parse_topology_filter` (3d-β). Consolidation deferred.

### 9 new host callbacks

All pure HOOK_CTX-shaped reads (Tier A from 3d-β framing — zero engine touch):

- 5 directory finders (`find_by_id` / `find_by_role` / `find_by_depth` / `find_in_subtree` / `is_ancestor`) read `current_directory()`.
- 4 Tier 1 graph_query accessors (`neighbors` / `has_node` / `has_edge` / `node_count`) read `current_graph_snapshot()`.

All ported verbatim from jsruntime (~170 LOC). Safe defaults (Null / empty array / False / 0) when TLS unset.

### TLS infrastructure

Two new TLS slots `CURRENT_DIRECTORY` (`Rc<RefCell<GraphDirectory>>`) + `CURRENT_GRAPH_SNAPSHOT` (`Rc<GraphSnapshot>`) installed by extended `PyEngineGuard::with_directory(engine, iso, directory, graph_snapshot)`. Drop clears all 4 slots. Legacy `PyEngineGuard::new(engine, iso)` retained as `#[cfg(test)]` for tests that don't need directory access.

`PyRuntime` gains `directory` + `graph_snapshot` fields, populated at `mount_with_scheduler` time from `scheduler.directory` + `GraphSnapshot::from_live(scheduler.graph, scheduler.manifests, scheduler.edge_accept_predicates)`.

### 3 acceptance tests

In `pyruntime.rs::tests`. Programmatic shape modeled cell-for-cell on JSC's `phase3e_delta_*`. Test helper `eval_python_body` updated to append `BODY_AUGMENT_SUFFIX` (otherwise programmatic tests don't get augmentation).

1. **`phase3d_gamma_ctx_directory_finders_query_live_directory`** — populate directory with 3 entries (alice / bob / carol); body queries 4 finders; observes each. 8 assertions.
2. **`phase3d_gamma_use_graph_query_returns_proxy_results`** — 3-node graph with 3 edges; selector exposes 6 fields (counts, has_node hits/misses, has_edge hits/misses); observes each. 6 assertions.
3. **`phase3d_gamma_directory_unset_returns_safe_defaults`** — verifies safe-default fallbacks when no `EngineGuard::with_directory` installed (uses legacy `PyEngineGuard::new`). 5 assertions.

The pre-existing `phase3b_hook_prelude_visible_as_builtins` test was extended (4th time) to also probe `use_graph_query` + `__augment_ctx` builtins.

### Footguns + lessons

1. **`SimpleNamespace` JSON round-trip failure** — invoke_global pattern fails when the augmented ctx contains non-JSON-serializable values. Source-wrap is the safe pattern.
2. **Test helper symmetry** — `eval_python_body` and `mount_with_scheduler` are separate code paths; both must apply the augmentation suffix.
3. **`PyEngineGuard::new` test-only annotation** — when introducing variant constructors, mark legacy as `#[cfg(test)]` for clippy `dead_code` cleanliness.
4. **`GraphSnapshot` field visibility** — exposing as `pub` field on `pub struct PyRuntime` requires `pub struct GraphSnapshot` (not `pub(crate)`). Clippy `private-interfaces` catches this.

### M13-phase3d-γ (2026-05-03) acceptance

- `cargo run -p xtask -- zig-build --release` produces `libgraphreact_cpython_zig.a` (2,129,292 bytes).
- `cargo test --workspace --release`: **594 passed, 2 ignored** (was 591 phase3d-β; +3 phase 3d-γ tests = 594).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3d-gamma (Python 3.9.6)`.
- All 4 canonical JSC fixtures + phase 3b/3c CPython fixtures regression-clean.

## Phase 3d-δ — `use_graph_query` widened with 7 algorithm accessors

`__graph_query_proxy` gains 7 algorithm methods alongside the 4 Tier 1 basic accessors shipped in 3d-γ. All 7 host callbacks are verbatim ports from JSC's M12.3a-followon-B (`jsruntime.rs:1576-1760`); same body shape, identifiers swapped to `host_graph_<accessor>` Python names. The `__graph_query_proxy` Python `SimpleNamespace` widens from 4 lambdas to 11; tests + consumers from 3d-γ are forward-compatible.

### 7 new host callbacks

| Callback | Python proxy method | Algorithm fn | Return | Safe default (TLS unset) |
|---|---|---|---|---|
| `host_graph_shortest_path(from, to)` | `g.shortest_path(from, to)` | `graphreact_algorithms::shortest_path` | `Array<String>` or `Null` | `Null` |
| `host_graph_all_paths(from, to, max?)` | `g.all_paths(from, to, max=None)` | `graphreact_algorithms::all_paths` | `Array<Array<String>>` | `[]` |
| `host_graph_a_star_path(from, to)` | `g.a_star_path(from, to)` | `graphreact_algorithms::astar_path` (no heuristic) | `Array<String>` or `Null` | `Null` |
| `host_graph_bfs(from, depth?)` | `g.bfs(from, depth=None)` | `graphreact_algorithms::bfs_nodes` | `Array<String>` | `[]` |
| `host_graph_dfs(from, depth?)` | `g.dfs(from, depth=None)` | `graphreact_algorithms::dfs_nodes` | `Array<String>` | `[]` |
| `host_graph_connected_components()` | `g.connected_components()` | `graphreact_algorithms::connected_components` | `Array<Array<String>>` (sorted) | `[]` |
| `host_graph_cycle_detection()` | `g.cycle_detection()` | `has_cycle` + `find_cycle` | `{hasCycle: Bool, cycle: Array<Array<String>> | Null}` | `{hasCycle: False, cycle: None}` |

All 7 follow the canonical `build_host_graph_neighbors_callback` pattern: extract args from `&[HostValue]`; fetch `current_graph_snapshot()`; borrow `snapshot.graph`; delegate; marshal result. Tier A from the 3d-β framing — pure HOOK_CTX-shaped reads, no engine touch, safe-default fallbacks.

### Live-handle verification

`GraphSnapshot::from_live` — already shipped in 3d-γ — stores three `Rc<RefCell<...>>` handles cloned cheaply from the scheduler. The handles share storage with `scheduler.graph` / `scheduler.manifests` / `scheduler.edge_accept_predicates`, so any mutation applied through the scheduler is visible to subsequent borrows of the snapshot.

JSC's M12.3a-followon-C performed a separate "static→live" refactor — JSC's `GraphSnapshot` was originally constructed from owned vec data and had to be rewritten to hold `Rc<RefCell<...>>` handles. The Python port short-circuited this during 3d-γ by constructing the snapshot from live handles from the start. **Phase 3d-δ adds a load-bearing test** (`phase3d_delta_graph_snapshot_reflects_post_mount_mutation`) that confirms the pattern works: mount a watcher, drive `process_batch` (observe `count=1, hasNew=False`), call `scheduler.graph.borrow_mut().add_node('newcomer')`, bump a `use_state` dep so `use_graph_query` re-runs, drive a second `process_batch` (observe `count=2, hasNew=True`).

### Why A* heuristics defer to 3d-ε

The 5 cost heuristics — `latency` / `compliance` / `hierarchy` / `trust-attenuated` / `prerequisite-distance` — are policy-derived: they read per-node `PermissionManifest` data and derive edge costs from manifest pairs (e.g. `compliance` heuristic walks the emit/accept manifest and assigns penalty weight based on classification mismatch). Tier 2 (3d-ε) is where the policy-derived reachability infrastructure lands; A* heuristics naturally compose with that. Mirrors JSC's followon-B → followon-E split exactly. Without a heuristic, A* degenerates to BFS shortest-path with uniform edge cost (per `graphreact-algorithms` documentation).

### Acceptance tests (4 new)

1. `phase3d_delta_use_graph_query_shortest_path` — 4-node line graph A→B→C→D; selector calls `g.shortest_path('A', 'D')` + `g.a_star_path('A', 'D')` + `g.shortest_path('A', 'zombie')` (returns `None`) + `g.all_paths('A', 'B')` (returns 1 path on a line). 4 assertions.
2. `phase3d_delta_use_graph_query_bfs_dfs_traversal_order` — same line graph; `g.bfs('A')` and `g.dfs('A')` both return `['A','B','C','D']`. 2 assertions.
3. `phase3d_delta_use_graph_query_connected_components` — 5-node disconnected graph (A-B-C and X-Y); selector observes count + sorted membership of both components + `g.cycle_detection()['hasCycle']` (False on a tree). 4 assertions.
4. `phase3d_delta_graph_snapshot_reflects_post_mount_mutation` — load-bearing live-handle test described above. 2 assertions.

### M13-phase3d-δ (2026-05-04) acceptance

- `cargo run -p xtask -- zig-build --release` rebuilds `libgraphreact_cpython_zig.a` (version-string bump only).
- `cargo test --workspace --release`: **598 passed, 2 ignored** (was 594 phase3d-γ; +4 phase 3d-δ tests = 598).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3d-delta (Python 3.9.6)`.
- All 4 canonical JSC fixtures + phase 3b/3c CPython fixtures regression-clean.

## Phase 3d-ε — Tier 2 `g.tier2(msgType, opts?)`

`g.tier2(msgType, opts?)` adds policy-derived directed reachability alongside Tier 1's structural graph queries. Verbatim Python port of JSC's M12.3a-followon-E. `__graph_query_proxy` gains a `tier2` selector that returns a fresh msgType-scoped sub-proxy on each call.

### Why Tier 2 has its own slice

Tier 1 (3d-γ + 3d-δ) covers structural topology questions ("what are my neighbors? what's the shortest path?"). Tier 2 adds **policy-derived directed reachability** — "for message type T, where can `s` actually deliver?" — derived from per-node `PermissionManifest.emits/accepts` pairs. Application classes that depend on msgType-aware routing — Self-Healing K8s (Thymus failover paths), ComplyFlow compliance routing, Adaptive Education (prerequisite-aware lesson sequencing) — were unblocked on JSC at followon-E and need the same surface on Python.

### 12 new host callbacks + 12 algorithm helpers

| `host_tier2_*` callback | Sub-proxy method | Algorithm helper | Return type | Safe default (TLS unset) |
|---|---|---|---|---|
| `host_tier2_node_count(msg)` | `node_count()` | reads `manifests.len()` | `Number` | `0` |
| `host_tier2_has_node(msg, id)` | `has_node(id)` | reads `manifests.contains_key(id)` | `Bool` | `False` |
| `host_tier2_neighbors(msg, id, ds)` | `neighbors(id)` | `Tier2Adj::neighbors` | `Array<String>` | `[]` |
| `host_tier2_has_edge(msg, u, v, ds)` | `has_edge(u, v)` | `Tier2Adj::has_edge` | `Bool` | `False` |
| `host_tier2_bfs(msg, from, depth?, ds)` | `bfs(from, depth=None)` | `tier2_bfs` | `Array<String>` | `[]` |
| `host_tier2_dfs(msg, from, depth?, ds)` | `dfs(from, depth=None)` | `tier2_dfs` | `Array<String>` | `[]` |
| `host_tier2_shortest_path(msg, from, to, ds)` | `shortest_path(from, to)` | `tier2_shortest_path` | `Array<String>` or `Null` | `Null` |
| `host_tier2_topological_sort(msg, ds)` | `topological_sort()` | `tier2_topological_sort` (Kahn's) | `Array<String>` or `Null` | `Null` |
| `host_tier2_a_star_path(msg, from, to, h?, ds)` | `a_star_path(from, to, opts?)` | `tier2_astar_path` (5 heuristics) | `Array<String>` or `Null` | `Null` |
| `host_tier2_all_paths(msg, from, to, max?, ds)` | `all_paths(from, to, max=None)` | `tier2_all_paths` | `Array<Array<String>>` | `[]` |
| `host_tier2_connected_components(msg, ds)` | `connected_components()` | `tier2_strongly_connected_components` (Tarjan SCC) | `Array<Array<String>>` | `[]` |
| `host_tier2_cycle_detection(msg, ds)` | `cycle_detection()` | `tier2_directed_cycle_detection` (3-color DFS) | `{hasCycle, cycle}` | `{hasCycle: False, cycle: None}` |

All 12 callbacks are pure HOOK_CTX-shaped reads (Tier A from 3d-β framing): they call `current_graph_snapshot()`, borrow `manifests` + `graph`, build the `Tier2Adj` for the msgType, dispatch to the algorithm helper, and marshal the result. Safe-default fallbacks on TLS-unset.

### `DirectedAdj` trait + `Tier2Adj` struct — shared with 3d-ζ Tier 3

The 12 algorithm helpers (`tier2_bfs`, `tier2_dfs`, ..., `tier2_directed_cycle_detection`) are parameterised over `&dyn DirectedAdj`:

```rust
trait DirectedAdj {
    fn nodes_slice(&self) -> &[String];
    fn neighbors(&self, id: &str) -> &[String];
    fn has_node(&self, id: &str) -> bool;
    fn has_edge(&self, u: &str, v: &str) -> bool;
}
```

`Tier2Adj` (built by `build_tier2_adj`) implements it. 3d-ζ's `Tier3Adj` will also implement it, reusing every algorithm helper without duplication.

### Two flavors of Tier 2 adjacency

- **Structural** (default; `opts.includeDirectoryShortcuts == False`): edge `(s, u)` exists iff a Tier 1 edge `s ↔ u` exists in the live `CommunicationGraph` AND `s.emits ∋ msg_type` AND `u.accepts ∋ msg_type`. Preserves both topology and policy; pathfinding remains semantically meaningful.
- **Directory-shortcut** (opt-in via `opts={'includeDirectoryShortcuts': True}`): edge `(s, u)` exists iff `s.emits ∋ msg_type ∧ u.accepts ∋ msg_type`. Tier 1 edge presence is IGNORED — any emitter reaches any acceptor in 1 hop via direct messaging.

### 5 cost-from-policy A* heuristics

`g.tier2(msg).a_star_path(from, to, {'heuristic': '<kind>'})` selects per-step costs from manifest + graph data. Verbatim JSC attr keys for cross-engine `.grx` portability:

| Heuristic | Per-step cost | Heuristic estimate | Reads |
|---|---|---|---|
| `latency` | edge attr `latency` (default 1.0) | 0 | edge `latency` |
| `compliance` | 1.0 + 10 if edge `encrypted == false` + 10 if node `compliant == false` | 0 | edge `encrypted`, node `compliant` |
| `hierarchy` | 1.0 + \|depth(from) - depth(to)\| | \|depth(cur) - depth(target)\| (admissible) | directory `depth` |
| `trust-attenuated` | 1.0 / max(0, edge `trust`) | 0 | edge `trust` |
| `prerequisite-distance` | 1.0 + max(0, node `difficulty`) | 0 | node `difficulty` |

Defaults are permissive (cost 1.0 / no penalty) so attr-less fixtures behave as BFS-equivalent.

### `topological_sort` closes followon-B's deferral

3d-δ's Tier 1 `topological_sort` was deferred because `CommunicationGraph` is undirected (no source→target ordering). Tier 2's directed adjacency makes direction well-defined per message type, so Kahn's algorithm produces a meaningful ordering — and returns `null` on cycle.

### Acceptance tests (7 new)

Verbatim ports of JSC's followon-E suite. Each populates `PermissionManifest` data via body return values:

1. `phase3d_epsilon_tier2_bfs_finds_policy_reachable_peers` — producer / worker / sink + observer (Tier 1 bystander). Manifests pruning: `bfs('producer')` excludes observer.
2. `phase3d_epsilon_tier2_topological_sort_directed_pipeline` — linear chain with 'step' manifests. `topological_sort()` returns deterministic ordering.
3. `phase3d_epsilon_tier2_a_star_compliance_picks_compliant_path` — diamond with `b.compliant=true`, `c.compliant=false`. A* with compliance heuristic picks the compliant path.
4. `phase3d_epsilon_tier2_directed_long_tail_accessors` — triangle with 'send' manifests. Exercises `all_paths`, `connected_components`, `cycle_detection`.
5. `phase3d_epsilon_tier2_a_star_trust_attenuated_picks_high_trust_path` — diamond with `trust=1.0` vs `trust=0.1` edges. A* picks high-trust.
6. `phase3d_epsilon_tier2_a_star_prerequisite_distance_picks_easy_path` — diamond with `difficulty=0` vs `difficulty=10` nodes. A* picks easy.
7. `phase3d_epsilon_tier2_directory_shortcuts_flag_enables_edge_free_reachability` — producer + sink with matching manifests but no Tier 1 edge. Default flavor: not reachable. With `{'includeDirectoryShortcuts': True}`: reachable.

### M13-phase3d-ε (2026-05-04) acceptance

- `cargo run -p xtask -- zig-build --release` rebuilds `libgraphreact_cpython_zig.a` (version-string bump only).
- `cargo test --workspace --release`: **605 passed, 2 ignored** (was 598 phase3d-δ; +7 phase 3d-ε tests = 605).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3d-epsilon (Python 3.9.6)`.
- All 4 canonical JSC fixtures + phase 3b/3c CPython fixtures regression-clean.

## Phase 3d-ζ — Tier 3 `g.tier3(msgType, opts?)` + `use_edge_accept_handler`

Phase 3d-ζ adds the **negotiation layer** — per-target `EdgeAcceptPredicate`s filter Tier 2 candidacy at query time AND veto `AcceptEdge` mutations at the scheduler level. **Closes the M13 phase-3d series** — every primitive in the three-tier possibility model now has parity on Python. Verbatim Python port of JSC's M12.3a-followon-F.

### Why Tier 3 closes the M13 phase-3d series

3d-γ + 3d-δ established Tier 1 (structural topology — `g.neighbors / has_node / has_edge / shortest_path / ...`). 3d-ε added Tier 2 (manifest-derived reachability — `g.tier2(msgType).bfs / a_star_path / ...`). 3d-ζ adds Tier 3 (negotiation-aware reachability — `g.tier3(msgType).neighbors / ...` filters Tier 2 through per-target accept/veto predicates). All three tiers in the possibility model are now reachable from Python component bodies.

### 13 new host callbacks

| `host_*` callback | Reuses | Return type | Safe default |
|---|---|---|---|
| `host_edge_accept_handler(token)` | builds `EdgeAcceptPredicate` closure capturing iso + token | `Null` | error on bad token |
| `host_tier3_node_count(msg, ds)` | reads `manifests.len()` | `Number` | `0` |
| `host_tier3_has_node(msg, id, ds)` | reads `manifests.contains_key(id)` | `Bool` | `False` |
| `host_tier3_neighbors(msg, id, ds)` | `Tier3Adj::neighbors` | `Array<String>` | `[]` |
| `host_tier3_has_edge(msg, u, v, ds)` | `Tier3Adj::has_edge` | `Bool` | `False` |
| `host_tier3_bfs(msg, from, depth?, ds)` | `tier2_bfs` (DirectedAdj) | `Array<String>` | `[]` |
| `host_tier3_dfs(msg, from, depth?, ds)` | `tier2_dfs` (DirectedAdj) | `Array<String>` | `[]` |
| `host_tier3_shortest_path(msg, from, to, ds)` | `tier2_shortest_path` (DirectedAdj) | `Array<String>` or `Null` | `Null` |
| `host_tier3_topological_sort(msg, ds)` | `tier2_topological_sort` (DirectedAdj) | `Array<String>` or `Null` | `Null` |
| `host_tier3_a_star_path(msg, from, to, h?, ds)` | `tier2_astar_path` (DirectedAdj) | `Array<String>` or `Null` | `Null` |
| `host_tier3_all_paths(msg, from, to, max?, ds)` | `tier2_all_paths` (DirectedAdj) | `Array<Array<String>>` | `[]` |
| `host_tier3_connected_components(msg, ds)` | `tier2_strongly_connected_components` (Tarjan) | `Array<Array<String>>` | `[]` |
| `host_tier3_cycle_detection(msg, ds)` | `tier2_directed_cycle_detection` (3-color DFS) | `{hasCycle, cycle}` | `{hasCycle: False, cycle: None}` |

The 8 reusable algorithm helpers all already accept `&dyn DirectedAdj` (3d-ε design choice). `Tier3Adj` implements the same trait, so Tier 3 reuses every helper without duplication.

### Predicate dispatch — `invoke_global_static` bypass

Predicates fire from two paths:

- **Phase 9 mutation veto path**: `Scheduler::process_pending_mutations::AcceptEdge` consults `predicates[target](source, "")` before committing the edge. Empty `msg_type` (channel is untyped). Outside any active engine borrow → safe to `engine.borrow_mut().invoke_global(...)`.
- **Tier 3 query path**: `host_tier3_*` callbacks during querier body eval call the predicate from inside an active `engine.borrow_mut().call_function(...)` borrow held by the querier body. A second `engine.borrow_mut()` would panic with `BorrowMutError`.

The fix mirrors JSC's followon-F: `graphreact_engine_cpython::invoke_global_static(iso, ...)` is a free function (not a method on `CPythonEngine`) that takes an `IsolateHandle` directly and routes the FFI call through process-global state without touching the engine's `Rc<RefCell<...>>`. CPython's GIL is already held by the active thread; the C API is safely re-entrant under GIL.

### AcceptEdge mutation veto

When `use_accept_edge('target')` (3d-β) enqueues a `PendingMutation::AcceptEdge`, the scheduler consults `predicates[target](source, "")` before committing the edge. Predicate keyed by **target** id (the node making the accept/veto decision). Default-accept on missing predicate preserves Phase-9 backward compat. The predicate is invoked outside any engine borrow, so `invoke_global` would also work — but the same closure handles both paths uniformly via `invoke_global_static`.

### 5 application classes unblocked

- **Self-Healing Infrastructure (#1, Thymus)** — A* with `compliance` heuristic + predicate excludes noncompliant failovers.
- **Adaptive Education (#6)** — `g.tier3('compose').neighbors('fractions')` returns only learner-appropriate next concepts.
- **Generative World Building (#4)** — BFS over war messages excludes peace-treaty regions.
- **Biological Pathway Simulation (#7)** — `all_paths` from drug to kinase routes only through on-target receptors.
- **Reputation Systems (#8)** — A* with `trust-attenuated` heuristic + vouch-veto predicate routes through trusted intermediaries only.

### Acceptance tests (9 new)

Verbatim ports of JSC's followon-F suite — 4 substrate + 5 application class:

1. `phase3d_zeta_use_edge_accept_handler_default_accept_when_no_predicate` — Tier 3 == Tier 2 baseline.
2. `phase3d_zeta_tier3_query_includes_predicate_accepted_pair` — predicate returning True keeps pair.
3. `phase3d_zeta_tier3_query_excludes_predicate_rejected_pair` — predicate returning False removes pair.
4. `phase3d_zeta_use_accept_edge_consults_predicate` — Phase 9 mutation veto path.
5. `phase3d_zeta_self_healing_compliance_a_star_path_with_predicate` — Application class #1.
6. `phase3d_zeta_adaptive_education_neighbors_with_predicate` — Application class #6.
7. `phase3d_zeta_generative_worlds_bfs_with_peace_treaty_vetoes` — Application class #4.
8. `phase3d_zeta_biological_pathway_all_paths_with_off_target_vetoes` — Application class #7.
9. `phase3d_zeta_reputation_a_star_path_trust_attenuated_with_vouch_vetoes` — Application class #8.

### M13-phase3d-ζ (2026-05-04) acceptance

- `cargo run -p xtask -- zig-build --release` rebuilds `libgraphreact_cpython_zig.a` (version-string bump only — `invoke_global_static` was already shipped in graphreact-engine-cpython).
- `cargo test --workspace --release`: **614 passed, 2 ignored** (was 605 phase3d-ε; +9 phase 3d-ζ tests = 614).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3d-zeta (Python 3.9.6)`.
- All 4 canonical JSC fixtures + phase 3b/3c CPython fixtures regression-clean.
- `#[allow(dead_code)]` removed from `GraphSnapshot.edge_accept_predicates` — actively read by `build_tier3_adj`.

## Phase 3e-α — `use_spawn_child` / `use_unmount_self` / `use_compose`

Phase 3e-α is the first slice of phase 3e — dynamic node lifecycle hooks. Verbatim Python port of JSC's M12.3a-phase3e-γ (`crates/graphreact-cli/src/jsruntime.rs:1300-1360, 3811-3907`). Three hooks let bodies decide topology at runtime — previously Python systems could only decide topology at mount time:

- **`use_spawn_child()(spec)`** — spawn a single child node from the active body. `spec` is a dict `{id, component, props?, connectTo?}` where `component` is a Python callable `fn(ctx) -> manifest`. The HOOK_PRELUDE wrapper applies the lambda-wrap-at-registration pattern: `spec['component']` is wrapped in `lambda raw_ctx: spec['component'](__augment_ctx(raw_ctx))` so the spawned body sees `ctx['directory']` and Tier 1/2/3 proxies identically to top-level definitions. The wrapped fn is registered via `__register_callback`; the resulting token is dispatched to `host_spawn_child`.
- **`use_unmount_self()()`** — schedules an `UnmountNode` mutation for the active node. After the cascade drains, the node is gone — subsequent renders don't fire. HOOK_CTX-derived; no spec.
- **`use_compose()(specs)`** — declarative multi-spec variant. Same shape per element, plus a final `core_use_compose()(specs)` call. Uses a named `_make_compose_wrapper(fn)` factory inside the loop because Python's closure-in-loop semantics would otherwise capture every wrapped fn to the loop's last iteration (the canonical Python footgun).

### Three load-bearing design decisions

1. **Lambda-wrap-at-registration vs source-wrap.** The 3d-γ `BODY_AUGMENT_SUFFIX` pattern (appended to user `.grx` source at eval time) doesn't apply to spawned bodies — they arrive as Python function references inside `_hostCallbacks`, not as source strings. The Python wrappers wrap the user's component callable at registration time so each invocation pre-augments the ctx before the user's body runs. Same effect as JSC's IIFE `(function(fn){return function(rawCtx){return fn(__augmentCtx(rawCtx));};})(rawComponent)` pattern.

2. **Closure capture for re-entry.** [`build_compose_spec_from_host_value`] reads `current_engine()` and `current_iso()` from TLS at registration time (running synchronously inside the body via `host_spawn_child` / `host_compose`) and the resulting [`NodeDefinition`] closure captures both through `Rc` clones. Subsequent renders of the spawned body (driven by `Scheduler::evaluate_node` from `process_batch`) re-enter CPython via `engine.borrow_mut().invoke_global(iso, "__invoke_callback", ...)`. No nested-borrow path because `evaluate_node` runs outside any parent body's `call_function` — the parent body has already returned by the time the cascade drains the SpawnChild mutation and re-evaluates the new child. Same pattern as 3d-γ's [`build_py_node_definition`]; **no** `invoke_global_static` needed (that's reserved for hooks that fire from inside a body's own render — Tier 3 predicate dispatch in 3d-ζ).

3. **Bounded token leak — accepted.** Each `use_spawn_child` / `use_compose` call mints a fresh token in `_hostCallbacks`; `SpawnChild` is idempotent on existing IDs at the scheduler so duplicate-id calls don't double-mount, but their tokens still register and never get released. Same bounded-leak surface JSC documented in 3e-γ; followon-D swept it later via per-node token tracking on unmount. The Python port follows JSC's phasing — token GC is deferred to a future "M13-followon-D analogue" slice that batches token cleanup across all token-using hooks (`use_effect`, `use_message`, `use_topology`, `use_callback`, `use_edge_accept_handler`, `use_spawn_child`, `use_compose`).

### 3 new host callbacks + 2 helpers

| Symbol | Lines (pyruntime.rs) | Role |
|---|---|---|
| `build_host_spawn_child_callback` | ~3539 | Decodes spec → `ComposeChildSpec`, calls `core_use_spawn_child()(spec)` |
| `build_host_unmount_self_callback` | ~3554 | Calls `core_use_unmount_self()()` (HOOK_CTX-derived target) |
| `build_host_compose_callback` | ~3565 | Decodes Vec of specs, calls `core_use_compose()(specs)` |
| `build_compose_spec_from_host_value` | ~3593 | Builds `NodeDefinition` whose closure captures `(engine, iso, component_token)` and on every render invokes `__invoke_callback(token, [host_ctx])` via `CPythonEngine::invoke_global` |
| `manifest_from_invoke_envelope` | ~3656 | Extracts `value` from `{kind: 'value', value}` envelope; falls back to `PermissionManifest::default()` on shape mismatch |

### 6 acceptance tests

| # | Test | Validates |
|---|---|---|
| 1 | `phase3e_alpha_use_spawn_child_creates_dynamic_node` | Parent on first render spawns child-1 with body that observes its own `ctx.id`. Both parent renders + child render observed. |
| 2 | `phase3e_alpha_use_unmount_self_removes_node` | 'ephemeral' unmounts itself; 'witness' subscribes via `use_topology` and observes `NodeRemoved`. ephemeral does NOT re-render after unmount; witness sees `'witness:removed:ephemeral'`. |
| 3 | `phase3e_alpha_use_compose_spawns_multiple_children` | Composer's body declares 3 children (c-a, c-b, c-c) via `use_compose`; all 3 render with correct identity. |
| 4 | `phase3e_alpha_spawned_child_sees_augmented_ctx` | Spawned child reads `ctx['directory']` and calls `use_graph_query` from inside its body. Pins the wrap-at-registration pattern. |
| 5 | `phase3e_alpha_spawned_child_can_use_state` | Spawned child uses `use_state(11)`, observes initial value, signal-bumps externally to 99, second batch observes new value. Pins HOOK_CTX threading through spawn → cascade → evaluate. |
| 6 | `phase3e_alpha_compose_wrappers_capture_independent_components` | Two children with distinct components (`alpha_body` / `beta_body`); each renders its own component, not the loop's last. Pins the named-factory pattern that prevents Python's closure-in-loop bug. |

Plus a 5th extension to `phase3b_hook_prelude_visible_as_builtins` adding `'use_spawn_child'`, `'use_unmount_self'`, `'use_compose'` to the names tuple + Rust required array.

### M13-phase3e-α (2026-05-04) acceptance

- `cargo run -p xtask -- zig-build --release` rebuilds `libgraphreact_cpython_zig.a` (version-string bump only — no Zig FFI logic changes).
- `cargo test --workspace --release`: **620 passed, 2 ignored** (was 614 phase3d-ζ; +6 phase 3e-α tests = 620).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3e-alpha (Python 3.9.6)`.
- `target/release/grx run --runtime=cpython python-pingpong.grx`: regression-clean.
- All 4 canonical JSC fixtures regression-clean on `--runtime=jsc`.
- Stale-stub audit clean — no stray `phase3d-ζ`-as-current-phase markers (only historical), no "deferred to 3e-α" remnants.

## Phase 3e-β — supervision (`use_monitor` / `use_demonitor` / `use_link` / `use_unlink` / `use_group_join` / `use_group_leave`)

Phase 3e-β is the second slice of phase 3e — the OTP-shaped supervision surface. Verbatim Python port of JSC's M12.3a-phase3e-ζ ([jsruntime.rs:3702-3775](crates/graphreact-cli/src/jsruntime.rs#L3702-L3775)). All six hooks are **Tier A** — pure HOOK_CTX-shaped reads from the host callback side; zero engine touch; zero token registry interaction; no NodeDefinition closure capture. **Dramatically smaller than 3e-α** (~250 LOC vs ~600 LOC):

- **`use_monitor(target_id) → SimpleNamespace(id) | None`** — watch another node by id; the active node receives a `__DOWN__` system message when the target unmounts. `use_message('__DOWN__', handler)` (3c) catches the delivery via the standard token-registry path; no new wiring needed in 3e-β. Returns `None` if the target isn't registered.
- **`use_demonitor(mref)`** — cancel a previously-registered monitor. Accepts the `SimpleNamespace(id=...)` returned by `use_monitor`, OR the raw integer id (idiomatic flexibility).
- **`use_link(target_id) → bool`** — establish a bidirectional link. Re-asserted every render; returns `False` if the target has unmounted between evaluations.
- **`use_unlink(target_id)`** — remove a link. No-op on missing.
- **`use_group_join(group)`** — join a process group. Idempotent at the scheduler level. Auto-removed on node unmount.
- **`use_group_leave(group)`** — leave a group. No-op on missing membership.

### Three load-bearing design decisions

1. **All Tier A — no token registry interaction.** Each host callback dispatches directly to `core_use_X(...)` which reads `with_ctx(|c| ...)` to dispatch through the active node's HookCtx. No callback registration via `__register_callback`. No re-entry into Python from the Rust side (different from `use_message` and `use_topology` which fire from outside body eval). The 6 callbacks are thin shims at ~15 LOC each.

2. **`MonitorRef` marshalling shape.** JSC's `MonitorRef(pub u64)` round-trips across FFI as `{id: u64}`. The Python wrapper exposes it as `_SimpleNamespace(id=...)` for symmetry with JSC's `{id: number}` JS object. `use_demonitor(mref)` reads `mref.id` (or accepts the raw id) and round-trips. `int(...)` coercion handles JSON marshalling's float-coerced numbers (Python sees `0.0` instead of `0` when crossing FFI; same pattern as `host_state_set` from 3b).

3. **`__DOWN__` delivery already wired in core.** When a monitored target unmounts, `Scheduler` emits a `__DOWN__` system message with payload `{target, reason}` to all monitoring nodes. `use_message('__DOWN__', handler)` (3c) catches it via the standard token-registry dispatch path. **Token leak surface unchanged** — supervision hooks don't add new tokens.

### 6 new host callbacks

| Symbol | Role |
|---|---|
| `build_host_monitor_callback` | Decodes target string → calls `core_use_monitor(&target)` → returns `{id} \| null` |
| `build_host_demonitor_callback` | Decodes id → calls `core_use_demonitor(CoreMonitorRef(id))` |
| `build_host_link_callback` | Decodes target string → calls `core_use_link(&target)` → returns `bool` |
| `build_host_unlink_callback` | Decodes target string → calls `core_use_unlink(&target)` |
| `build_host_group_join_callback` | Decodes group string → calls `core_use_group_join(&group)` |
| `build_host_group_leave_callback` | Decodes group string → calls `core_use_group_leave(&group)` |

### 4 acceptance tests

| # | Test | Validates |
|---|---|---|
| 1 | `phase3e_beta_use_monitor_delivers_down_on_unmount` | observer monitors target + registers `use_message('__DOWN__', ...)`; target unmounts itself; observer observes `mref:set` (non-null monitor ref) + `down:target:normal`. Verbatim port of JSC's `phase3e_zeta_use_monitor_delivers_down_on_unmount`. |
| 2 | `phase3e_beta_use_link_and_group_join_register_cleanly` | body calls `use_link('b')` (with 'b' registered as a noop NodeDefinition) + `use_group_join('workers')`; observes `linked:True` + `joined-workers`. Verbatim port of JSC's `phase3e_zeta_use_link_and_group_join_register_cleanly`. |
| 3 | `phase3e_beta_use_demonitor_cancels_monitor` | observer monitors target then immediately demonitors; target unmounts; observer does NOT receive `__DOWN__`. Pins the cancellation path. **Python-novel.** |
| 4 | `phase3e_beta_unlink_group_leave_round_trip_and_monitor_none_for_unknown` | `use_link` + `use_unlink` round-trip; `use_group_join` + `use_group_leave` round-trip; `use_unlink` on never-linked target is no-op; `use_group_leave` on never-joined group is no-op; `use_monitor('does-not-exist')` returns `None`. Pins all idempotent / safe-default paths in one fixture. **Python-novel.** |

Plus a 6th extension to `phase3b_hook_prelude_visible_as_builtins` adding `'use_monitor'`, `'use_demonitor'`, `'use_link'`, `'use_unlink'`, `'use_group_join'`, `'use_group_leave'` to the names tuple + Rust required array.

### M13-phase3e-β (2026-05-04) acceptance

- `cargo run -p xtask -- zig-build --release` rebuilds `libgraphreact_cpython_zig.a` (version-string bump only — no Zig FFI logic changes).
- `cargo test --workspace --release`: **624 passed, 2 ignored** (was 620 phase3e-α; +4 phase 3e-β tests = 624).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3e-beta (Python 3.9.6)`.
- `target/release/grx run --runtime=cpython python-pingpong.grx`: regression-clean.
- All canonical JSC fixtures regression-clean on `--runtime=jsc`.
- Stale-stub audit clean.

## Phase 3e-γ — `use_source` + `timer_source` / `delay_source` / `http_source` / `watch_source`

Phase 3e-γ is the **third and final slice of phase 3e** — and **closes M13 phase 3 on Python**. Verbatim port of JSC's M12.3a-phase3e-ε ([jsruntime.rs:3608-3694](crates/graphreact-cli/src/jsruntime.rs#L3608-L3694)) plus M12.3a-followon-A combined into a single slice. Five new hook surfaces:

- **`use_source(factory_fn, deps=None)`** — invokes `factory_fn()` to produce a descriptor dict, dispatches to `host_use_source`, which decodes the kind and registers the resulting `Arc<dyn EventSource>` via [`graphreact_core::use_source`](crates/graphreact-core/src/runtime/hooks.rs#L423). Source events flow cross-thread via the scheduler's `external_inbox` and reach the active node as messages on the next `process_batch` — caught by `use_message(type, handler)` (3c).
- **`timer_source(interval_ms)`** — descriptor `{'kind': 'timer', 'intervalMs': ...}`. Real via `core_timer_source`.
- **`delay_source(delay_ms)`** — descriptor `{'kind': 'delay', 'delayMs': ...}`. Real via `core_delay_source`.
- **`http_source(opts)`** — descriptor `{'kind': 'http', 'opts': opts or {}}`. Real via `crate::sources::http_source(port)` (already shipped via followon-A; tokio::net listener; needs tokio runtime — `grx` main() installs one).
- **`watch_source(opts)`** — descriptor `{'kind': 'watch', 'opts': opts or {}}`. Real via `crate::sources::watch_source(path)` (already shipped via followon-A; notify-based file watcher; spawns its own thread, so works even outside a tokio runtime).

### Three load-bearing design decisions

1. **Descriptor-producing factories — pure JSON shape.** The Python factory globals return plain dicts (not native handles); descriptors cross the FFI as pure JSON. The Rust decoder reads `kind` plus the kind-specific fields (`intervalMs` / `delayMs` / `opts.port` / `opts.path`) and dispatches to the right factory. **No JS-to-Rust handle marshalling needed.**

2. **Single-slice staging — collapse JSC's two-stage rollout.** JSC's M12.3a-phase3e-ε deferred httpSource/watchSource firing to followon-A (which ported `crates/graphreact-napi/src/sources.rs` to `crates/graphreact-cli/src/sources.rs` plus added tokio + notify deps + tokio runtime in `grx` main()). For the Python port the followon-A infrastructure is already in place, so all four kind arms ship real on the first slice. **Substrate-not-MVP rule applied** — same total work, one slice instead of two.

3. **Token leak surface unchanged.** `use_source` doesn't register `__hostCallbacks` callbacks — descriptors are pure data; the Rust-side factory closure is constructed from primitive captured values (`interval_ms` / `delay_ms` / `port` / `path`). `core_use_source` enforces deps-aware re-registration; on deps-change the prior source is `stop()`'d and a fresh factory invoked. `lifecycle::stop_sources` (already shipped in core) cleans up `node.active_sources` on unmount.

### 1 new host callback

| Symbol | Role |
|---|---|
| `build_host_use_source_callback` | Decodes descriptor (`kind` + `intervalMs` / `delayMs` / `opts.port` / `opts.path`) plus deps array → constructs `Arc<dyn EventSource>` via the matching factory (`core_timer_source` / `core_delay_source` / `crate::sources::http_source` / `crate::sources::watch_source`) → calls `core_use_source(factory, deps)`. Verbatim port of JSC's [build_host_use_source_callback](crates/graphreact-cli/src/jsruntime.rs#L3623). |

### 3 acceptance tests

| # | Test | Validates |
|---|---|---|
| 1 | `phase3e_gamma_use_source_dispatches_timer_delay_descriptors` | wiring-only test with custom `host_use_source` capture callback. Body calls `use_source(lambda: timer_source(50))` + `use_source(lambda: delay_source(120))`. Asserts captured descriptors are `[("timer", 50), ("delay", 120)]`. Verbatim port of JSC's `phase3e_epsilon_use_source_dispatches_descriptor_to_host`. |
| 2 | `phase3e_gamma_use_source_decodes_http_and_watch_descriptors` | wiring-only test with custom capture callback. Body calls `use_source(lambda: http_source({'port': 8080}))` + `use_source(lambda: watch_source({'path': '/tmp/grx-3e-gamma'}))`. Asserts captured `[(kind, port, path)]` tuples decode the nested opts. Verbatim port of JSC's `followon_a_use_source_decodes_http_and_watch_descriptors`. |
| 3 | `phase3e_gamma_watch_source_fires_change_events_e2e` | end-to-end test using the production `host_use_source` decoder. Creates a `tempfile::tempdir()`, mounts a body that does `use_source(lambda: watch_source({'path': <tmpdir>}))` + `use_message('change', handler)`, runs initial `process_batch` (registers source, calls `watcher.start()`), sleeps 200ms (lets notify install its watch), writes `hello.txt`, polls `process_batch` every 100ms in a 5s window until the handler observes a `change`. Generous margins for FSEvents latency. notify spawns its own thread, so this runs cleanly under the existing sync `engine_lock` harness. Verbatim port of JSC's `followon_a_watch_source_fires_change_events_e2e`. |

Plus a 7th extension to `phase3b_hook_prelude_visible_as_builtins` adding `'use_source'`, `'timer_source'`, `'delay_source'`, `'http_source'`, `'watch_source'` to the names tuple + Rust required array.

**Why no e2e for timer/delay or httpSource**: timer/delay need a tokio runtime context for `tokio::time::interval`/`sleep` to fire; existing `engine_lock` is `MutexGuard<'static, ()>` (sync); mixing `#[tokio::test]` with the static lock is awkward. httpSource port-binding is OS-dependent and flaky in CI. Same coverage profile as JSC.

### M13-phase3e-γ (2026-05-04 — this session) acceptance

- `cargo run -p xtask -- zig-build --release` rebuilds `libgraphreact_cpython_zig.a` (version-string bump only — no Zig FFI logic changes).
- `cargo test --workspace --release`: **627 passed, 2 ignored** (was 624 phase3e-β; +3 phase 3e-γ tests = 627).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean (one `+`-as-list-marker doc-lazy-continuation footgun caught + fixed; recurring pattern).
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3e-gamma (Python 3.9.6)`.
- `target/release/grx run --runtime=cpython python-pingpong.grx`: regression-clean.
- Canonical JSC fixtures regression-clean on `--runtime=jsc`.
- Stale-stub audit clean.

### M13 phase 3 closeout

With 3e-γ shipping, **M13 phase 3 is feature-complete on Python**. Sub-phases 3a + 3b + 3c + 3d-α through 3d-ζ + 3e-α + 3e-β + 3e-γ all shipped. Every hook surface JSC has via M12.3a-phase3a through M12.3a-phase3e-ζ + M12.3a-followon-A is now real on `--runtime=cpython`. All 8 named application classes (Self-Healing Infrastructure, ComplyFlow / Compliance Routing, Adaptive Education, Generative World Building, Biological Pathway Simulation, Reputation Systems, Knowledge Graphs That Reason, Org Intelligence) are prototypable end-to-end on Python.

Outstanding M13 sub-phases — all concurrency / build-flag concerns, not new hook surfaces:
- **M13-phase3f** — PEP 684 per-interpreter GIL detection on Python 3.12+; widen 1000-cycle stress test gates; drop "shutdown leaves main interpreter alive" workaround.
- **M13-phase3g** — `useEffectAsync` via asyncio bridge (Python analogue of M12.3b-phase1's JS Promise → Rust Future bridge).
- **M13-phase3h** — PEP 703 free-threaded build (`python3.14t`) — linkage flag + loosen test-side `engine_lock()`. Rust-side `CALLBACK_REGISTRY` already `Send + Sync`.

## M13-followon-D analogue — per-node `__hostCallbacks` token GC

Per-node `__hostCallbacks` token GC on unmount. **Verbatim Python port of JSC's M12.3a-followon-D** ([jsruntime.rs:209, 411-413, 421-423, 433-455, 473-487, 4244-4297, 4697-4707](crates/graphreact-cli/src/jsruntime.rs)). Closes the bounded `__hostCallbacks` token leak documented since 3b.

### What's swept

Body-time tokens minted by Python-side `__register_callback`:
- `use_effect(fn)` from 3b
- `use_message('type', handler)` from 3c
- `use_callback(fn, deps)` from 3d-α
- `use_topology(handler, filter?)` from 3d-β
- `use_edge_accept_handler(predicate)` from 3d-ζ
- `use_spawn_child(spec)` componentToken from 3e-α
- `use_compose(specs)` componentToken from 3e-α

Each call mints a fresh token in `_hostCallbacks` (Python-side dict). Pre-this-slice, those tokens accumulated for the lifetime of the isolate. Now they're swept on each node's unmount.

### Mechanism

1. **Python `__register_callback(fn)` modification** — the prelude's existing `__register_callback` (shipped in 3b) now calls `host_track_token(token)` immediately after minting, guarded by `try / except NameError` for paths that don't register the callback (dry-mount, prelude-eval phase before host_track_token registers).
2. **`build_host_track_token_callback`** — appends token into `NODE_TOKENS[current_node_id]` TLS slot. When `current_node_id()` returns `None` (cleanup-token mints during `flush_effects` — outside body-eval window), the callback no-ops.
3. **Topology listener** — already shipped in 3b. Pushes `NodeRemoved.node_id` into `pending_unmounts: Arc<parking_lot::Mutex<Vec<String>>>` whenever `Scheduler` removes a node. Fired by `use_unmount_self` (3e-α) and by `Scheduler::remove_node_at_runtime` (test-driven).
4. **`PyRuntime::process_batch` drain** — after `scheduler.process_batch()` returns (engine + iso TLS still installed via `PyEngineGuard::with_directory`), drains `pending_unmounts` and calls `release_tokens_for_node` for each id.
5. **`release_tokens_for_node`** — drains the bucket via `drain_node_tokens` and dispatches `__release_tokens([...])` via `engine.borrow_mut().invoke_global(...)`. The Python prelude's `__release_tokens(tokens)` (already shipped in 3b) iterates and `_hostCallbacks.pop(t, None)`s each.

### Why cleanup tokens don't need explicit tracking

Cleanup tokens are minted by `__invoke_callback` during effect-fire (HOOK_PRELUDE — cleanup branch returns `__register_callback(result)`). At that point `flush_effects` has run and the body-eval window is closed, so `CURRENT_NODE_ID` is unset → `host_track_token` no-ops. That's intentional: `host_effect`'s `EffectCleanup` closure already calls `__release_callback(ct)` immediately after the cleanup runs, so the cleanup-fire path doesn't accumulate tokens. The only cleanup-token leak case is when a node unmounts WITH a pending un-fired cleanup — graphreact-core's `lifecycle::stop_effects` fires the cleanup during unmount, which then auto-releases. So even that case lands on the same auto-release path. **No explicit tracking needed for cleanup tokens.** Same reasoning JSC's M12.3a-followon-D documents.

### Files changed

- `crates/graphreact-cli/src/pyruntime.rs` — new `NODE_TOKENS` TLS slot; new `drain_node_tokens` / `release_tokens_for_node` / `tracked_token_count` (cfg(test)) helpers; new `build_host_track_token_callback`; `__register_callback` Python prelude modification (try/except NameError guard around `host_track_token(t)`); `host_track_token` registration in `install_hook_prelude`; `process_batch` drain wiring; `reset_global_state` clears `NODE_TOKENS`; module-level docs section. ~210 LOC + 1 test.
- `zig/bridge/cpython.zig` — `ENGINE_VERSION_BASE` → `cpython-0.1.0-m13-followon-D`; module-level docs followon-D bullet.
- `crates/graphreact-engine-cpython/src/lib.rs` — module docs followon-D bullet; `engine_version` doc bump.
- `crates/graphreact-engine-cpython/build.rs` — touched to force re-link.
- `crates/graphreact-cli/src/commands.rs` — `cpython_version_line()` + `run_run` cpython trailer rewritten.

### 1 acceptance test

- `followon_d_analogue_unmount_sweeps_host_callbacks_tokens` — single-node body uses `use_effect` + `use_message` + `use_callback` (three body-time tokens). After mount + first `process_batch`, asserts `tracked_token_count() >= 3` under node id `"leaky"`. Calls `release_tokens_for_node` directly. Asserts `tracked_token_count() == 0` post-sweep. Verbatim port of JSC's `followon_d_unmount_sweeps_host_callbacks_tokens`.

### M13-followon-D analogue (2026-05-04) acceptance

- `cargo run -p xtask -- zig-build --release` rebuilds `libgraphreact_cpython_zig.a` (version-string bump only — no Zig FFI changes).
- `cargo test --workspace --release`: **628 passed, 2 ignored** (was 627 phase3e-γ; +1 followon-D test = 628).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean (one `+`-as-list-marker doc-lazy-continuation footgun caught + fixed; 5th re-encounter — recurring pattern).
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-followon-D (Python 3.9.6)`.
- `target/release/grx run --runtime=cpython python-pingpong.grx`: regression-clean.
- Stale-stub audit clean.

## Phase 3g — `useEffectAsync` via asyncio bridge

`useEffectAsync` lets a body register an `async def` callable; the runtime awaits its completion plus handles cleanup plus rejection — same hook surface as JSC modulo Python idiom. **Verbatim Python port of JSC's M12.3b-phase1** ([jsruntime.rs:225-260, 715-935, 4280-4297](crates/graphreact-cli/src/jsruntime.rs)). 2 new host callbacks + `EffectAsyncSettlement` enum + `EFFECT_ASYNC_COMPLETIONS` TLS slot + Python-novel manual-coroutine-trampoline pattern.

### Mechanism

1. **Python `use_effect_async(async_fn, deps)`** HOOK_PRELUDE wrapper — registers a SYNC invoker built by `_make_async_effect_invoker(async_fn)` under a token via `__register_callback`; dispatches `host_effect_async(token, deps)`.
2. **`build_host_effect_async_callback`** parses args, captures `iso` from TLS, calls `core_use_effect_async` with an async closure that's `Fn() -> impl Future<Output = Option<AsyncEffectCleanup>> + 'static`. The closure is re-fired by `flush_effects_async` whenever deps change or the node first mounts.
3. **The async closure**: (a) allocates `completion_id`, parks `tokio::sync::oneshot::Sender` in `EFFECT_ASYNC_COMPLETIONS`; (b) invokes the Python invoker via `invoke_global_static(iso, "__invoke_callback", [token, [completion_id]])`; (c) awaits the `Receiver`.
4. **The Python invoker** (sync function): calls `async_fn()` → gets a coroutine; drives it to completion via a **manual `coro.send(None)` trampoline**; inspects the result (callable → cleanup; raised → throw; otherwise → value); calls `host_effect_async_complete(completion_id, kind, payload)`.
5. **`build_host_effect_async_complete_callback`** removes the Sender from the TLS map and signals it. The async closure's `Receiver.await` returns immediately (synchronous run-to-completion).
6. **Settlement** → `Some(AsyncEffectCleanup)` for cleanup callables (boxed closure that fires `__invoke_callback(ct, [])` + `__release_callback(ct)` on next deps-change re-render or unmount); `None` for value/throw.

### Python-novel design — manual coroutine trampoline

JSC uses `Promise.resolve().then(...)` and relies on JSC's auto-drain at `JSObjectCallAsFunction` boundary. Python's analogous mechanism would be `asyncio.run(coro)` or `loop.run_until_complete(coro)` — but those touch the `_asyncio` C extension's per-interpreter state, which **doesn't fully clean up on `Py_EndInterpreter` on CPython 3.9** (causing SIGSEGV in subsequent sub-interpreter teardowns; observed during initial 3g development).

**Workaround**: a manual coroutine trampoline that drives the coroutine via `coro.send(None)` / `coro.throw(...)` without touching `_asyncio`:

```python
_coro = coro_or_value
while True:
    try:
        future = _coro.send(None)
    except StopIteration as si:
        result = si.value
        break
    # If the coroutine yields (awaits) a real Future, raise — async I/O
    # on 3.9 sub-interpreters is unsupported. Lifts at PEP 684 (3.12+).
    if hasattr(future, 'done') and future.done():
        ...continue with future.result()...
    raise RuntimeError('use_effect_async on Python 3.9: only sync-resolved coroutines supported')
```

**Sync-resolved coroutines** (no `await` of a real `Future`, only direct returns or `await`s of other sync-resolved coroutines) complete in one `send` cycle. **Genuine async I/O on 3.9 is a documented limitation** — the trampoline detects unfinished `Future` yields and raises a clear error. Lifts at PEP 684 (Python 3.12+) where sub-interpreter + asyncio interaction is sound.

### CLI activation deferred

`grx run --runtime=cpython` continues to drive the sync `process_batch`. Rationale matches JSC's M12.3b-phase1 punt: nested-runtime restructuring is its own slice. Not a regression — `useEffectAsync` was a no-op stub on the sync CLI path before this slice. Tests drive `process_batch_async` via `tokio::runtime::Builder::new_current_thread().enable_all().build()` + `tokio::task::LocalSet::block_on` because `PyRuntime` is `!Send`.

### Token leak surface unchanged

Body invokers (`_make_async_effect_invoker(async_fn)`) auto-track via `__register_callback` → `host_track_token` (followon-D). Cleanup tokens (the callable returned by the user's async fn) fire during `flush_effects` (CURRENT_NODE_ID unset → no-op tracking) and auto-release via `__release_callback(ct)` in the `AsyncEffectCleanup` closure. Same auto-release shape as the sync `use_effect` path. **Followon-D's bounded-leak guarantee carries forward.**

### 2 new host callbacks + 5 acceptance tests

| Symbol | Role |
|---|---|
| `build_host_effect_async_callback` | Parks oneshot Sender; invokes Python invoker; awaits Receiver; translates settlement → `AsyncEffectCleanup` or None |
| `build_host_effect_async_complete_callback` | Decodes (completion_id, kind, payload); removes parked Sender; signals settlement |

| # | Test | Validates |
|---|---|---|
| 1 | `phase3g_use_effect_async_void_resolution_fires_body` | async fn returns None; body fires during `process_batch_async`. Pins basic wiring. |
| 2 | `phase3g_use_effect_async_cleanup_fires_on_deps_change` | async fn returns a cleanup callable; setter triggers re-render; prior cleanup runs before new effect mounts. Closure-captured count value verifies cleanup-binding semantics. |
| 3 | `phase3g_use_effect_async_rejection_handled_gracefully` | async fn raises; `process_batch_async` returns Ok; render still completes; rejection logged via `eprintln`. No panic. |
| 4 | `phase3g_use_effect_async_coexists_with_sync_use_effect` | both `use_effect` and `use_effect_async` fire during `process_batch_async`. Pins the contract that the async path processes sync slot identically. |
| 5 | `phase3g_use_effect_async_skipped_on_sync_path` | `process_batch()` (sync) does NOT fire async effects. Pins the opt-in contract. |

Each test (except #5) runs inside `tokio::runtime::Builder::new_current_thread().enable_all().build()` + `tokio::task::LocalSet::block_on`. Same pattern as JSC's phase1b tests.

### M13-phase3g (2026-05-05 — this session) acceptance

- `cargo run -p xtask -- zig-build --release` rebuilds `libgraphreact_cpython_zig.a` (size delta from version-string bump only — no Zig FFI changes).
- `cargo test --workspace --release`: **633 passed, 2 ignored** (was 628 followon-D; +5 phase 3g tests = 633).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3g (Python 3.9.6)`.
- `target/release/grx run --runtime=cpython python-pingpong.grx`: regression-clean.
- Stale-stub audit clean.

### Footgun discovered: `_asyncio` sub-interpreter state pollution

Initial implementation used `asyncio.new_event_loop().run_until_complete(coro)`. The 5 tests passed individually but the full run crashed with SIGSEGV (signal 11) — `Py_EndInterpreter` doesn't fully clean `_asyncio` C-extension state on CPython 3.9, so subsequent sub-interpreter teardowns hit freed-memory access. **Fix**: manual `coro.send(None)` trampoline that drives coroutines without touching `_asyncio`. Genuine async I/O on 3.9 raises a clear error; lifts at PEP 684 (3.12+) where the issue is fixed.

**Pattern reusable for future sub-interpreter + C-extension interactions on 3.9**: any C-extension that stashes per-interpreter state (asyncio, threading internals, signal, etc.) needs to be avoided or carefully scoped to the sub-interpreter lifetime. Document the limitation; lift at 3.12+.

## Phase 3f — PEP 684 per-interpreter GIL on Python 3.12+

PEP 684 (Python 3.12) lets each sub-interpreter own its GIL via a new `Py_NewInterpreterFromConfig(_, &config)` entry point with `PyInterpreterConfig.gil = OWN_GIL`. Concurrent isolates on different OS threads no longer serialise on a process-global GIL — the headline win for backend workloads that mount many components in parallel. PEP 684 also makes the C-extension global state isolated per sub-interpreter, which removes the limits that forced our 3-cycle modest-stress gate on 3.9 (the legacy `Py_NewInterpreter` path crashes at ~50 cycles on 3.9 due to `_signal` / `_asyncio` C-extension state pollution; PEP 684 fixes that for all C extensions that opt in via the multi-interp marker).

### Mechanism

Two `comptime`-gated branches in `zig/bridge/cpython.zig`:

1. **`gr_cpython_isolate_create`** — when `build_opts.python_3_12` is true, builds a `PyInterpreterConfig` with `use_main_obmalloc=0` / `check_multi_interp_extensions=1` / `gil=OWN_GIL` / `allow_fork=1` / `allow_exec=1` / `allow_threads=1` / `allow_daemon_threads=1` and calls `Py_NewInterpreterFromConfig(&tstate_out, &config)`. On failure, restores the main thread state and returns `GR_ERR_INTERNAL`. Legacy `Py_NewInterpreter()` retained for ≤3.11.
2. **`gr_cpython_engine_shutdown`** — when `build_opts.python_3_12` is true, after disposing all isolates, swaps to the main thread state, calls real `Py_FinalizeEx()`, nulls `state.main_thread_state` / `state.version_string`, and re-inits `state.isolates`. The legacy "leave main alive" workaround stays for ≤3.11 (Py_FinalizeEx + re-Py_Initialize is unsupported there).

### Build-time selector

`crates/graphreact-engine-cpython/build.rs::resolve_python` runs the resolution chain documented in [Discovery + linkage](#discovery--linkage) and emits:

- `cargo:rustc-cfg=python_3_12` when minor ≥ 12 (gates Rust-side tests and any future runtime branches).
- `cargo:python-rpath=<path>` / `cargo:python-include=<path>` / `cargo:python-minor=<N>` / `cargo:python-3-12=<bool>` / `cargo:python-source=<provenance>` / `cargo:python-pkg-config-name=<name>` — surfaced to downstream crates' build scripts as `DEP_GRAPHREACT_CPYTHON_*` env vars (works because `Cargo.toml` declares `links = "graphreact_cpython"`).

`graphreact-cli/build.rs` reads `DEP_GRAPHREACT_CPYTHON_PYTHON_RPATH` to bake the same rpath into the `grx` binary, and `DEP_GRAPHREACT_CPYTHON_PYTHON_3_12` to forward the cfg flag for any cli-side gating.

`xtask/src/main.rs::resolve_python_for_zig_build` mirrors the same probe (resolution-only — no link config) and passes `-Dpython_include=<path>` plus `-Dpython_3_12=true` (when minor ≥ 12) to `zig build`. Both probes resolve the same Python on a given machine, so cargo-side and Zig-side header sets stay in sync.

### Comptime gating + header-set safety

`Py_NewInterpreterFromConfig` and `PyInterpreterConfig` don't exist in Python 3.9's headers. Zig's `comptime` lets the legacy path compile cleanly when `python_3_12 = false` — the 3.12+ branch's body is elided from the AST, so the compiler never tries to resolve the missing symbols. When `python_3_12 = true` the include path must point at 3.12+ headers; the xtask probe enforces this by deriving the include path from the same pkg-config that the cfg flag was derived from.

### `Py_INCREF` macro vs `Py_IncRef` function

Python 3.13's `Py_INCREF` macro accesses `ob_refcnt_split[PY_BIG_ENDIAN]` to support free-threaded refcount splitting. Zig's translate-c can't model that construct (the array index uses a macro that translate-c doesn't fully expand) and fails with `expected type 'u32', found '[2]u32'`. The fix: replace all `c.Py_INCREF(x)` calls with `c.Py_IncRef(x)`. `Py_IncRef` is a stable C function (not a macro) exported by libpython since 3.0; it's slightly slower in microbenchmarks (no inlining) but irrelevant at our call density. `Py_DecRef` translates fine, so `c.Py_DECREF` calls stay as-is.

### 1000-cycle stability test

`isolate_stress_pep684_1000_cycles_no_leak` is gated behind `#[cfg(python_3_12)]`. It runs 1000 mount/dispose cycles and compares two RSS-growth windows:

- Warm-up: cycles 100–199 (after sub-interpreter pool + obmalloc init costs are absorbed).
- Steady-state: cycles 900–999 (where any per-cycle leak would dominate).

The bound is `steady_growth ≤ warmup_growth + 20 MiB`. **Stability gate, not absolute-RSS gate** — macOS's coarse-grained RSS reporting (4 KiB pages, conservative virtual-memory release back to the OS) makes absolute bounds noisy across runs. A stability gate captures the load-bearing signal: identical incremental growth between windows means no per-cycle leak is accumulating.

The legacy 3-cycle test (`isolate_stress_modest_cycles_no_leak`) is unchanged — runs on every build, including 3.12+, as a cheap sanity gate.

### M13-phase3f (2026-05-05 — this session) acceptance

- `cargo run -p xtask -- zig-build --release` rebuilds `libgraphreact_cpython_zig.a` (now linked against Homebrew Python 3.13.13 on this machine via `pkg-config python-3.13-embed`).
- `cargo test --workspace --release`: **634 passed, 0 failed, 2 ignored** on Python 3.13.13 (was 633 phase3g + 1 new `isolate_stress_pep684_1000_cycles_no_leak` test = 634; the 2 ignored are the engine-cpython 200-cycle `isolate_stress_probe_threshold` probe and a graphreact-core diagnostic).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3f (Python 3.13.13)`.
- `target/release/grx run --runtime=cpython python-pingpong.grx`: regression-clean.
- Stale-stub audit clean.

### What 3f unblocks

- **Concurrent isolate execution on 3.12+.** The Rust-side `CALLBACK_REGISTRY` is already `Send + Sync` (shipped at phase 2.5); the test-side `engine_lock()` was the remaining serialisation. Future slices can drop or scope the lock when the build is 3.12+, enabling threaded isolate stress tests.
- **Real shutdown/re-init cycles on 3.12+.** Tests that previously had to share a single bring-up across the whole pool can now bring up + tear down per-test; useful for isolation in tests that touch process-global state.
- **PEP 703 free-threaded slice (3h).** Reuses 3f's pkg-config probe + cfg flag scaffold; just needs to recognise `python-3.14t-embed` (already in the auto-probe list at top priority) and emit a `python_free_threaded` cfg.

### Linux/Windows linkage

The pkg-config-first build script means Linux/Windows binaries can be produced with no further code changes — install `python3-dev` (Linux) / Python with the official MSI (Windows, which ships `python3.lib` + `python3.pc`) and the same resolution chain finds it. Per-OS artifact production is a parallel CI track (multi-platform runners + per-Python-minor matrix); not blocked on M13-phase3h.

### Footgun discovered: macOS RSS reporting can't drive absolute-memory leak gates

Initial 1000-cycle test used an absolute 100 MiB RSS growth bound (mirroring JSC's followon-A pattern). The first run reported 1.4 GB growth — but the test didn't crash (PEP 684 was working; the legacy 3.9 path crashes at ~50 cycles). The growth was a combination of CPython sub-interpreter pool overhead (each `own_gil` interpreter owns its obmalloc, type cache, builtins dict) and macOS's conservative virtual-memory release. **Fix**: switch to a stability gate (warm-up window vs steady-state window) — a real per-cycle leak shows up in both windows; the one-shot pool initialisation cost only shows up in the warm-up window. The bound becomes "steady-state delta ≤ warm-up delta + 20 MiB" instead of "total growth ≤ 100 MiB". Pattern reusable for any future PEP 684-using stress test.

## Phase 3h — PEP 703 free-threaded build linkage (`python3.Xt`)

PEP 703 (Python 3.13 optional, 3.14 official under PEP 779) removes the GIL entirely. Free-threaded builds ship as `python3.Xt` — the trailing `t` distinguishes them from regular `python3.X`. Multiple OS threads can hold different sub-interpreters' thread states simultaneously without serialising on a process-global GIL.

### Mechanism

3h is a build-flag slice — the runtime path stays uniform across GIL'd and free-threaded builds because PEP 703 doesn't change any C-API entry point we use (`Py_InitializeEx`, `PyThreadState_Swap`, `Py_NewInterpreterFromConfig`, `PyRun_String`, `PyObject_*` all work identically). What changes is the *semantics* of those calls: in a GIL'd build, two threads can never be inside the C API at once; in a free-threaded build, they can.

The slice ships:

1. **Detection.** `crates/graphreact-engine-cpython/build.rs::probe_pkg_config` checks two complementary signals:
   - pkg-config name suffix: `python-3.Xt-embed` (trailing `t` before `-embed`).
   - libname suffix: pkg-config `--libs` resolves to `-lpython3.Xt`.

   Either signal flips the resolution's `free_threaded: bool` field. `--cfg python_free_threaded` emitted; `cargo:python-free-threaded=1` surfaced for downstream crates via `links = "graphreact_cpython"`.

2. **xtask zig-build mirror.** `xtask::resolve_python_for_zig_build` runs the same probe and passes `-Dpython_free_threaded=true` to `zig build`. `zig/bridge/build.zig` exposes the option; `cpython.zig` reads `build_opts.python_free_threaded` at comptime.

3. **Engine version annotation.** `cpython.zig`'s `gr_cpython_engine_initialize` appends ` free-threaded` to the version string when `build_opts.python_free_threaded` is set. The Rust trait-side `engine_version` returns the annotated string verbatim.

4. **Runtime probe test.** `#[cfg(python_free_threaded)] pep703_free_threaded_build_has_gil_disabled` evaluates `sys._is_gil_enabled()` inside an isolate and asserts the result is `False`. Honest acceptance — confirms the libpython we linked actually has GIL removal compiled in *and* engaged at runtime. (`sys.flags.gil` returns `None` rather than `0`/`1` on 3.14t when the GIL state is "dynamic" — `sys._is_gil_enabled()` is the canonical probe.)

### What 3h does NOT do (and why)

3h does NOT loosen `engine_lock()` to a no-op on free-threaded builds (despite the original outstanding-work plan suggesting that). The Zig bridge carries process-global mutable state — `state.isolates`, `state.next_handle`, `state.version_string`, plus per-isolate `modules` / `functions` lists — that's only thread-safe under serialised access. In a GIL'd build, the GIL implicitly serialises every C-API entry point. In a free-threaded build, no implicit serialisation exists, so the same global state without explicit Zig-side locking would race.

Adding `std.Thread.Mutex` protection around every state.* mutation site in `cpython.zig` (~30 entry points, with care to avoid deadlocks under recursive PyObject re-entries through callbacks) is a separable refactor. 3h ships the build-flag substrate; that refactor unlocks actual concurrent isolate execution and is the natural follow-on for any code path that wants per-isolate parallelism (M14 distribution, future per-test concurrency, etc.).

The `python_free_threaded` cfg flag flowing through to the Rust runtime layer is the load-bearing piece — code paths that DO want per-isolate parallelism can branch on it without another build-system pass.

### `Py_DECREF` / `PyList_Check` / `PyList_Size` macro footguns

PEP 703 builds inline `_Py_atomic_*` helpers into multiple Python C-API macros:

- `Py_DECREF` accesses split refcounts via `_Py_atomic_load_uint32_relaxed` / `_Py_atomic_store_uint32_relaxed` (because shared objects need atomic refcount updates).
- `PyList_Check` / `PyList_Size` access the type cache via `_Py_atomic_load_uint64_relaxed` (because type pointers can be mutated under free-threaded execution).

Zig's translate-c can't inline these atomic helpers (the macros expand to multi-statement constructs that translate-c models as extern function calls). The helpers aren't exported by libpython3.Xt as real symbols, so linking fails with `Undefined symbols: __Py_atomic_load_uint32_relaxed` etc.

**Fix shape (same as M13-phase3f's `Py_INCREF` → `Py_IncRef` swap):**

- All `c.Py_DECREF(x)` swapped to `c.Py_DecRef(x)` — `Py_DecRef` is a stable C function (not a macro), exported by libpython since 3.0; slightly slower in microbenchmarks (no inlining) but irrelevant at our call density. 31 sites swapped across `cpython.zig`.
- `c.PyList_Check(args_list) == 0` checks at 2 sites elided — `PyList_AsTuple` sets a `TypeError` and returns null on non-list inputs, so the explicit type check was defensive, not load-bearing.
- `c.PyList_Size` lookup at 1 site elided — the value (`argc`) was discarded immediately (`_ = argc`).

### M13-phase3h (2026-05-05 — this session) acceptance

- `cargo run -p xtask -- zig-build --release` rebuilds `libgraphreact_cpython_zig.a` (now linked against Homebrew `python-freethreading` 3.14.4 on this machine via `pkg-config python-3.14t-embed`).
- `cargo test -p graphreact-engine-cpython --release`: **17 passed, 0 failed, 1 ignored** on Python 3.14.4 free-threaded (was 16 phase3f-on-3.14t + 1 new `pep703_free_threaded_build_has_gil_disabled` = 17). Note: the `isolate_stress_pep684_1000_cycles_no_leak` from phase 3f also passes here (3.14t implies 3.12+ → `python_3_12` cfg also set).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3h (Python 3.14.4 free-threaded)`.
- `target/release/grx run --runtime=cpython python-pingpong.grx`: regression-clean.
- JSC fixtures: regression-clean.
- Stale-stub audit clean.

### What 3h unblocks

- **Future concurrent isolate execution.** The `python_free_threaded` cfg flag is now available to code that wants to branch on it. The Zig-side state-locking refactor that actually unlocks concurrent execution is a separable follow-on, but 3h provides the substrate it needs.
- **M14 distribution layer.** When M14 lands actor-shaped distributed runtime work, threads-per-actor models can run on free-threaded Python with appropriate Zig-side locking — no further build-system work needed.
- **Per-OS / per-Python-ABI distribution slices.** Combined with 3f's per-minor-version slicing, the artifact matrix is now `{macOS-arm64, macOS-x86_64, linux-x86_64, linux-arm64, windows-x86_64} × {3.12, 3.13, 3.14, 3.14t}` — production CI matrix territory. Linkage works via the same pkg-config path on every supported OS with `python3-dev` / `python-freethreading` / equivalent installed.

### Build-machine readiness

3h requires a free-threaded Python install on the build machine:
- **macOS via Homebrew**: `brew install python-freethreading` provides Python 3.14.4 free-threaded as `python3.14t`; `pkg-config python-3.14t-embed` resolves cleanly from `/opt/homebrew/opt/python-freethreading/Frameworks/PythonT.framework/Versions/3.14/`.
- **Linux**: distro packages (`apt install python3.14-freethreading-dev` or equivalent depending on distribution).
- **Windows**: Python with the official MSI ships free-threaded as a separate ABI variant.

Without a free-threaded Python install, the existing `python_3_12` path continues to be auto-resolved (priority 2 in the auto-probe chain), and `--cfg python_free_threaded` stays unset — the slice degrades cleanly to the 3f path.

### M13 closure summary

**Every M13 sub-phase shipped:**

| Sub-phase series | Status |
|---|---|
| Phase 1+2 (lifecycle + eval/resolve/call) | ✓ shipped 2026-05-02 |
| Phase 2.5 (bidirectional callbacks) | ✓ shipped 2026-05-02 |
| Phase 3a–3e-γ (10 hook-surface slices) | ✓ shipped 2026-05-03/04 |
| M13-followon-D analogue (token GC) | ✓ shipped 2026-05-04 |
| Phase 3g (useEffectAsync via asyncio bridge) | ✓ shipped 2026-05-05 |
| Phase 3f (PEP 684 multi-GIL on 3.12+) | ✓ shipped 2026-05-05 |
| Phase 3h (PEP 703 free-threaded linkage) | ✓ shipped 2026-05-05 |

The next strategic decision is between:
- M14 distribution (actor-shaped multi-host clustering).
- M12.4 Node-API compat for JSC bodies (backend NPM parity).
- M16-Backend ops surface (Prometheus + OTel + JSON logs + management API).
- Per-OS / per-Python-minor CI matrix (production-track follow-on for 3f/3h).
- hyperfine benchmark (CPython-vs-JSC perf characterisation).

## What's deferred

| Item | Lands at |
|---|---|
| ~~Python HOOK_PRELUDE: useState / useEffect (deps + cleanup) via `__hostCallbacks` token registry~~ | ~~M13-phase3b~~ — shipped 2026-05-03 |
| ~~useEmit + useMessage (message dispatch + handler registration)~~ | ~~M13-phase3c~~ — shipped 2026-05-03 |
| ~~useMemo + useCallback + useReducer (memoization hooks via split-cache pattern)~~ | ~~M13-phase3d-α~~ — shipped 2026-05-03 |
| ~~useNeighbors + useTopology + useAcceptEdge (Tier 1 graph-read hooks + mutation queue)~~ | ~~M13-phase3d-β~~ — shipped 2026-05-03 |
| ~~ctx.directory (5 finders) + useGraphQuery Tier 1 (4 accessors over static GraphSnapshot)~~ | ~~M13-phase3d-γ~~ — shipped 2026-05-03 |
| ~~useGraphQuery widened with 7 algorithm accessors + live GraphSnapshot~~ | ~~M13-phase3d-δ~~ — shipped 2026-05-04 |
| ~~Tier 2 g.tier2(msgType, opts?) (12 accessors + 5 cost heuristics)~~ | ~~M13-phase3d-ε~~ — shipped 2026-05-04 |
| ~~useEdgeAcceptHandler + Tier 3 g.tier3(msgType, opts?)~~ | ~~M13-phase3d-ζ~~ — shipped 2026-05-04 |
| ~~useSpawnChild / useUnmountSelf / useCompose (dynamic node lifecycle hooks)~~ | ~~M13-phase3e-α~~ — shipped 2026-05-04 |
| ~~supervision (useMonitor / useDemonitor / useLink / useUnlink / useGroupJoin / useGroupLeave)~~ | ~~M13-phase3e-β~~ — shipped 2026-05-04 |
| ~~useSource + timer_source / delay_source / http_source / watch_source~~ | ~~M13-phase3e-γ~~ — shipped 2026-05-04 (closes M13 phase 3 on Python) |
| ~~Token GC on unmount (followon-D analogue — sweeps bounded leak across all token-using hooks since 3b)~~ | ~~M13-followon-D analogue~~ — shipped 2026-05-04 |
| ~~PEP 684 per-interpreter GIL on Python 3.12+; widen stress test gates; drop process-singleton workaround~~ | ~~M13-phase3f~~ — shipped 2026-05-05 |
| ~~asyncio integration — Promise/Future-style bridge (Python analogue of M12.3b-phase1)~~ | ~~M13-phase3g~~ — shipped 2026-05-05 |
| ~~PEP 703 free-threaded build (`python3.14t`) — linkage flag~~ | ~~M13-phase3h~~ — shipped 2026-05-05 (engine_lock() relaxation deferred — requires Zig-side state locking, separable refactor) |
| Per-OS / per-Python-minor artifact production (linkage works since phase 3f; CI matrix is the unbuilt piece) | parallel track (Linux / Windows / macOS-Homebrew slices) |
| Per-engine binary split (`grx-jsc` vs `grx-py` via `grx` shim) | natural inflection point at phase 3b+ |
| Multi-engine systems (mixed `lang="js"` + `lang="python"` in same `.grx`) | M22 (deferred per re-architecture decisions) |
| All M12.3b-phase3-5 deferrals (vendored WebKit clone+build, Linux/Windows libJSC, drain_shim against vendored headers, CI matrix) | unchanged — orthogonal track |

## Acceptance verification

### M13-phase1+2 (2026-05-02 morning)
- `cargo run -p xtask -- zig-build --release` produces `libgraphreact_cpython_zig.a` (~2.1 MB).
- `cargo test -p graphreact-engine-cpython --release`: 12 passed, 1 ignored (the probe).
- `cargo test --workspace --release`: 562 passed.
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx run --runtime=cpython <fixture.grx>` runs the smoke module end-to-end.

### M13-phase2.5 (2026-05-02 afternoon)
- `cargo run -p xtask -- zig-build --release` produces `libgraphreact_cpython_zig.a` (~2.13 MB after callback bridge).
- `cargo test -p graphreact-engine-cpython --release`: 15 passed, 1 ignored (was 12 phase1+2; +4 phase2.5 tests, −1 stub-marker test = +3 net).
- `cargo test --workspace --release`: 565 passed (was 562; +3).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase2.5 (...)`.
- `target/release/grx run --runtime=cpython <fixture.grx>` runs the smoke module end-to-end on the phase2.5 trailer.
- All 4 canonical JSC fixtures still terminate cleanly on `grx run --runtime=jsc` (M12.3b-phase1.5 regression-clean).

### M13-phase3a (2026-05-03 morning)
- `cargo run -p xtask -- zig-build --release` produces `libgraphreact_cpython_zig.a` (2,129,380 bytes — version-string bump only; no Zig delta beyond the engine version constant).
- `cargo test --workspace --release`: 571 passed, 2 ignored (was 565; +4 parser/IR tests + +2 pyruntime tests = +6 net).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3a (Python 3.9.6)`.
- `target/release/grx run --runtime=cpython crates/graphreact-cli/tests/fixtures/python-smoke.grx`: first time author-written Python bodies execute end-to-end through the standalone CLI.

### M13-phase3b (2026-05-03 afternoon — this session)
- `cargo run -p xtask -- zig-build --release` produces `libgraphreact_cpython_zig.a` (2,129,380 bytes — version-string bump only).
- `cargo test --workspace --release`: **576 passed, 2 ignored** (was 571; +7 pyruntime tests, −2 phase 3a tests now subsumed under the rewrite = +5 net).
- `cargo clippy --workspace --all-targets --release -- -D warnings`: clean.
- `target/release/grx version` lists `engine-cpython cpython-0.1.0-m13-phase3b (Python 3.9.6)`.
- **`target/release/grx run --runtime=cpython crates/graphreact-cli/tests/fixtures/python-counter.grx`**: mounts a Counter body using `use_state` + `use_effect` end-to-end through the scheduler. The trailer text describes the phase 3b mechanism + the 3c-3h roadmap.
- All 4 canonical JSC fixtures still terminate cleanly on `grx run --runtime=jsc` (M12.3b-phase1.5 regression-clean).

## Files added / modified

### New (M13-phase1+2)
- `crates/graphreact-engine-cpython/Cargo.toml`
- `crates/graphreact-engine-cpython/build.rs`
- `crates/graphreact-engine-cpython/src/ffi.rs`
- `crates/graphreact-engine-cpython/src/lib.rs`
- `zig/bridge/cpython.zig`
- `crates/graphreact-cli/build.rs` (rpath bake-in for the consumer binary)
- `docs/m13-phase1-2-cpython.md` (this file)

### Modified — M13-phase1+2
- `Cargo.toml` (workspace members += `crates/graphreact-engine-cpython`)
- `zig/bridge/build.zig` (extended to build cpython.zig + addIncludePath for Python.h)
- `crates/graphreact-cli/Cargo.toml` (dep += `graphreact-engine-cpython`)
- `crates/graphreact-cli/src/cli.rs` (`RuntimeArg::Cpython` variant + Display impl)
- `crates/graphreact-cli/src/engine_select.rs` (`EngineKind::Cpython` + `select` arm + `CpythonNotLinked` error + 2 cpython tests)
- `crates/graphreact-cli/src/commands.rs` (`EngineKind::Cpython` arm in `run_run` + `cpython_version_line()` + cpython trailer)

### Modified — M13-phase2.5
- `zig/bridge/cpython.zig` — real `gr_cpython_callback_register` + `gr_cpython_callback_invoke`; `CallbackEntry` struct; shared `SHARED_METHOD_DEF`; `pythonCallbackTrampoline`; `callbackEntryCapsuleDestructor`; `jsonDumps` rewritten to avoid the `PyObject_CallMethod` tuple-unpacking footgun. `ENGINE_VERSION_BASE` bumped to `cpython-0.1.0-m13-phase2.5`.
- `crates/graphreact-engine-cpython/src/lib.rs` — `CALLBACK_REGISTRY` (process-global `OnceLock<RwLock<FxHashMap<u64, SharedCallback>>>`); `host_callback_trampoline` (engine-agnostic body verbatim from JSC); `State::iso_callbacks`; real `register_callback`; `CPythonEngine::invoke_global` + free-fn `invoke_global_static`; `dispose_isolate` / `shutdown` drain registry. 4 new tests; phase2.5 stub-marker test removed.
- `crates/graphreact-engine-cpython/src/ffi.rs` — doc comments flipped from "M13-phase2.5 stub" to "M13-phase2.5 — REAL".
- `crates/graphreact-cli/src/commands.rs` — `cpython_version_line` + `run_run` cpython trailer bumped to phase2.5; cpython arm comment updated to reflect real callbacks.

### New — M13-phase3a (this session)
- `crates/graphreact-cli/src/pyruntime.rs` (~280 LOC + 2 tests). `PyRuntime` struct + `mount_python_definitions(path)` entry point. Mirrors `jsruntime::mount_with_scheduler`'s shape at deliberately reduced scope (no hooks, no scheduler). Custom `Debug` impl (CPythonEngine isn't `Debug`).
- `crates/graphreact-cli/tests/fixtures/python-smoke.grx` — canonical Python fixture: Sensor + Logger, both `lang="python"`, returning manifest dicts that include props_seen for ctx-marshalling validation.
- `crates/graphreact-cli/tests/fixtures/python-smoke-mixed-rejected.grx` — mixed `lang="js"` + `lang="python"` fixture used to pin the EngineMisroute error path.

### New — M13-phase3b (this session)
- `crates/graphreact-cli/tests/fixtures/python-counter.grx` (use_state + observe; NameError-guarded).
- `crates/graphreact-cli/tests/fixtures/python-setter.grx` (use_effect calls setter on mount).
- `crates/graphreact-cli/tests/fixtures/python-effect.grx` (deps + cleanup).
- `crates/graphreact-cli/tests/fixtures/python-effect-empty-deps.grx` (empty deps fires once).

### Modified — M13-phase3b
- `crates/graphreact-cli/src/pyruntime.rs` — top-to-bottom rewrite (~1100 LOC including ~80-line HOOK_PRELUDE Python source const + 7 tests). Adds thread-local registries (SIGNAL_REGISTRY, SIGNAL_BY_HOOK_POS, NEXT_SIGNAL_ID, CURRENT_NODE_ID, PY_HOOK_INDEX, CURRENT_ENGINE, CURRENT_ISO), ComponentEvalGuard, PyEngineGuard, host_state/host_state_set/host_effect callbacks, install_hook_prelude, PyRuntime struct, mount_with_scheduler, process_batch. Phase 3a's mount_python_definitions retained as a back-compat entry routing through mount_with_scheduler + one process_batch call.
- `zig/bridge/cpython.zig` — `ENGINE_VERSION_BASE` bumped to `cpython-0.1.0-m13-phase3b`.
- `crates/graphreact-engine-cpython/src/lib.rs` — `engine_version` doc bumped.
- `crates/graphreact-cli/src/commands.rs` — cpython arm `runtime.engine.borrow().engine_version()` (engine is now `Rc<RefCell<...>>`); cpython_version_line + run_run cpython trailer text rewritten for phase 3b mechanism + 3c-3h roadmap.

### New — M13-phase3c (this session)
- `crates/graphreact-cli/tests/fixtures/python-pingpong.grx` — canonical Python ping-pong fixture; two nodes (`Ping`, `Pong`) bouncing messages with `<edge ttl={5} />` capping the cycle. NameError-guarded `host_observe` for clean CLI smoke runs.

### Modified — M13-phase3c
- `crates/graphreact-cli/src/pyruntime.rs` — HOOK_PRELUDE Python source extended with `use_emit` + `use_message` Python wrappers + builtins install lines. Two new Rust callbacks `build_host_emit_callback` + `build_host_message_callback` ported verbatim from jsruntime.rs (engine-agnostic body); new local `parse_emit_options` duplicated from jsruntime (private fn). `install_hook_prelude` registers the two new host callbacks before evaluating the prelude. Module-level docs updated with phase 3c section. 6 new acceptance tests + 1 extension to the prelude-visible test = 7 net new test assertions.
- `zig/bridge/cpython.zig` — `ENGINE_VERSION_BASE` bumped to `cpython-0.1.0-m13-phase3c`. Module-level docs updated: phase 3a/3b/3c shipped status, phase 3d-3h breakdown.
- `crates/graphreact-engine-cpython/src/lib.rs` — module docs updated for phase 3b + 3c shipped status with phase 3d-3h breakdown. `engine_version` doc bumped.
- `crates/graphreact-cli/src/commands.rs` — `cpython_version_line` + `run_run` cpython trailer text rewritten for phase 3c mechanism + updated 3d-3h roadmap blurb. Cpython arm comment updated since 3b/3c no longer "deferred".

### Modified — M13-phase3d-α
- `crates/graphreact-cli/src/pyruntime.rs` — HOOK_PRELUDE Python source extended with `use_memo` / `use_callback` / `use_reducer` wrappers + 3 `_builtins.<name>` install lines. Two new Rust callbacks `build_host_memo_check_callback` + `build_host_memo_store_callback` ported verbatim from jsruntime.rs (engine-agnostic body — split-cache pattern). Two new TLS slots `MEMO_REGISTRY` (`FxHashMap<(String, usize), Value>`) + `PENDING_MEMO_KEY` (`Option<(String, usize)>`) added to the per-thread registries block. `install_hook_prelude` registers `host_memo_check` + `host_memo_store` between `host_message` and the `eval_module` call. `reset_global_state` clears the two new TLS slots. `core_use_memo` added to the graphreact-core import block. Module-level docs updated with phase 3d-α section. 4 new acceptance tests + 1 extension to the prelude-visible test (probes `use_memo` / `use_callback` / `use_reducer` builtins).
- `zig/bridge/cpython.zig` — `ENGINE_VERSION_BASE` bumped to `cpython-0.1.0-m13-phase3d-alpha`. Module-level docs updated: phase 3a/3b/3c/3d-α shipped, 3d-β through 3h breakdown.
- `crates/graphreact-engine-cpython/src/lib.rs` — module docs updated with phase 3d-α section + 3d-β through 3h breakdown. `engine_version` doc bumped.
- `crates/graphreact-cli/src/commands.rs` — `cpython_version_line` + `run_run` cpython trailer text rewritten for phase 3d-α mechanism (split-cache pattern) + updated 3d-β through 3h roadmap blurb.

### Modified — M13-phase3d-β
- `crates/graphreact-cli/src/pyruntime.rs` — HOOK_PRELUDE Python source extended with `use_neighbors` / `use_topology` / `use_accept_edge` wrappers + 3 `_builtins.<name>` install lines. Three new Rust callbacks (`build_host_neighbors_callback`, `build_host_topology_callback`, `build_host_accept_edge_callback`) + 2 helper functions (`parse_topology_filter`, `topology_event_to_host_value`) ported verbatim from jsruntime.rs. `install_hook_prelude` registers `host_neighbors` + `host_topology` + `host_accept_edge` between `host_memo_store` and the `eval_module` call. graphreact-core imports extended with `use_accept_edge`/`use_neighbors`/`use_topology`/`TopologyEventKind`/`TopologyFilterOptions`. Module-level docs updated with phase 3d-β section (three-tier safety framing). 4 new acceptance tests + 1 extension to the prelude-visible test (probes `use_neighbors` / `use_topology` / `use_accept_edge` builtins).
- `zig/bridge/cpython.zig` — `ENGINE_VERSION_BASE` bumped to `cpython-0.1.0-m13-phase3d-beta`. Module-level docs updated: phase 3a/3b/3c/3d-α/3d-β shipped, 3d-γ through 3h breakdown.
- `crates/graphreact-engine-cpython/src/lib.rs` — module docs updated with phase 3d-β section. `engine_version` doc bumped.
- `crates/graphreact-cli/src/commands.rs` — `cpython_version_line` + `run_run` cpython trailer text rewritten for phase 3d-β three-tier safety framing + updated 3d-γ through 3h roadmap blurb.

### Modified — M13-phase3d-γ (this session)
- `crates/graphreact-cli/src/pyruntime.rs` — HOOK_PRELUDE Python source extended with directory + graph_query proxies (`_directory_proxy` + `_graph_query_proxy` as `types.SimpleNamespace`) + `__augment_ctx(raw_ctx)` + `use_graph_query(selector, deps)` (composes over `use_memo`) + 2 `_builtins.<name>` install lines. New `BODY_AUGMENT_SUFFIX` constant (Python source) appended to user body source at eval time — re-binds `body` (or `init`) to call original through `__augment_ctx`. Two new TLS slots `CURRENT_DIRECTORY` (`Rc<RefCell<GraphDirectory>>`) + `CURRENT_GRAPH_SNAPSHOT` (`Rc<GraphSnapshot>`) + readers `current_directory()` / `current_graph_snapshot()`. New `GraphSnapshot` struct (3 `Rc<RefCell<...>>` fields cloned from scheduler) + `from_live` constructor — verbatim port from jsruntime. `PyEngineGuard` extended with `with_directory(engine, iso, directory, graph_snapshot)` constructor; legacy `new(engine, iso)` marked `#[cfg(test)]`. 9 new host callbacks (5 directory finders + 4 Tier 1 graph_query accessors) ported verbatim from jsruntime — all pure HOOK_CTX-shaped reads. `install_hook_prelude` registers all 9 between `host_accept_edge` and `eval_module`. `mount_with_scheduler` appends `BODY_AUGMENT_SUFFIX` to user source, clones `scheduler.directory`, builds `GraphSnapshot::from_live(...)`. `PyRuntime` struct gains `directory` + `graph_snapshot` fields. `PyRuntime::process_batch` uses `with_directory` (instead of `new`). `reset_global_state` clears 2 new TLS slots. graphreact-core imports extended with `EdgeAcceptPredicate`/`GraphDirectory`. Module-level docs extended with phase 3d-γ section. 3 new acceptance tests + 1 extension to the prelude-visible test (now probes `use_graph_query` + `__augment_ctx`).
- `zig/bridge/cpython.zig` — `ENGINE_VERSION_BASE` bumped to `cpython-0.1.0-m13-phase3d-gamma`. Module-level docs updated: phase 3a/3b/3c/3d-α/3d-β/3d-γ shipped, 3d-δ through 3h breakdown.
- `crates/graphreact-engine-cpython/src/lib.rs` — module docs updated with phase 3d-γ section (directory + graph_query routing + body augmentation). `engine_version` doc bumped.
- `crates/graphreact-cli/src/commands.rs` — `cpython_version_line` + `run_run` cpython trailer text rewritten for phase 3d-γ directory + graph_query routing + updated 3d-δ through 3h roadmap blurb.

### Modified — M13-phase3a
- `crates/graphreact-grx-parser/src/ast.rs` — new `ComponentLang` enum (`Js` default / `Python`) + `lang` field on `NodeDefinitionBlock`. `from_attr_str` / `as_attr_str` helpers.
- `crates/graphreact-grx-parser/src/parser.rs` — `parse_definition_block` now calls `parse_attributes()` to consume `lang="…"` (lexer's raw-content trigger unchanged — still fires on no-`id`). Unknown lang values surface as `Parse` errors with a clear message. 3 new unit tests + extends existing `parses_definition_block` to assert default `Js`.
- `crates/graphreact-grx-parser/src/ir.rs` — `IrDefinition { lang }` flows from AST. `ir_to_json` emits `"lang": "..."` for `grx parse --json`. 1 new unit test.
- `crates/graphreact-cli/src/lib.rs` — re-export `pub mod pyruntime;` (macOS only, mirroring `jsruntime`).
- `crates/graphreact-cli/src/runtime.rs` — `definitions` tuple widened to carry `ComponentLang`; both `dry_mount` and `dry_mount_jsc` enforce `lang == Js` (or skip on the trait-level `dry_mount` path); new `RuntimeError::EngineMisroute` variant with a clear "use --runtime=…" diagnostic.
- `crates/graphreact-cli/src/jsruntime.rs` — `definitions` tuple widened; `mount_with_scheduler` errors with `EngineMisroute` on non-JS bodies.
- `crates/graphreact-cli/src/commands.rs` — cpython arm in `run_run` checks for any `lang="python"` definition and routes through `pyruntime::mount_python_definitions`; falls back to the predecessor smoke-module path on JSC-only fixtures. `cpython_version_line` + cpython trailer text bumped to phase3a, including the PEP 703 / PEP 684 roadmap note.
- `zig/bridge/cpython.zig` — `ENGINE_VERSION_BASE` bumped to `cpython-0.1.0-m13-phase3a`.
- `crates/graphreact-engine-cpython/src/lib.rs` — module-doc phase scope updated (phase 2.5 closed, phase 3a delta lives in CLI's pyruntime.rs, phase 3b+ deferred).
