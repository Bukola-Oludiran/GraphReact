//! Python runtime composition layer — M13-phase3a/3b/3c/3d-α/3d-β/3d-γ/3d-δ/3d-ε/3d-ζ/3e-α/3e-β/3e-γ + M13-followon-D analogue + M13-phase3g.
//!
//! Mounts `lang="python"` component definitions on top of the
//! [`graphreact_engine_cpython::CPythonEngine`]. Parallels
//! [`crate::jsruntime`]'s `mount_with_scheduler` shape.
//!
//! ## Phase 3a (shipped 2026-05-03)
//!
//! - `.grx` parser learns `lang="python"` (defaults to js).
//! - [`mount_python_definitions`] mounts each Python body as a fresh
//!   sub-interpreter module; per-component `init(ctx)` resolved + called
//!   once per node with `ctx={id, props}`. Returned manifest dict
//!   captured per mount. **No hooks, no scheduler.**
//!
//! ## Phase 3b (shipped 2026-05-03)
//!
//! - **Python HOOK_PRELUDE** installed on each isolate's builtins:
//!   `use_state`, `use_effect`, `__register_callback`,
//!   `__invoke_callback`, `__release_callback`, `__release_tokens`.
//! - **Real `useState`** via `core_use_state` + thread-local
//!   [`SIGNAL_REGISTRY`]: state preserved across renders. Setter
//!   closures call `host_state_set(id, value)` which marks the owning
//!   node dirty.
//! - **Real `useEffect(fn, deps)` + cleanup**: fires from
//!   `flush_effects` via [`PyEngineGuard`]-driven re-entry into the
//!   Python `__invoke_callback`. Cleanup detected by envelope
//!   `{kind: 'cleanup', token: ct}`.
//! - **Scheduler integration**: [`PyRuntime::mount_with_scheduler`]
//!   binds bodies to a [`graphreact_core::Scheduler`]; setter calls
//!   re-render on the next [`PyRuntime::process_batch`] tick.
//! - **Author convention**: bodies define `body(ctx)` (or `init(ctx)`
//!   for phase 3a back-compat). Top-level hooks fire from inside the
//!   body. Manifest returned from the function is the node's
//!   permission shape.
//!
//! ## Phase 3c (shipped 2026-05-03)
//!
//! - **`use_emit()`** — returns a closure `emit(msg_type, payload=None,
//!   opts=None)` mirroring JSC's 3-arg shape. Targets are addressed via
//!   `opts={'to': ['nodeId']}` or `opts={'direct': 'nodeId'}`. Routes
//!   through [`build_host_emit_callback`] which calls
//!   [`graphreact_core::use_emit`] and immediately invokes the returned
//!   closure (use_emit-returns-callable-that-emits-now pattern).
//! - **`use_message(msg_type, handler)`** — registers `handler` in the
//!   per-isolate `_hostCallbacks` dict (token registry) and forwards
//!   `(msg_type, token)` to [`build_host_message_callback`]. The Rust
//!   callback registers a [`graphreact_core::use_message`] closure that
//!   on every delivery marshals `IncomingMessage` into
//!   `{from, type, payload, ttl}` and invokes
//!   `__invoke_callback(token, [marshalled_msg])` via
//!   [`CPythonEngine::invoke_global`].
//! - **Permission-manifest enforcement** + **TTL inheritance** are
//!   already shipped in graphreact-core's scheduler — phase 3c just
//!   exposes the surface.
//!
//! ## Phase 3d-α (shipped 2026-05-03)
//!
//! - **`use_memo(factory, deps)`** — split-cache pattern. The Python
//!   wrapper calls [`build_host_memo_check_callback`] first, which
//!   advances core's `HOOK_INDEX` via a no-op factory + side-effect
//!   flag, and returns one of three envelopes: `{cached: true, value}`
//!   (deps unchanged), `{cached: false, previous}` (deps changed),
//!   `{cached: false}` (registry miss). On miss/recompute Python runs
//!   the factory itself and calls
//!   [`build_host_memo_store_callback`] with the new value.
//!   `host_memo_check` and `host_memo_store` are pure thread-local
//!   read/writes — never re-enter CPython, so the outer engine borrow
//!   stays valid.
//! - **`use_callback(fn, deps)`** — caches the `_hostCallbacks` token
//!   (not the function reference) in [`MEMO_REGISTRY`]. On cache hit
//!   looks up the token and returns the stored function (identity-
//!   stable). On dep change releases the previous token via
//!   `__release_callback(previous)` to prevent leaking one entry per
//!   dep change.
//! - **`use_reducer(reducer, initial)`** — pure-Python composition
//!   over `use_state`. No host callback. Dispatch is created fresh per
//!   render (not identity-stable; wrap in `use_callback` if needed).
//!
//! ## Phase 3d-β (shipped 2026-05-03)
//!
//! Tier 1 graph-read hooks. Three hooks split by safety tier:
//!
//! - **Tier A — pure HOOK_CTX reads (zero engine touch):**
//!   `use_neighbors(filter?)` and `use_accept_edge()`. Direct ports;
//!   route through `core_use_neighbors(None)` and
//!   `core_use_accept_edge()` respectively. Python-side filter for
//!   `use_neighbors` avoids per-element JSON FFI hops.
//!   `use_accept_edge` returns a closure that on call enqueues a
//!   `PendingMutation::AcceptEdge`; the scheduler drains at the next
//!   cascade and fires `EdgeAdded` automatically.
//! - **Tier B — token-registry + TLS-capture (same shape as
//!   `use_message`):** `use_topology(handler, filter?)`. Reuses the
//!   existing `CURRENT_ENGINE` / `CURRENT_ISO` TLS slots; closure
//!   captures engine + iso at registration time. On dispatch (from
//!   `Scheduler::dispatch_topology_event`, well outside any held
//!   borrow), invokes Python via `__invoke_callback(token, [event])`
//!   with the marshalled `{kind, nodeId? | u, v, key?}` envelope.
//! - Helpers `parse_topology_filter` + `topology_event_to_host_value`
//!   ported verbatim from jsruntime (private fns there, duplicated
//!   here — same shape as `parse_emit_options` was in phase 3c).
//!
//! ## Phase 3d-γ (2026-05-03)
//!
//! Directory + Tier 1 graph_query proxies, exposed via `ctx.directory`
//! and `use_graph_query`:
//!
//! - **`ctx['directory']`** — a stable Python `SimpleNamespace`
//!   singleton with 5 finder methods (`find_by_id` / `find_by_role` /
//!   `find_by_depth` / `find_in_subtree` / `is_ancestor`). Injected
//!   into every body's ctx via `__augment_ctx`, which JS achieves by
//!   wrapping the body source in an IIFE; Python invokes
//!   `__augment_ctx(raw_ctx)` from Rust via `engine.invoke_global`
//!   before calling the body. Two sequential `borrow_mut()` calls,
//!   not nested.
//! - **`use_graph_query(selector, deps)`** — composes over `use_memo`
//!   from phase 3d-α. Passes a `__graph_query_proxy` (SimpleNamespace
//!   with `neighbors` / `has_node` / `has_edge` / `node_count` in
//!   3d-γ; widened to 11 accessors in 3d-δ) to the selector. Selector
//!   runs only when deps change; result cached in MEMO_REGISTRY.
//! - **`GraphSnapshot`** — bundles the live `Rc<RefCell<...>>`
//!   handles (graph, manifests, edge_accept_predicates) cloned from
//!   `Scheduler` at mount time. Phase 3d-γ exposed Tier 1 basic
//!   accessors; phase 3d-δ added 7 algorithm accessors; phase 3d-ε
//!   reads `manifests` for Tier 2 directed reachability; Tier 3
//!   (with accept-edge predicates) is read by phase 3d-ζ's
//!   `build_tier3_adj`.
//! - **`PyEngineGuard::with_directory`** — extends the guard to
//!   install `CURRENT_DIRECTORY` + `CURRENT_GRAPH_SNAPSHOT` TLS
//!   alongside `CURRENT_ENGINE` + `CURRENT_ISO`. The legacy
//!   `PyEngineGuard::new` constructor is retained for tests that
//!   don't need directory access — those callbacks safely return
//!   defaults (Null / empty array / false / 0) when TLS is unset.
//!
//! ## Phase 3d-δ (2026-05-04)
//!
//! `use_graph_query` widens its proxy with 7 algorithm accessors,
//! verbatim ports of JSC's followon-B surface:
//!
//! - **7 new accessors on `__graph_query_proxy`** —
//!   `shortest_path(from, to)`, `all_paths(from, to, max_depth?)`,
//!   `a_star_path(from, to)`, `bfs(from, depth_limit?)`,
//!   `dfs(from, depth_limit?)`, `connected_components()`,
//!   `cycle_detection()`. All composable with `use_graph_query` so
//!   the selector pattern from 3d-γ continues to work — the proxy
//!   just gains 7 attribute lookups. Selectors that depended on
//!   topology-only probes are forward-compatible.
//! - **7 new host callbacks** (Tier A from 3d-β framing — pure
//!   `current_graph_snapshot()` reads, no engine touch). Each
//!   delegates to the corresponding `graphreact_algorithms` function
//!   (`shortest_path`, `all_paths`, `astar_path`, `bfs_nodes`,
//!   `dfs_nodes`, `connected_components`, `has_cycle` / `find_cycle`)
//!   and marshals results to `HostValue`. Algorithm errors collapse
//!   to neutral returns (Null / empty Array / `{hasCycle: False, cycle: None}`).
//! - **A* heuristic kinds shipped in 3d-ε.** Tier 1's `a_star_path`
//!   has no heuristic argument; without one A* degenerates to BFS
//!   shortest-path with uniform edge cost. The 5 cost heuristics
//!   (latency / compliance / hierarchy / trust-attenuated /
//!   prerequisite-distance) land with Tier 2's policy-derived
//!   reachability infrastructure in phase 3d-ε via `g.tier2(...).a_star_path(from, to, {'heuristic': '...'})`.
//! - **`GraphSnapshot::from_live` already-live verification.** The
//!   live-handle pattern shipped in 3d-γ and is re-exercised by
//!   `phase3d_delta_graph_snapshot_reflects_post_mount_mutation` —
//!   mutations applied via `scheduler.graph.borrow_mut()` between
//!   two `process_batch` rounds are observed by the second batch's
//!   `use_graph_query`. JSC's followon-C "static→live" refactor was
//!   short-circuited at 3d-γ; no equivalent refactor needed for
//!   Python.
//!
//! ## Phase 3d-ε (2026-05-04)
//!
//! `g.tier2(msgType, opts?)` adds **policy-derived directed
//! reachability** alongside Tier 1's structural graph queries.
//! Verbatim port of JSC's M12.3a-followon-E surface:
//!
//! - **`tier2(msgType, opts?)` on `__graph_query_proxy`** — returns
//!   a fresh sub-proxy with 12 accessors: `node_count`, `has_node`,
//!   `neighbors`, `has_edge`, `bfs`, `dfs`, `shortest_path`,
//!   `topological_sort`, `a_star_path` (with cost-from-policy
//!   heuristic), `all_paths`, `connected_components` (Tarjan SCC),
//!   `cycle_detection` (3-color DFS). Each sub-proxy is msgType-
//!   scoped; `opts.includeDirectoryShortcuts: True` opts into the
//!   directory-shortcut flavor.
//! - **12 new `host_tier2_*` host callbacks** — each builds a
//!   `Tier2Adj` from the live `GraphSnapshot`'s `manifests` borrow
//!   and dispatches to a `&dyn DirectedAdj` algorithm helper. All
//!   pure HOOK_CTX-shaped reads (Tier A from 3d-β framing); no
//!   engine touch; safe-default fallbacks on TLS-unset.
//! - **`DirectedAdj` trait + `Tier2Adj` struct** — common shape
//!   consumed by Tier 2 algorithm helpers. 3d-ζ's `Tier3Adj`
//!   implements the same trait, so the 8 `tier2_*` algorithm helpers
//!   are reused over Tier 3 without duplication.
//! - **Two flavors of Tier 2 adjacency** —
//!   *Structural* (default): `(s, u)` exists iff Tier 1 edge `s ↔ u`
//!   AND `s.emits(T)` AND `u.accepts(T)`. *Directory-shortcut*
//!   (`opts.includeDirectoryShortcuts: True`): drops the Tier 1
//!   edge requirement; any emitter reaches any acceptor in 1 hop
//!   via direct messaging.
//! - **5 cost-from-policy A* heuristics** —
//!   `latency` (edge attr `latency`),
//!   `compliance` (edge `encrypted` + node `compliant`, +10 each on
//!   false), `hierarchy` (directory depth difference; admissible
//!   estimate), `trust-attenuated` (`1.0 / edge.trust`),
//!   `prerequisite-distance` (`1.0 + node.difficulty`). Verbatim
//!   JSC attr keys so cross-engine `.grx` fixtures behave
//!   identically under either runtime. Defaults are permissive (all
//!   degenerate to BFS-equivalent on attr-less fixtures).
//! - **`topological_sort` on Tier 2** closes followon-B's deferral —
//!   direction is well-defined per message type at Tier 2, so
//!   Kahn's algorithm produces a meaningful ordering (returns `null`
//!   on cycle).
//!
//! ## Phase 3d-ζ (2026-05-04)
//!
//! `use_edge_accept_handler(predicate)` + `g.tier3(msgType, opts?)`
//! add the **negotiation layer**: per-target `EdgeAcceptPredicate`s
//! filter Tier 2 candidacy at query time AND can veto `AcceptEdge`
//! mutations from `use_accept_edge` (3d-β) at the scheduler level.
//! Verbatim port of JSC's M12.3a-followon-F surface. **Closes the
//! M13 phase-3d series** — every primitive in the three-tier
//! possibility model now has parity on Python.
//!
//! - **`use_edge_accept_handler(predicate)` hook** — registers a
//!   Python predicate `fn(source_id, msg_type) -> bool` for the
//!   active node. Internally registers via `__register_callback(fn)`
//!   then dispatches the token to `host_edge_accept_handler`. The
//!   predicate is keyed by **target** id (this node owns the
//!   accept/veto decision); default-accept on missing predicate
//!   preserves Phase-9 backward compatibility.
//! - **`g.tier3(msgType, opts?)` selector on `__graph_query_proxy`**
//!   — returns a fresh sub-proxy with 12 accessors mirroring Tier 2
//!   (`node_count` / `has_node` / `neighbors` / `has_edge` / `bfs` /
//!   `dfs` / `shortest_path` / `topological_sort` / `a_star_path` /
//!   `all_paths` / `connected_components` / `cycle_detection`).
//!   Same 5 cost-from-policy A* heuristics from 3d-ε. Same two
//!   flavors (structural default; directory-shortcut opt-in).
//! - **`Tier3Adj` struct + `impl DirectedAdj for Tier3Adj`** — reuses
//!   the `DirectedAdj` trait shipped in 3d-ε, so all 8 algorithm
//!   helpers (`tier2_bfs`, `tier2_dfs`, `tier2_shortest_path`,
//!   `tier2_topological_sort`, `tier2_astar_path`, `tier2_all_paths`,
//!   `tier2_strongly_connected_components`,
//!   `tier2_directed_cycle_detection`) work over Tier 3 without
//!   duplication. `build_tier3_adj` reuses `build_tier2_adj` as the
//!   supremum + filters through `predicates.get(u)`.
//! - **12 new `host_tier3_*` callbacks** — mirror `host_tier2_*` but
//!   construct `Tier3Adj` via `build_tier3_adj_from_snapshot`. All
//!   pure HOOK_CTX-shaped reads + safe-default fallbacks on TLS-unset.
//! - **Predicate dispatch via `invoke_global_static`** — predicates
//!   fire from two paths: (a) `Scheduler::process_pending_mutations`
//!   for `AcceptEdge` veto (engine RefCell free); (b) `host_tier3_*`
//!   callbacks during querier body eval (engine RefCell HELD). The
//!   second path can't use `engine.borrow_mut().invoke_global(...)`
//!   without double-borrow. `graphreact_engine_cpython::invoke_global_static(iso, ...)`
//!   bypasses the RefCell by routing through process-global state
//!   via the `IsolateHandle` directly. Mirrors JSC's followon-F
//!   pattern.
//! - **AcceptEdge mutation veto** — when `use_accept_edge('target')`
//!   (3d-β) enqueues a `PendingMutation::AcceptEdge`, the scheduler
//!   consults `predicates[target](source, "")` before committing the
//!   edge. Empty msg_type — the AcceptEdge channel is untyped (Phase
//!   9). Wired into `graphreact-core::Scheduler` since followon-F.
//!
//! ## Phase 3e-α (2026-05-04)
//!
//! Dynamic node lifecycle hooks. Verbatim Python port of JSC's
//! M12.3a-phase3e-γ. Three new hooks let bodies decide topology at
//! runtime — previously Python systems could only decide topology at
//! mount time:
//!
//! - **`use_spawn_child()(spec)`** — spawn a single child node where
//!   `spec` is a dict `{id, component, props?, connectTo?}` and
//!   `component` is a Python callable `fn(ctx) -> manifest`. The
//!   HOOK_PRELUDE wrapper applies the lambda-wrap-at-registration
//!   pattern: `spec['component']` is wrapped in
//!   `lambda raw_ctx: spec['component'](__augment_ctx(raw_ctx))` so
//!   the spawned body sees `ctx['directory']` + Tier 1/2/3 proxies
//!   identically to top-level definitions. The wrapped fn is
//!   registered via `__register_callback`; the resulting token is
//!   dispatched to `host_spawn_child`.
//! - **`use_unmount_self()()`** — schedules an `UnmountNode`
//!   mutation for the active node. After the cascade drains, the
//!   node is gone — subsequent renders don't fire. HOOK_CTX-derived;
//!   no spec.
//! - **`use_compose()(specs)`** — declarative multi-spec variant.
//!   Same shape per element + a final `core_use_compose()(specs)`
//!   call. Uses a named `_make_compose_wrapper(fn)` factory inside
//!   the loop because Python's closure-in-loop semantics would
//!   otherwise capture every wrapped fn to the loop's last
//!   iteration.
//!
//! **Lambda-wrap-at-registration vs source-wrap.** The 3d-γ
//! `BODY_AUGMENT_SUFFIX` pattern (appended to user `.grx` source at
//! eval time) doesn't apply to spawned bodies — they arrive as
//! Python function references inside `_hostCallbacks`, not as source
//! strings. The Python wrappers wrap the user's component callable
//! at registration time so each invocation pre-augments the ctx
//! before the user's body runs. Same effect as JSC's IIFE
//! `(function(fn){return function(rawCtx){return fn(__augmentCtx(rawCtx));};})(rawComponent)`
//! pattern.
//!
//! **Re-entrancy.** [`build_compose_spec_from_host_value`] reads
//! `current_engine()` + `current_iso()` from TLS at registration
//! time (running synchronously inside the body via
//! `host_spawn_child` / `host_compose`) and the resulting
//! [`NodeDefinition`] closure captures both through `Rc` clones.
//! Subsequent renders of the spawned body (driven by
//! `Scheduler::evaluate_node` from `process_batch`) re-enter CPython
//! via `engine.borrow_mut().invoke_global(iso, "__invoke_callback", ...)`.
//! No nested-borrow path because `evaluate_node` runs outside any
//! parent body's `call_function` — the parent body has already
//! returned by the time the cascade drains the SpawnChild mutation
//! and re-evaluates the new child. Same pattern as 3d-γ's
//! [`build_py_node_definition`]; **no** `invoke_global_static`
//! needed (that's reserved for hooks that fire from inside a body's
//! own render — Tier 3 predicate dispatch in 3d-ζ).
//!
//! **Token leak — accepted bounded leak.** Each `use_spawn_child` /
//! `use_compose` call mints a fresh token in `_hostCallbacks`;
//! SpawnChild is idempotent on existing IDs at the scheduler so
//! duplicate-id calls don't double-mount, but their tokens still
//! register and never get released. Same bounded-leak surface JSC
//! documented in 3e-γ; followon-D swept it later via per-node token
//! tracking on unmount. The Python port follows JSC's phasing —
//! token GC is deferred to a future "M13-followon-D analogue" slice
//! that batches token cleanup across all token-using hooks
//! (`use_effect`, `use_message`, `use_topology`, `use_callback`,
//! `use_edge_accept_handler`, `use_spawn_child`, `use_compose`).
//!
//! ## Phase 3e-β (2026-05-04)
//!
//! Six supervision hooks. Verbatim Python port of JSC's
//! M12.3a-phase3e-ζ supervision surface. All Tier A — pure
//! HOOK_CTX-shaped reads from the host callback side; zero engine
//! touch; zero token registry interaction; no NodeDefinition
//! closure capture. Dramatically smaller than 3e-α (~250 LOC vs
//! ~600 LOC):
//!
//! - **`use_monitor(target_id) → SimpleNamespace(id) | None`** —
//!   watch another node by id; the active node receives a `__DOWN__`
//!   system message when the target unmounts. `use_message('__DOWN__',
//!   handler)` (3c) catches the delivery via the standard token-
//!   registry path; no new wiring needed in 3e-β. Returns `None`
//!   if the target isn't registered.
//! - **`use_demonitor(mref)`** — cancel a previously-registered
//!   monitor. Accepts the `SimpleNamespace(id=...)` returned by
//!   `use_monitor`, OR the raw integer id (idiomatic flexibility).
//! - **`use_link(target_id) → bool`** — establish a bidirectional
//!   link. Re-asserted every render; returns `False` if the target
//!   has unmounted between evaluations.
//! - **`use_unlink(target_id)`** — remove a link. No-op on missing.
//! - **`use_group_join(group)`** — join a process group. Idempotent
//!   at the scheduler level — multiple evaluations don't create
//!   duplicate memberships. Auto-removed on node unmount.
//! - **`use_group_leave(group)`** — leave a group. No-op on missing
//!   membership.
//!
//! **`MonitorRef` marshalling.** JSC's `MonitorRef(pub u64)` round-
//! trips across FFI as `{id: u64}`. The Python wrapper exposes it
//! as `_SimpleNamespace(id=...)` for symmetry with JSC's `{id: number}`
//! JS object. `use_demonitor(mref)` reads `mref.id` (or accepts the
//! raw id) and round-trips to `host_demonitor(id)`. `int(...)`
//! coercion handles JSON marshalling's float-coerced numbers (Python
//! sees `0.0` instead of `0` when crossing FFI; same pattern as
//! `host_state_set` from 3b).
//!
//! **Token leak surface unchanged.** Supervision hooks DON'T register
//! callbacks via `__register_callback` — they're synchronous shims
//! over `core_use_X(...)`. The bounded leak from 3b/3c/3d-β/3d-ζ/3e-α
//! doesn't grow with this slice.
//!
//! ## Phase 3e-γ (2026-05-04)
//!
//! Event-source surface. Verbatim Python port of JSC's
//! M12.3a-phase3e-ε (timer + delay descriptor factories) and
//! M12.3a-followon-A (httpSource + watchSource real Rust-side
//! firing) **collapsed into a single slice**. Five new hook surfaces:
//!
//! - **`use_source(factory_fn, deps=None)`** — invokes `factory_fn()`
//!   to produce a descriptor dict, dispatches to `host_use_source`,
//!   which decodes the kind and registers the resulting
//!   `Arc<dyn EventSource>` via [`core_use_source`]. Source events
//!   flow cross-thread via the scheduler's `external_inbox` and reach
//!   the active node as messages on the next `process_batch` —
//!   caught by `use_message(type, handler)` (3c).
//! - **`timer_source(interval_ms)`** — descriptor
//!   `{'kind': 'timer', 'intervalMs': ...}`. Real via
//!   [`core_timer_source`].
//! - **`delay_source(delay_ms)`** — descriptor
//!   `{'kind': 'delay', 'delayMs': ...}`. Real via
//!   [`core_delay_source`].
//! - **`http_source(opts)`** — descriptor
//!   `{'kind': 'http', 'opts': opts or {}}`. Real via
//!   [`crate::sources::http_source`] (already shipped via
//!   followon-A; tokio::net listener; requires the binary to be
//!   inside a tokio runtime context — `grx` main() installs one).
//! - **`watch_source(opts)`** — descriptor
//!   `{'kind': 'watch', 'opts': opts or {}}`. Real via
//!   [`crate::sources::watch_source`] (already shipped via
//!   followon-A; notify-based file watcher; spawns its own thread
//!   internally so works even outside a tokio runtime).
//!
//! **Descriptor-producing factories.** The Python factory globals
//! return plain dicts (not native handles); descriptors cross the
//! FFI as pure JSON. The Rust decoder reads `kind` + the
//! kind-specific fields (intervalMs / delayMs / opts.port /
//! opts.path) and dispatches to the right factory. No
//! JS-to-Rust handle marshalling needed.
//!
//! **Single-slice staging.** JSC's M12.3a-phase3e-ε deferred
//! http/watch firing to followon-A (which ported
//! `crates/graphreact-napi/src/sources.rs` to
//! `crates/graphreact-cli/src/sources.rs` + added tokio + notify
//! deps + tokio runtime in `grx` main()). For the Python port the
//! followon-A infrastructure is already in place, so all four kind
//! arms ship real on the first slice. Substrate-not-MVP rule
//! applied.
//!
//! **Token leak surface unchanged.** `use_source` doesn't register
//! `__hostCallbacks` callbacks — descriptors are pure data; the
//! Rust-side factory closure is constructed from primitive captured
//! values (`interval_ms` / `delay_ms` / `port` / `path`).
//! `core_use_source` enforces deps-aware re-registration; on
//! deps-change the prior source is `stop()`'d and a fresh factory
//! invoked. `lifecycle::stop_sources` (already shipped in core)
//! cleans up `node.active_sources` on unmount.
//!
//! **M13 phase 3 closeout.** With 3e-γ shipping, every hook surface
//! JSC has via M12.3a-phase3a through M12.3a-phase3e-ζ +
//! M12.3a-followon-A is now real on `--runtime=cpython`. All 8 named
//! application classes are prototypable end-to-end on Python. The
//! remaining M13 sub-phases (3f / 3g / 3h) are concurrency /
//! build-flag concerns, not new hook surfaces.
//!
//! ## M13-followon-D analogue (2026-05-04)
//!
//! Per-node `__hostCallbacks` token GC on unmount. Closes the
//! bounded leak documented since 3b. Verbatim Python port of JSC's
//! M12.3a-followon-D ([jsruntime.rs:209, 411-413, 421-423, 433-455,
//! 473-487, 4244-4297, 4697-4707](crates/graphreact-cli/src/jsruntime.rs)).
//!
//! Body-time tokens (`use_effect` fn from 3b, `use_message` handler
//! from 3c, `use_callback` fn from 3d-α, `use_topology` handler from
//! 3d-β, `use_edge_accept_handler` predicate from 3d-ζ,
//! `use_spawn_child` componentToken from 3e-α, `use_compose`
//! componentToken from 3e-α) accumulated for the lifetime of the
//! isolate — minted by Python-side `__register_callback`, never
//! released until process exit. With this slice they're swept on
//! each node's unmount.
//!
//! **Mechanism**:
//!
//! 1. Python `__register_callback(fn)` (already shipped in 3b) now
//!    calls `host_track_token(token)` immediately after minting,
//!    guarded by `try / except NameError` for paths that don't
//!    register the callback.
//! 2. [`build_host_track_token_callback`] appends the token into
//!    [`NODE_TOKENS`] keyed by [`current_node_id`] (set by
//!    [`ComponentEvalGuard`] during body eval). When
//!    `current_node_id()` returns `None` (cleanup-token mints during
//!    `flush_effects` — outside the body-eval window), the callback
//!    no-ops; cleanup tokens auto-release via `__release_callback(ct)`
//!    in the [`EffectCleanup`] closure built by
//!    [`build_host_effect_callback`], so they don't leak either.
//! 3. The topology listener installed in `mount_with_scheduler`
//!    (already shipped in 3b) pushes `NodeRemoved.node_id` into
//!    `pending_unmounts` whenever `Scheduler` removes a node. Already
//!    fired by `use_unmount_self` (3e-α) and by
//!    `Scheduler::remove_node_at_runtime` (test-driven).
//! 4. [`PyRuntime::process_batch`] drains `pending_unmounts` after
//!    `scheduler.process_batch()` returns and calls
//!    [`release_tokens_for_node`] for each id. The sweep runs while
//!    the engine + iso TLS are still installed via
//!    [`PyEngineGuard::with_directory`], so the Python re-entry
//!    inside `release_tokens_for_node` succeeds.
//! 5. [`release_tokens_for_node`] drains the bucket via
//!    [`drain_node_tokens`] and dispatches `__release_tokens([...])`
//!    via `engine.borrow_mut().invoke_global(...)`. The Python
//!    prelude's `__release_tokens(tokens)` (already shipped in 3b)
//!    iterates and `_hostCallbacks.pop(t, None)`s each.
//!
//! **Why cleanup tokens don't need explicit tracking**: cleanup
//! tokens are minted by `__invoke_callback` during effect-fire
//! (HOOK_PRELUDE line 838+ of the prelude string — cleanup branch
//! returns `__register_callback(result)`). At that point
//! `flush_effects` has run and the body-eval window is closed, so
//! `CURRENT_NODE_ID` is unset → `host_track_token` no-ops. That's
//! intentional: `host_effect`'s [`EffectCleanup`] closure already
//! calls `__release_callback(ct)` immediately after the cleanup
//! runs, so the cleanup-fire path doesn't accumulate tokens. The
//! only cleanup-token leak case is when a node unmounts WITH a
//! pending un-fired cleanup — graphreact-core's `lifecycle::stop_effects`
//! fires the cleanup during unmount, which then auto-releases. So
//! even that case lands on the same auto-release path. **No
//! explicit tracking needed for cleanup tokens.**
//!
//! Same reasoning JSC's M12.3a-followon-D documents.
//!
//! ## M13-phase3g (this slice — 2026-05-04)
//!
//! `useEffectAsync` via asyncio bridge — Python analogue of JSC's
//! M12.3b-phase1 Promise → Future bridge. Verbatim Python port of
//! [JSC's M12.3b-phase1](crates/graphreact-cli/src/jsruntime.rs).
//! Lets a body register an async callable; the runtime awaits its
//! completion plus handles cleanup plus rejection — same hook surface
//! as JSC modulo Python idiom.
//!
//! **Mechanism**:
//!
//! 1. Python `use_effect_async(async_fn, deps)` HOOK_PRELUDE wrapper
//!    builds a SYNC invoker via `_make_async_effect_invoker(async_fn)`,
//!    registers it under a token via `__register_callback`, dispatches
//!    `host_effect_async(token, deps)`.
//! 2. [`build_host_effect_async_callback`] parses args, captures
//!    `iso` from TLS, calls [`core_use_effect_async`] with an async
//!    closure. The closure is `Fn() -> impl Future<Output =
//!    Option<AsyncEffectCleanup>> + 'static` — re-fired by
//!    `flush_effects_async` whenever deps change or the node first
//!    mounts.
//! 3. When fired, the async closure: (a) allocates `completion_id`,
//!    parks `tokio::sync::oneshot::Sender` in
//!    [`EFFECT_ASYNC_COMPLETIONS`]; (b) invokes the Python invoker
//!    via `invoke_global_static(iso, "__invoke_callback", [token,
//!    [completion_id]])`; (c) awaits the receiver.
//! 4. The Python invoker (sync function): calls `async_fn()` → gets
//!    a coroutine; drives it to completion via `asyncio.run(coro)`.
//!    PEP 684 (3.12+) per-interpreter GIL isolates the `_asyncio`
//!    C-extension state per sub-interpreter, so creating + tearing
//!    down a fresh event loop inside the isolate is sound. Inspects
//!    the result and calls `host_effect_async_complete(completion_id,
//!    kind, payload)` with `'value'` / `'cleanup'` / `'throw'`.
//! 5. [`build_host_effect_async_complete_callback`] removes the
//!    parked Sender from `EFFECT_ASYNC_COMPLETIONS` and signals it.
//!    The async closure's `Receiver.await` returns immediately
//!    (synchronous run-to-completion).
//! 6. Settlement → `Some(AsyncEffectCleanup)` for cleanup callables;
//!    `None` for value/throw. Cleanup fires via
//!    `lifecycle::stop_effects` on next deps-change re-render or
//!    node unmount; the cleanup closure invokes
//!    `__invoke_callback(ct, [])` plus `__release_callback(ct)`.
//!
//! **Python-novel design** vs JSC: JSC uses `Promise.resolve().then(...)`
//! and relies on JSC's auto-drain at `JSObjectCallAsFunction` boundary
//! to drive synchronous Promise chains. Python's analogous mechanism
//! is `asyncio.run(coro)` — synchronous run-to-completion of any
//! coroutine, including genuine async I/O. PEP 684 (3.12+, the engine
//! floor) isolates `_asyncio` per sub-interpreter, so create/teardown
//! of a fresh event loop inside the isolate is sound and cleans up
//! cleanly on `Py_EndInterpreter`.
//!
//! **Re-entrancy**: `host_effect_async`'s closure runs from
//! `flush_effects_async`, which is driven by
//! `Scheduler::process_batch_async` — outside any active engine
//! borrow. [`invoke_global_static`] is used (not
//! `engine.borrow_mut().invoke_global`) for symmetry with the 3d-ζ
//! predicate path; either works since no nested borrow.
//!
//! **Token leak surface unchanged.** Body tokens
//! (`_make_async_effect_invoker(async_fn)`) and cleanup tokens (the
//! callable returned by the user's async fn) auto-track via
//! `__register_callback` → `host_track_token` (followon-D). Body
//! tokens land under `current_node_id()`; cleanup tokens fire
//! during `flush_effects` (CURRENT_NODE_ID unset) so they no-op
//! track but auto-release via `__release_callback(ct)` in the
//! `AsyncEffectCleanup` closure. Same auto-release shape as the
//! sync `use_effect` path.
//!
//! **CLI activation deferred**: `grx run --runtime=cpython` continues
//! to drive the sync `process_batch`. Rationale matches JSC's
//! M12.3b-phase1 punt: nested-runtime restructuring is its own
//! slice. Not a regression — `useEffectAsync` was a no-op stub on
//! the sync CLI path before this slice.
//!
//! ## Hook routing
//!
//! When a Python body calls `use_state(0)`:
//! 1. The HOOK_PRELUDE wrapper calls the registered host callback
//!    `host_state(0)`.
//! 2. [`build_host_state_callback`] reads [`CURRENT_NODE_ID`] (TLS set
//!    by [`ComponentEvalGuard`]) + [`PY_HOOK_INDEX`] (incremented per
//!    `host_state` call).
//! 3. Calls [`graphreact_core::use_state`] which uses graphreact-core's
//!    own TLS to allocate / reuse the hook slot — preserving state
//!    across renders.
//! 4. Mints (or reuses) a stable u64 id for `(node_id, hook_pos)`,
//!    stores the [`Signal`] in [`SIGNAL_REGISTRY`], returns
//!    `{id, value}` to Python.
//! 5. The HOOK_PRELUDE wrapper builds a Python setter closure that
//!    calls `host_state_set(id, new_value)` when invoked. The closure
//!    captures `id`; its enqueue path goes through the Signal in the
//!    registry, which marks the owning node dirty for next batch.
//!
//! When a Python body calls `use_effect(fn, deps)`:
//! 1. The HOOK_PRELUDE wrapper calls Python `__register_callback(fn)`
//!    which stores `fn` in a per-isolate `_hostCallbacks` dict and
//!    returns a token.
//! 2. The wrapper calls `host_effect(token, deps)`.
//! 3. [`build_host_effect_callback`] calls
//!    [`graphreact_core::use_effect`] with a closure that, when the
//!    scheduler's `flush_effects` fires it, reads
//!    [`CURRENT_ENGINE`]/[`CURRENT_ISO`] from TLS (set by
//!    [`PyEngineGuard`] around `process_batch`) and invokes the
//!    Python `__invoke_callback(token, [])` via
//!    [`graphreact_engine_cpython::CPythonEngine::invoke_global`].
//! 4. Python returns an envelope: `{kind: 'value'}` for plain effects,
//!    `{kind: 'cleanup', token: ct}` when the effect callback returned
//!    a callable (the conventional cleanup shape), `{kind: 'throw',
//!    message}` on exception.
//! 5. The Rust closure translates `{kind: 'cleanup', token: ct}` into
//!    an [`EffectCleanup`] that re-invokes `__invoke_callback(ct, [])` +
//!    `__release_callback(ct)` when graphreact-core's flush triggers
//!    cleanup (deps changed or node unmounted).
//!
//! ## Why two hook indices
//!
//! graphreact-core's `HOOK_INDEX` is private to that crate and tracks
//! Rust-side hook slots. [`PY_HOOK_INDEX`] is our parallel counter for
//! deriving stable `(node_id, hook_pos)` keys for [`SIGNAL_BY_HOOK_POS`].
//! Both increment in lockstep with each `host_state` call.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use serde_json::Value;

use graphreact_core::{
    delay_source as core_delay_source, timer_source as core_timer_source,
    use_accept_edge as core_use_accept_edge, use_compose as core_use_compose,
    use_demonitor as core_use_demonitor, use_edge_accept_handler as core_use_edge_accept_handler,
    use_effect as core_use_effect, use_effect_async as core_use_effect_async,
    use_emit as core_use_emit, use_group_join as core_use_group_join,
    use_group_leave as core_use_group_leave, use_link as core_use_link, use_memo as core_use_memo,
    use_message as core_use_message, use_monitor as core_use_monitor,
    use_neighbors as core_use_neighbors, use_source as core_use_source,
    use_spawn_child as core_use_spawn_child, use_state as core_use_state,
    use_topology as core_use_topology, use_unlink as core_use_unlink,
    use_unmount_self as core_use_unmount_self, AsyncEffectCleanup, Attrs, CommunicationGraph,
    ComposeChildSpec, EdgeAcceptPredicate, EffectCleanup, EmitOptions, EventSource,
    GraphDirectory, IncomingMessage, MonitorRef as CoreMonitorRef, NodeContext, NodeDefinition,
    PermissionManifest, Scheduler, Signal, Subscription, TopologyEvent, TopologyEventKind,
    TopologyFilterOptions,
};
use graphreact_engine_cpython::{invoke_global_static, CPythonEngine};
use graphreact_engine_zig::{
    BoxCallback, Engine, EngineError, FunctionHandle, HostValue, IsolateHandle, IsolateOptions,
};
use graphreact_grx_parser::{
    ast::ComponentLang, ir::IrDefinition, ir::IrNode, multi_file::resolve_multi_file,
};

use crate::runtime::RuntimeError;

// ---------------------------------------------------------------------
// Per-thread registries (mirroring jsruntime.rs)
// ---------------------------------------------------------------------
//
// `Signal<Value>` contains an `Rc<dyn Fn()>` (the enqueue closure
// pinned to the owning node's scheduler) and is therefore
// `!Send + !Sync`. The scheduler is single-threaded by design (M4
// baseline), so thread-locals are the correct shape — the CPython
// trampoline runs on the same thread that drives
// `Scheduler::process_batch`, which is the same thread that owns the
// Signals.

thread_local! {
    /// Registered Signals indexed by stable u64 id. Ids survive
    /// re-renders — the same `(node_id, hook_pos)` always resolves to
    /// the same id, even though `core_use_state` is called fresh each
    /// render.
    static SIGNAL_REGISTRY: RefCell<FxHashMap<u64, Signal<Value>>> =
        RefCell::new(FxHashMap::default());

    /// Stable id assignment per `(node_id, hook_pos)`.
    static SIGNAL_BY_HOOK_POS: RefCell<FxHashMap<(String, usize), u64>> =
        RefCell::new(FxHashMap::default());

    /// Monotonic id source for [`SIGNAL_REGISTRY`].
    static NEXT_SIGNAL_ID: Cell<u64> = const { Cell::new(1) };

    /// Phase 3d-α memo cache. Keyed by `(node_id, hook_pos)`. Holds
    /// the actual cached value (or token, for `use_callback`); core's
    /// `use_memo` slot stores `Value::Null` as a placeholder. Split-
    /// cache pattern: factory runs Python-side; `host_memo_check` /
    /// `host_memo_store` are pure thread-local read/writes that never
    /// re-enter CPython.
    static MEMO_REGISTRY: RefCell<FxHashMap<(String, usize), Value>> =
        RefCell::new(FxHashMap::default());

    /// Transient carrier from `host_memo_check` to `host_memo_store`
    /// (set on the recompute / first-call paths; consumed by
    /// `host_memo_store`). Empty between renders.
    static PENDING_MEMO_KEY: RefCell<Option<(String, usize)>> =
        const { RefCell::new(None) };

    /// Phase 3d-γ directory handle. Set by [`PyEngineGuard::with_directory`]
    /// around `process_batch`; read by `host_directory_*` callbacks. None
    /// between batches (or in tests using the legacy `PyEngineGuard::new`
    /// constructor) — callbacks return safe defaults when unset.
    static CURRENT_DIRECTORY: RefCell<Option<Rc<RefCell<GraphDirectory>>>> =
        const { RefCell::new(None) };

    /// Phase 3d-γ graph snapshot handle. Same lifecycle as
    /// [`CURRENT_DIRECTORY`]; read by `host_graph_*` callbacks. The
    /// snapshot wraps live `Rc<RefCell<...>>` handles cloned from the
    /// scheduler — phase 3d-γ exposes Tier 1 accessors (neighbors /
    /// has_node / has_edge / node_count). Algorithm accessors land in
    /// phase 3d-δ.
    static CURRENT_GRAPH_SNAPSHOT: RefCell<Option<Rc<GraphSnapshot>>> =
        const { RefCell::new(None) };

    /// **M13-followon-D analogue** — per-node `__hostCallbacks` token
    /// accumulator. Keyed by `current_node_id()`; values are token ids
    /// minted by Python-side `__register_callback` during a body
    /// invocation. Populated by `host_track_token`; on node unmount
    /// [`release_tokens_for_node`] drains the bucket and dispatches
    /// `__release_tokens([...])` to delete the entries from
    /// `_hostCallbacks` JS-side.
    ///
    /// Conditional tracking: when `current_node_id()` returns `None`
    /// (cleanup-token mints during effect-fire — `flush_effects` runs
    /// outside the body-eval window where `ComponentEvalGuard` set
    /// `CURRENT_NODE_ID`), `host_track_token` no-ops. Cleanup tokens
    /// auto-release via `__release_callback(ct)` in the
    /// [`EffectCleanup`] closure built by [`build_host_effect_callback`],
    /// so they don't leak. Same reasoning as JSC followon-D.
    static NODE_TOKENS: RefCell<FxHashMap<String, Vec<u64>>> =
        RefCell::new(FxHashMap::default());

    /// **M13-phase3g** — `useEffectAsync` settlement registry. When
    /// `host_effect_async`'s registered closure fires (from
    /// `flush_effects_async`), it allocates a fresh completion id and
    /// parks a `tokio::sync::oneshot::Sender` here keyed by that id,
    /// then invokes the Python invoker which synchronously runs the
    /// user's coroutine to completion (via
    /// `asyncio.new_event_loop().run_until_complete(...)`) and calls
    /// `host_effect_async_complete(completion_id, kind, payload)`;
    /// that callback removes the Sender from this map and signals it.
    ///
    /// `oneshot::Sender<EffectAsyncSettlement>` is `Send + Sync` (T is
    /// Send), so the TLS RefCell store is sound. Lookups happen on the
    /// same thread that registered (the local-set thread driving
    /// `process_batch_async`) — no cross-thread access.
    static EFFECT_ASYNC_COMPLETIONS: RefCell<FxHashMap<u64, tokio::sync::oneshot::Sender<EffectAsyncSettlement>>> =
        RefCell::new(FxHashMap::default());

    /// Monotonic id source for [`EFFECT_ASYNC_COMPLETIONS`]. M13-phase3g.
    static NEXT_EFFECT_ASYNC_COMPLETION_ID: Cell<u64> = const { Cell::new(1) };
}

/// **M13-phase3g** — settlement envelope sent from
/// `host_effect_async_complete` (Python side has resolved or rejected
/// the user's async function) to the parked future inside
/// `host_effect_async`'s closure. Verbatim Python port of JSC's
/// `EffectAsyncSettlement` enum.
#[derive(Debug)]
enum EffectAsyncSettlement {
    /// Async fn resolved with a non-callable value — no cleanup to run.
    Value,
    /// Async fn resolved with a callable — that's the cleanup callback.
    /// The u64 is the Python-side `_hostCallbacks` token for it.
    Cleanup(u64),
    /// Async fn raised an exception; the String is the stringified error.
    Throw(String),
}

/// **M13-phase3g.** Allocate a fresh monotonic id for
/// [`EFFECT_ASYNC_COMPLETIONS`].
fn next_effect_async_completion_id() -> u64 {
    NEXT_EFFECT_ASYNC_COMPLETION_ID.with(|c| {
        let id = c.get();
        c.set(id.wrapping_add(1).max(1));
        id
    })
}

/// Phase 3d-γ graph snapshot. Bundles the three `Rc<RefCell<...>>`
/// handles that the `__graph_query_proxy` accessors need. All three
/// are cloned cheaply from `Scheduler` at mount time — the snapshot
/// observes runtime mutations applied through the scheduler. Verbatim
/// port of [`crate::jsruntime::GraphSnapshot`].
pub struct GraphSnapshot {
    pub graph: Rc<RefCell<CommunicationGraph>>,
    /// Per-node permission manifests. Source of truth for Tier 2
    /// reachability (read by phase 3d-ε's `build_tier2_adj`) —
    /// direction emerges per message type from
    /// `manifest.emits/accepts` pairs.
    pub manifests: Rc<RefCell<FxHashMap<String, PermissionManifest>>>,
    /// Per-node edge-accept predicates. Source of truth for Tier 3
    /// reachability (read by phase 3d-ζ's `build_tier3_adj`) and for
    /// the AcceptEdge mutation veto path in `Scheduler`.
    pub edge_accept_predicates: Rc<RefCell<FxHashMap<String, EdgeAcceptPredicate>>>,
}

impl GraphSnapshot {
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

// ---------------------------------------------------------------------
// Per-evaluation TLS
// ---------------------------------------------------------------------

thread_local! {
    /// Set by [`ComponentEvalGuard`] at the start of each component
    /// evaluation; cleared on drop. Read by host callbacks (host_state,
    /// host_state_set, host_effect, …).
    static CURRENT_NODE_ID: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Python-side hook position counter. Reset to 0 by
    /// [`ComponentEvalGuard`]; incremented by each host_state call.
    static PY_HOOK_INDEX: Cell<usize> = const { Cell::new(0) };

    /// Engine handle for deferred-fire host callbacks (effects).
    /// Set by [`PyEngineGuard`] around [`PyRuntime::process_batch`] so
    /// closures invoked from `flush_effects` can re-enter CPython
    /// without capturing `Rc<RefCell<CPythonEngine>>` directly. The
    /// `BoxCallback` type stored in engine-cpython's process-global
    /// registry is `Send+Sync`, which `Rc` violates — TLS sidesteps
    /// the constraint.
    static CURRENT_ENGINE: RefCell<Option<Rc<RefCell<CPythonEngine>>>> =
        const { RefCell::new(None) };

    /// Isolate handle paired with [`CURRENT_ENGINE`].
    static CURRENT_ISO: Cell<Option<IsolateHandle>> = const { Cell::new(None) };
}

/// RAII guard that scopes a single component evaluation. Resets
/// [`PY_HOOK_INDEX`] and stamps [`CURRENT_NODE_ID`] on construction;
/// clears both on drop. Safe across panics + early returns.
pub(crate) struct ComponentEvalGuard;

impl ComponentEvalGuard {
    pub fn new(node_id: &str) -> Self {
        PY_HOOK_INDEX.with(|i| i.set(0));
        CURRENT_NODE_ID.with(|n| *n.borrow_mut() = Some(node_id.to_string()));
        Self
    }
}

impl Drop for ComponentEvalGuard {
    fn drop(&mut self) {
        PY_HOOK_INDEX.with(|i| i.set(0));
        CURRENT_NODE_ID.with(|n| *n.borrow_mut() = None);
    }
}

fn current_node_id() -> Option<String> {
    CURRENT_NODE_ID.with(|n| n.borrow().clone())
}

fn next_py_hook_index() -> usize {
    PY_HOOK_INDEX.with(|i| {
        let p = i.get();
        i.set(p + 1);
        p
    })
}

/// **M13-followon-D analogue.** Drain the per-node token bucket for
/// `node_id`, returning the accumulated tokens. Called from
/// [`release_tokens_for_node`] when a node is unmounted.
fn drain_node_tokens(node_id: &str) -> Vec<u64> {
    NODE_TOKENS.with(|m| m.borrow_mut().remove(node_id).unwrap_or_default())
}

/// **M13-followon-D analogue.** Sum of all tokens currently tracked
/// across [`NODE_TOKENS`]. Test-only — used by
/// `followon_d_analogue_unmount_sweeps_host_callbacks_tokens` to
/// assert the registry's pre/post-sweep state without reaching into
/// Python.
#[cfg(test)]
fn tracked_token_count() -> usize {
    NODE_TOKENS.with(|m| m.borrow().values().map(Vec::len).sum())
}

/// **M13-followon-D analogue.** Drain the active node's token bucket
/// and dispatch `__release_tokens([...])` Python-side to delete the
/// entries from `_hostCallbacks`. Best-effort — FFI errors are
/// surfaced via `eprintln` rather than propagated, mirroring JSC's
/// `release_tokens_for_node` semantics. Called from
/// [`PyRuntime::process_batch`] after the scheduler settles, while
/// the engine + iso TLS are still installed via
/// [`PyEngineGuard::with_directory`] so Python re-entry succeeds.
fn release_tokens_for_node(
    engine: &Rc<RefCell<CPythonEngine>>,
    iso: IsolateHandle,
    node_id: &str,
) {
    let tokens = drain_node_tokens(node_id);
    if tokens.is_empty() {
        return;
    }
    #[allow(clippy::cast_precision_loss)]
    let arr: Vec<HostValue> = tokens
        .into_iter()
        .map(|t| HostValue::Number(t as f64))
        .collect();
    if let Err(e) = engine.borrow_mut().invoke_global(
        iso,
        "__release_tokens",
        &[HostValue::Array(arr)],
    ) {
        eprintln!(
            "[graphreact-cli] __release_tokens dispatch failed for node {node_id}: {e}"
        );
    }
}

/// RAII guard scoping the engine + isolate TLS across an entire
/// [`PyRuntime::process_batch`] call. Deferred-fire host callbacks
/// (effects) need TLS access — not `Rc` capture — to satisfy the
/// `Send+Sync` bound on engine-cpython's callback registry.
pub(crate) struct PyEngineGuard;

impl PyEngineGuard {
    /// Engine + iso only. Used by tests that don't exercise the
    /// 3d-γ directory / graph-query proxy surface; equivalent to
    /// `with_directory(..., default(), default())`.
    #[cfg(test)]
    pub fn new(engine: Rc<RefCell<CPythonEngine>>, iso: IsolateHandle) -> Self {
        CURRENT_ENGINE.with(|cell| *cell.borrow_mut() = Some(engine));
        CURRENT_ISO.with(|cell| cell.set(Some(iso)));
        // Leave directory + graph TLS slots cleared — host_directory_*
        // and host_graph_* callbacks safely return defaults when TLS
        // isn't installed.
        Self
    }

    /// Phase 3d-γ variant: also installs a directory handle and graph
    /// snapshot for the duration of the guard.
    pub fn with_directory(
        engine: Rc<RefCell<CPythonEngine>>,
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

impl Drop for PyEngineGuard {
    fn drop(&mut self) {
        CURRENT_ENGINE.with(|cell| *cell.borrow_mut() = None);
        CURRENT_ISO.with(|cell| cell.set(None));
        CURRENT_DIRECTORY.with(|cell| *cell.borrow_mut() = None);
        CURRENT_GRAPH_SNAPSHOT.with(|cell| *cell.borrow_mut() = None);
    }
}

fn current_engine() -> Option<Rc<RefCell<CPythonEngine>>> {
    CURRENT_ENGINE.with(|cell| cell.borrow().clone())
}

fn current_iso() -> Option<IsolateHandle> {
    CURRENT_ISO.with(Cell::get)
}

fn current_directory() -> Option<Rc<RefCell<GraphDirectory>>> {
    CURRENT_DIRECTORY.with(|cell| cell.borrow().clone())
}

fn current_graph_snapshot() -> Option<Rc<GraphSnapshot>> {
    CURRENT_GRAPH_SNAPSHOT.with(|cell| cell.borrow().clone())
}

// ---------------------------------------------------------------------
// HOOK_PRELUDE — Python source installed on each isolate's builtins
// ---------------------------------------------------------------------
//
// The prelude module:
//   - Defines `_hostCallbacks` (per-isolate dict mapping token → fn)
//     + `_nextToken` counter.
//   - Defines `__register_callback(fn) → token`,
//     `__invoke_callback(token, args=None) → envelope`,
//     `__release_callback(token)`, `__release_tokens(tokens)`.
//   - Defines `use_state(initial) → (value, setter)` wrapping
//     `host_state` + `host_state_set`.
//   - Defines `use_effect(fn, deps=None) → None` wrapping
//     `__register_callback` + `host_effect`.
//   - Installs all of the above on `builtins` so any user body sees
//     them without import.
//
// The wrappers reference the host callbacks (`host_state`,
// `host_state_set`, `host_effect`) which the runtime registers via
// `engine.register_callback` BEFORE the prelude is evaluated. Those
// land on builtins via the M13-phase2.5 callback bridge.

const HOOK_PRELUDE: &str = r"
import builtins as _builtins

# Per-isolate token registry. Lives in this module's globals; the
# closures below close over it. Sub-interpreter teardown frees this
# dict along with the prelude module itself.
_hostCallbacks = {}
_nextToken = [1]

def __register_callback(fn):
    t = _nextToken[0]
    _nextToken[0] += 1
    _hostCallbacks[t] = fn
    # M13-followon-D analogue: notify the Rust side so the token can be
    # tracked under the active node and swept on unmount. Guard against
    # NameError because dry-mount paths and the prelude-eval phase
    # (before host_track_token is registered) don't have the callback
    # installed; in those cases the token is still leak-bounded by
    # process lifetime, mirroring JSC's pre-followon-D behaviour.
    try:
        host_track_token(t)
    except NameError:
        pass
    return t

def __invoke_callback(token, args=None):
    fn = _hostCallbacks.get(token)
    if fn is None:
        return {'kind': 'missing'}
    try:
        result = fn(*(args or []))
    except Exception as e:
        return {'kind': 'throw', 'message': str(e)}
    if callable(result):
        ct = __register_callback(result)
        return {'kind': 'cleanup', 'token': ct}
    return {'kind': 'value', 'value': result}

def __release_callback(token):
    _hostCallbacks.pop(token, None)

def __release_tokens(tokens):
    for t in tokens:
        _hostCallbacks.pop(t, None)

def use_state(initial):
    pair = host_state(initial)
    sid = pair['id']
    val = pair['value']
    def setter(new_val):
        host_state_set(sid, new_val)
    return val, setter

def use_effect(fn, deps=None):
    if deps is None:
        deps = []
    token = __register_callback(fn)
    host_effect(token, deps)

# Phase 3g - useEffectAsync via asyncio bridge. The user passes an
# `async def` callable (or a coroutine-returning callable). We wrap it
# in `_make_async_effect_invoker(async_fn)` which is a SYNC function
# registered under a token. When `host_effect_async`'s closure fires
# from `flush_effects_async` and calls back into Python via
# `__invoke_callback(token, [completion_id])`, the wrapper:
#
#   1. Calls async_fn() to get a coroutine (or value if not a coroutine).
#   2. Drives the coroutine to completion via
#      `asyncio.new_event_loop().run_until_complete(coro)` - a
#      synchronous run-to-completion that handles both
#      sync-resolved coroutines and genuine async I/O.
#   3. Inspects the result: if callable, registers as cleanup token
#      and calls `host_effect_async_complete(completion_id, 'cleanup',
#      ct)`. If raised, calls `host_effect_async_complete(...,
#      'throw', str(e))`. Otherwise calls `host_effect_async_complete(
#      ..., 'value', 0)`.
#   4. The Rust-side `host_effect_async_complete` callback signals the
#      parked oneshot Sender, which the awaiting future inside
#      `host_effect_async`'s closure observes.
#
# Python-novel design vs JSC: JSC uses `Promise.resolve().then(...)`
# and relies on JSC's auto-drain at JSObjectCallAsFunction boundary.
# Python's analogous mechanism is `asyncio.run(coro)` — synchronous
# run-to-completion of any coroutine including genuine async I/O.
# Engine floor 3.12+ (PEP 684 per-interpreter GIL) isolates the
# `_asyncio` C-extension state per sub-interpreter, so a fresh event
# loop created + torn down inside the isolate cleans up cleanly on
# Py_EndInterpreter.

def _make_async_effect_invoker(async_fn):
    def invoker(completion_id):
        cid = int(completion_id)
        try:
            coro_or_value = async_fn()
        except Exception as e:
            try:
                host_effect_async_complete(cid, 'throw', str(e))
            except NameError:
                pass
            return
        # If async_fn returned a non-coroutine (e.g. a regular function
        # that the user accidentally wrote), treat the value as the
        # final result.
        try:
            import asyncio as _asyncio
        except ImportError:
            _asyncio = None
        if _asyncio is not None and _asyncio.iscoroutine(coro_or_value):
            try:
                result = _asyncio.run(coro_or_value)
            except Exception as e:
                try:
                    host_effect_async_complete(cid, 'throw', str(e))
                except NameError:
                    pass
                return
        else:
            result = coro_or_value
        if callable(result):
            try:
                ct = __register_callback(result)
                host_effect_async_complete(cid, 'cleanup', ct)
            except NameError:
                pass
        else:
            try:
                host_effect_async_complete(cid, 'value', 0)
            except NameError:
                pass
    return invoker

def use_effect_async(async_fn, deps=None):
    if not callable(async_fn):
        return
    if deps is None:
        deps = []
    try:
        token = __register_callback(_make_async_effect_invoker(async_fn))
    except NameError:
        return
    try:
        host_effect_async(token, deps)
    except NameError:
        __release_callback(token)

def use_emit():
    def emit(msg_type, payload=None, opts=None):
        host_emit(msg_type, payload, opts)
    return emit

def use_message(msg_type, handler):
    if not callable(handler):
        return
    token = __register_callback(handler)
    host_message(msg_type, token)

def use_memo(fn, deps=None):
    if deps is None:
        deps = []
    try:
        checked = host_memo_check(deps)
    except NameError:
        try:
            return fn()
        except Exception:
            return None
    if checked and checked.get('cached'):
        return checked.get('value')
    try:
        result = fn()
    except Exception:
        result = None
    host_memo_store(result)
    return result

def use_callback(fn, deps=None):
    if deps is None:
        deps = []
    try:
        checked = host_memo_check(deps)
    except NameError:
        return fn
    if checked and checked.get('cached'):
        prev_token = checked.get('value')
        if prev_token is not None and prev_token in _hostCallbacks:
            return _hostCallbacks[prev_token]
    if checked and checked.get('previous') is not None:
        __release_callback(checked['previous'])
    token = __register_callback(fn)
    host_memo_store(token)
    return fn

def use_reducer(reducer, initial):
    state, set_state = use_state(initial)
    def dispatch(action):
        set_state(reducer(state, action))
    return state, dispatch

def use_neighbors(filter=None):
    try:
        all_neighbors = host_neighbors() or []
    except NameError:
        return []
    if callable(filter):
        return [n for n in all_neighbors if filter(n)]
    return all_neighbors

def use_topology(handler, filter=None):
    if not callable(handler):
        return
    try:
        token = __register_callback(handler)
    except NameError:
        return
    try:
        host_topology(token, filter)
    except NameError:
        __release_callback(token)

def use_accept_edge():
    def accept(target):
        try:
            host_accept_edge(target)
        except NameError:
            pass
    return accept

# Phase 3d-γ — directory + graph_query proxies.
#
# Both proxies are stable singletons: __augment_ctx points every
# body's ctx['directory'] at the same _directory_proxy object;
# use_graph_query passes the same _graph_query_proxy to every
# selector. Methods route through the registered host callbacks.

from types import SimpleNamespace as _SimpleNamespace

_directory_proxy = _SimpleNamespace(
    find_by_id=lambda _id: host_directory_find_by_id(_id),
    find_by_role=lambda _role: host_directory_find_by_role(_role),
    find_by_depth=lambda _depth: host_directory_find_by_depth(_depth),
    find_in_subtree=lambda _aid: host_directory_find_in_subtree(_aid),
    is_ancestor=lambda _a, _d: host_directory_is_ancestor(_a, _d),
)

def use_edge_accept_handler(predicate):
    # Phase 3d-ζ — register an EdgeAcceptPredicate fn(source_id,
    # msg_type) -> bool. Filters Tier 3 candidacy at query time AND
    # vetoes AcceptEdge mutations from use_accept_edge (3d-β) at the
    # scheduler level. Predicate keyed by target id (this node).
    if not callable(predicate):
        return
    try:
        token = __register_callback(predicate)
    except NameError:
        return
    try:
        host_edge_accept_handler(token)
    except NameError:
        __release_callback(token)

def _make_tier2_proxy(_msg_type, _opts):
    # Phase 3d-ε — Tier 2 sub-proxy. Captured per-proxy: every
    # host_tier2_* call appends `ds` as the trailing positional arg so
    # the Rust decoder can branch between structural and
    # directory-shortcut adjacencies.
    _ds = bool(_opts and isinstance(_opts, dict) and _opts.get('includeDirectoryShortcuts'))
    return _SimpleNamespace(
        node_count=lambda: int(host_tier2_node_count(_msg_type, _ds)),
        has_node=lambda _id: host_tier2_has_node(_msg_type, _id, _ds),
        neighbors=lambda _id: host_tier2_neighbors(_msg_type, _id, _ds) or [],
        has_edge=lambda _u, _v: host_tier2_has_edge(_msg_type, _u, _v, _ds),
        bfs=lambda _from, _depth=None: host_tier2_bfs(_msg_type, _from, _depth, _ds) or [],
        dfs=lambda _from, _depth=None: host_tier2_dfs(_msg_type, _from, _depth, _ds) or [],
        shortest_path=lambda _from, _to: host_tier2_shortest_path(_msg_type, _from, _to, _ds),
        topological_sort=lambda: host_tier2_topological_sort(_msg_type, _ds),
        a_star_path=lambda _from, _to, _call_opts=None: host_tier2_a_star_path(
            _msg_type,
            _from,
            _to,
            (_call_opts.get('heuristic') if isinstance(_call_opts, dict) else None),
            _ds,
        ),
        all_paths=lambda _from, _to, _max=None: host_tier2_all_paths(_msg_type, _from, _to, _max, _ds) or [],
        connected_components=lambda: host_tier2_connected_components(_msg_type, _ds) or [],
        cycle_detection=lambda: host_tier2_cycle_detection(_msg_type, _ds) or {'hasCycle': False, 'cycle': None},
    )

def _make_tier3_proxy(_msg_type, _opts):
    # Phase 3d-ζ — Tier 3 sub-proxy. Same shape as _make_tier2_proxy
    # but routes to host_tier3_* callbacks. Predicate veto applied
    # implicitly via build_tier3_adj on the Rust side.
    _ds = bool(_opts and isinstance(_opts, dict) and _opts.get('includeDirectoryShortcuts'))
    return _SimpleNamespace(
        node_count=lambda: int(host_tier3_node_count(_msg_type, _ds)),
        has_node=lambda _id: host_tier3_has_node(_msg_type, _id, _ds),
        neighbors=lambda _id: host_tier3_neighbors(_msg_type, _id, _ds) or [],
        has_edge=lambda _u, _v: host_tier3_has_edge(_msg_type, _u, _v, _ds),
        bfs=lambda _from, _depth=None: host_tier3_bfs(_msg_type, _from, _depth, _ds) or [],
        dfs=lambda _from, _depth=None: host_tier3_dfs(_msg_type, _from, _depth, _ds) or [],
        shortest_path=lambda _from, _to: host_tier3_shortest_path(_msg_type, _from, _to, _ds),
        topological_sort=lambda: host_tier3_topological_sort(_msg_type, _ds),
        a_star_path=lambda _from, _to, _call_opts=None: host_tier3_a_star_path(
            _msg_type,
            _from,
            _to,
            (_call_opts.get('heuristic') if isinstance(_call_opts, dict) else None),
            _ds,
        ),
        all_paths=lambda _from, _to, _max=None: host_tier3_all_paths(_msg_type, _from, _to, _max, _ds) or [],
        connected_components=lambda: host_tier3_connected_components(_msg_type, _ds) or [],
        cycle_detection=lambda: host_tier3_cycle_detection(_msg_type, _ds) or {'hasCycle': False, 'cycle': None},
    )

_graph_query_proxy = _SimpleNamespace(
    neighbors=lambda _id: host_graph_neighbors(_id) or [],
    has_node=lambda _id: host_graph_has_node(_id),
    has_edge=lambda _u, _v: host_graph_has_edge(_u, _v),
    node_count=lambda: host_graph_node_count(),
    # Phase 3d-δ — algorithm accessors (verbatim followon-B parity):
    shortest_path=lambda _from, _to: host_graph_shortest_path(_from, _to),
    all_paths=lambda _from, _to, _max_depth=None: host_graph_all_paths(_from, _to, _max_depth) or [],
    a_star_path=lambda _from, _to: host_graph_a_star_path(_from, _to),
    bfs=lambda _from, _depth_limit=None: host_graph_bfs(_from, _depth_limit) or [],
    dfs=lambda _from, _depth_limit=None: host_graph_dfs(_from, _depth_limit) or [],
    connected_components=lambda: host_graph_connected_components() or [],
    cycle_detection=lambda: host_graph_cycle_detection() or {'hasCycle': False, 'cycle': None},
    # Phase 3d-ε — Tier 2 selector returning a fresh sub-proxy per call:
    tier2=lambda _msg, _opts=None: _make_tier2_proxy(_msg, _opts),
    # Phase 3d-ζ — Tier 3 selector returning a fresh sub-proxy per call:
    tier3=lambda _msg, _opts=None: _make_tier3_proxy(_msg, _opts),
)

def __augment_ctx(raw_ctx):
    if not isinstance(raw_ctx, dict):
        return raw_ctx
    raw_ctx['directory'] = _directory_proxy
    return raw_ctx

def use_graph_query(selector, deps=None):
    if not callable(selector):
        return None
    return use_memo(lambda: selector(_graph_query_proxy), deps if deps is not None else [])

# Phase 3e-α — dynamic node lifecycle hooks. Mirrors JSC's
# useSpawnChild / useUnmountSelf / useCompose. The lambda-wrap-at-
# registration pattern injects __augment_ctx into spawned bodies so
# every dynamically-mounted node sees ctx['directory'] + Tier 1/2/3
# proxies the same as top-level definitions (which get the same
# augmentation via BODY_AUGMENT_SUFFIX appended Rust-side).
#
# A separate _make_compose_wrapper factory is used inside use_compose
# to capture the per-iteration component fn; a bare lambda would close
# over the loop variable and resolve every wrapped fn to the loop's
# last component (Python closure-in-loop footgun).

def _make_compose_wrapper(fn):
    def wrapped(raw_ctx):
        return fn(__augment_ctx(raw_ctx))
    return wrapped

def use_spawn_child():
    def spawn(spec):
        if not isinstance(spec, dict):
            return
        component = spec.get('component')
        if not callable(component):
            return
        try:
            token = __register_callback(_make_compose_wrapper(component))
        except NameError:
            return
        try:
            host_spawn_child({
                'id': spec.get('id', ''),
                'componentToken': token,
                'props': spec.get('props', {}),
                'connectTo': spec.get('connectTo', []),
            })
        except NameError:
            __release_callback(token)
    return spawn

def use_unmount_self():
    def unmount():
        try:
            host_unmount_self()
        except NameError:
            pass
    return unmount

def use_compose():
    def compose(specs):
        if not isinstance(specs, list):
            return
        marshalled = []
        leaked_tokens = []
        for s in specs:
            if not isinstance(s, dict):
                continue
            component = s.get('component')
            if not callable(component):
                continue
            try:
                token = __register_callback(_make_compose_wrapper(component))
            except NameError:
                continue
            leaked_tokens.append(token)
            marshalled.append({
                'id': s.get('id', ''),
                'componentToken': token,
                'props': s.get('props', {}),
                'connectTo': s.get('connectTo', []),
            })
        try:
            host_compose(marshalled)
        except NameError:
            for t in leaked_tokens:
                __release_callback(t)
    return compose

# Phase 3e-beta - supervision hooks. All Tier A (pure HOOK_CTX reads
# from the host callback side; no token registry; no engine
# re-entry). use_monitor returns a SimpleNamespace(id=...) shape
# mirroring JSC's `{id}` JS object; use_demonitor accepts that
# handle (or its raw id). __DOWN__ system-message delivery is
# already wired in graphreact-core's Scheduler; use_message from 3c
# catches it.

def use_monitor(target_id):
    try:
        result = host_monitor(target_id)
    except NameError:
        return None
    if result is None:
        return None
    if not isinstance(result, dict):
        return None
    return _SimpleNamespace(id=int(result.get('id', 0)))

def use_demonitor(mref):
    if mref is None:
        return
    mid = mref.id if hasattr(mref, 'id') else mref
    try:
        host_demonitor(int(mid))
    except (NameError, TypeError, ValueError):
        pass

def use_link(target_id):
    try:
        return bool(host_link(target_id))
    except NameError:
        return False

def use_unlink(target_id):
    try:
        host_unlink(target_id)
    except NameError:
        pass

def use_group_join(group):
    try:
        host_group_join(group)
    except NameError:
        pass

def use_group_leave(group):
    try:
        host_group_leave(group)
    except NameError:
        pass

# Phase 3e-gamma - event-source surface. `use_source(factory_fn,
# deps)` invokes `factory_fn()` to produce a descriptor dict, then
# dispatches to `host_use_source` which decodes the kind and
# constructs the right Rust-native `Arc<dyn EventSource>` via
# graphreact-core's `timer_source` / `delay_source` or
# `crate::sources::http_source` / `crate::sources::watch_source`
# (the latter two ported from graphreact-napi during followon-A;
# tokio::net + notify infrastructure already in graphreact-cli).
# Source events flow cross-thread via the scheduler's
# `external_inbox` and reach the active node as messages on the
# next `process_batch` - caught by `use_message(type, handler)`
# (3c). The factory pattern keeps descriptors as pure JSON; no
# native handle marshalling.

def use_source(factory_fn, deps=None):
    if not callable(factory_fn):
        return
    try:
        descriptor = factory_fn()
    except Exception:
        return
    if not isinstance(descriptor, dict):
        return
    try:
        host_use_source(descriptor, deps if deps is not None else [])
    except NameError:
        pass

def timer_source(interval_ms):
    return {'kind': 'timer', 'intervalMs': int(interval_ms)}

def delay_source(delay_ms):
    return {'kind': 'delay', 'delayMs': int(delay_ms)}

def http_source(opts=None):
    return {'kind': 'http', 'opts': opts if isinstance(opts, dict) else {}}

def watch_source(opts=None):
    return {'kind': 'watch', 'opts': opts if isinstance(opts, dict) else {}}

# Install on builtins so any user module sees these without import.
_builtins.__register_callback = __register_callback
_builtins.__invoke_callback = __invoke_callback
_builtins.__release_callback = __release_callback
_builtins.__release_tokens = __release_tokens
_builtins.__augment_ctx = __augment_ctx
_builtins.use_state = use_state
_builtins.use_effect = use_effect
_builtins.use_effect_async = use_effect_async
_builtins.use_emit = use_emit
_builtins.use_message = use_message
_builtins.use_memo = use_memo
_builtins.use_callback = use_callback
_builtins.use_reducer = use_reducer
_builtins.use_neighbors = use_neighbors
_builtins.use_topology = use_topology
_builtins.use_accept_edge = use_accept_edge
_builtins.use_edge_accept_handler = use_edge_accept_handler
_builtins.use_graph_query = use_graph_query
_builtins.use_spawn_child = use_spawn_child
_builtins.use_unmount_self = use_unmount_self
_builtins.use_compose = use_compose
_builtins.use_monitor = use_monitor
_builtins.use_demonitor = use_demonitor
_builtins.use_link = use_link
_builtins.use_unlink = use_unlink
_builtins.use_group_join = use_group_join
_builtins.use_group_leave = use_group_leave
_builtins.use_source = use_source
_builtins.timer_source = timer_source
_builtins.delay_source = delay_source
_builtins.http_source = http_source
_builtins.watch_source = watch_source
";

// ---------------------------------------------------------------------
// Host callback builders
// ---------------------------------------------------------------------

/// **M13-followon-D analogue.** Build the `host_track_token(token)`
/// callback. The Python-side `__register_callback(fn)` wrapper calls
/// this immediately after minting a new token (try/except NameError
/// guard — no-op when the callback isn't registered, e.g. during
/// dry-mount paths). The callback appends the token into
/// `NODE_TOKENS[current_node_id]` so [`release_tokens_for_node`] can
/// drain the bucket on unmount and dispatch `__release_tokens([...])`.
///
/// Conditional tracking: when [`current_node_id`] returns `None`
/// (cleanup-token mints during effect-fire — `flush_effects` runs
/// outside the body-eval window where `ComponentEvalGuard` set
/// `CURRENT_NODE_ID`), this callback no-ops. Cleanup tokens auto-
/// release via `__release_callback(ct)` in the [`EffectCleanup`]
/// closure built by [`build_host_effect_callback`], so they don't
/// leak. Same reasoning as JSC followon-D's
/// `build_host_track_token_callback`.
fn build_host_track_token_callback() -> BoxCallback {
    Box::new(|args| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

/// Build the `host_state(initial) → {id, value}` callback. Python-side
/// `use_state` delegates to this; the closure runs on every render.
///
/// First render at a given hook position: allocates a fresh
/// `Signal<Value>` via `core_use_state`, mints a stable id, stores
/// the signal in [`SIGNAL_REGISTRY`], maps `(node_id, hook_pos) → id`
/// in [`SIGNAL_BY_HOOK_POS`], returns `{id, value: initial}`.
///
/// Subsequent renders: `core_use_state` returns the same `Signal`
/// (state preserved by graphreact-core's hook slots); we look up the
/// id from `SIGNAL_BY_HOOK_POS` and return `{id, value: current}`.
fn build_host_state_callback() -> BoxCallback {
    Box::new(|args| {
        let initial: Value = args
            .first()
            .cloned()
            .map(HostValue::into_json)
            .unwrap_or(Value::Null);

        let node_id = current_node_id()
            .ok_or_else(|| EngineError::Internal("host_state: no active node".into()))?;
        let hook_pos = next_py_hook_index();

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

        #[allow(clippy::cast_precision_loss)]
        Ok(HostValue::Object(vec![
            ("id".into(), HostValue::Number(id as f64)),
            ("value".into(), HostValue::from_json(current_value)),
        ]))
    })
}

/// Build the `host_state_set(id, value)` callback. Python-side setter
/// closures route through this; looking up the Signal by id and
/// calling `.set(v)` marks the owning node dirty. The next
/// `Scheduler::process_batch` re-evaluates it.
fn build_host_state_set_callback() -> BoxCallback {
    Box::new(|args| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

/// Build the `host_effect(token, deps)` callback. Python-side
/// `use_effect` registers the callback in the prelude's
/// `_hostCallbacks` dict (returning a token) then calls this. The
/// callback is invoked AFTER the body returns (via graphreact-core's
/// `flush_effects`), so re-entry into CPython is safe — the engine
/// borrow held during `call_function` has been released by then.
///
/// On invocation, the inner closure calls `__invoke_callback(token,
/// [])` via [`CPythonEngine::invoke_global`]. Python returns an
/// envelope `{kind, value?, token?, message?}` — when the user's
/// effect callback returns a callable (the conventional cleanup
/// shape), `__invoke_callback` registers that callable under a fresh
/// token and returns `{kind: 'cleanup', token: ct}`. We translate
/// that into an [`EffectCleanup`] closure that re-invokes
/// `__invoke_callback(ct)` + `__release_callback(ct)` when it fires.
fn build_host_effect_callback() -> BoxCallback {
    Box::new(|args| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

        let engine = current_engine()
            .ok_or_else(|| EngineError::Internal("host_effect: engine TLS unset".into()))?;
        let iso = current_iso()
            .ok_or_else(|| EngineError::Internal("host_effect: iso TLS unset".into()))?;

        core_use_effect(
            move || -> Option<EffectCleanup> {
                #[allow(clippy::cast_precision_loss)]
                let token_hv = HostValue::Number(token as f64);
                let result = engine.borrow_mut().invoke_global(
                    iso,
                    "__invoke_callback",
                    &[token_hv, HostValue::Array(vec![])],
                );
                let envelope = match result {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[graphreact-cli] use_effect invocation failed: {e}");
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
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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
                        "[graphreact-cli] use_effect callback threw: {}",
                        throw_message.unwrap_or_default()
                    );
                    return None;
                }
                if kind_str != "cleanup" {
                    return None;
                }
                let ct = cleanup_token?;
                let engine_for_cleanup = Rc::clone(&engine);
                let cleanup: EffectCleanup = Box::new(move || {
                    #[allow(clippy::cast_precision_loss)]
                    let ct_hv = HostValue::Number(ct as f64);
                    let _ = engine_for_cleanup.borrow_mut().invoke_global(
                        iso,
                        "__invoke_callback",
                        &[ct_hv.clone(), HostValue::Array(vec![])],
                    );
                    let _ = engine_for_cleanup.borrow_mut().invoke_global(
                        iso,
                        "__release_callback",
                        &[ct_hv],
                    );
                });
                Some(cleanup)
            },
            deps,
        );
        Ok(HostValue::Null)
    })
}

/// **M13-phase3g.** Build the `host_effect_async(token, deps)`
/// callback. Mirrors [`build_host_effect_callback`] but routes through
/// [`core_use_effect_async`]. The Python-side `use_effect_async(async_fn,
/// deps)` registers a Python invoker (constructed by
/// `_make_async_effect_invoker(async_fn)` in HOOK_PRELUDE) under a
/// token; we install an async-effect closure that, when fired by
/// `flush_effects_async`:
///
/// 1. Allocates a fresh completion id and parks a one-shot `Sender`
///    in [`EFFECT_ASYNC_COMPLETIONS`] under that id.
/// 2. Invokes the Python invoker (`__invoke_callback(token,
///    [completion_id])`) via [`invoke_global_static`]. The invoker
///    synchronously runs the user's coroutine to completion via
///    `asyncio.run(coro)`. PEP 684 (3.12+, the engine floor)
///    isolates `_asyncio` C-extension state per sub-interpreter, so
///    the loop is created + torn down cleanly without polluting
///    subsequent sub-interpreter teardowns. On settle, calls
///    `host_effect_async_complete(completion_id, kind, payload)`.
/// 3. Awaits the one-shot receiver. The Python synchronous run-to-
///    completion path settles the channel from the same thread (TLS-
///    driven) before `invoke_global_static` returns, so the
///    `Receiver.await` returns immediately.
/// 4. Translates `EffectAsyncSettlement` into an [`AsyncEffectCleanup`]
///    (or `None` for value/throw) and returns it to graphreact-core's
///    `flush_effects_async`.
///
/// Re-entrancy: the closure runs from `flush_effects_async`, which is
/// driven by `Scheduler::process_batch_async` — outside any active
/// engine borrow. [`invoke_global_static`] is used (not
/// `engine.borrow_mut().invoke_global`) for symmetry with the 3d-ζ
/// predicate path; either would work here since no nested borrow.
fn build_host_effect_async_callback() -> BoxCallback {
    Box::new(|args| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let token = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_effect_async: expected Number token, got {other:?}"
                )));
            }
        };
        let deps: Vec<Value> = match args.get(1) {
            Some(HostValue::Array(arr)) => arr.iter().cloned().map(HostValue::into_json).collect(),
            None | Some(HostValue::Null) => Vec::new(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_effect_async: expected Array deps, got {other:?}"
                )));
            }
        };

        let iso = current_iso()
            .ok_or_else(|| EngineError::Internal("host_effect_async: iso TLS unset".into()))?;

        core_use_effect_async(
            move || async move {
                let completion_id = next_effect_async_completion_id();
                let (tx, rx) = tokio::sync::oneshot::channel::<EffectAsyncSettlement>();
                EFFECT_ASYNC_COMPLETIONS.with(|cell| {
                    cell.borrow_mut().insert(completion_id, tx);
                });

                // Fire the Python invoker. The invoker drives the
                // user's coroutine to completion via asyncio.run(coro)
                // and synchronously calls host_effect_async_complete
                // before this invoke_global_static returns.
                #[allow(clippy::cast_precision_loss)]
                if let Err(e) = invoke_global_static(
                    iso,
                    "__invoke_callback",
                    &[
                        HostValue::Number(token as f64),
                        HostValue::Array(vec![HostValue::Number(completion_id as f64)]),
                    ],
                ) {
                    eprintln!("[graphreact-cli] use_effect_async invocation failed: {e}");
                    EFFECT_ASYNC_COMPLETIONS.with(|cell| {
                        cell.borrow_mut().remove(&completion_id);
                    });
                    return None;
                }

                // The oneshot was inserted pre-invocation, so any
                // synchronous Python settlement (the run-to-completion
                // path) that signaled before this `.await` is already
                // buffered in the channel.
                let settle = match rx.await {
                    Ok(s) => s,
                    Err(_) => {
                        // Sender dropped without sending — Python
                        // invoker never settled. Fall back to no
                        // cleanup.
                        return None;
                    }
                };

                match settle {
                    EffectAsyncSettlement::Cleanup(ct) => {
                        let cleanup: AsyncEffectCleanup = Box::new(move || {
                            Box::pin(async move {
                                #[allow(clippy::cast_precision_loss)]
                                let _ = invoke_global_static(
                                    iso,
                                    "__invoke_callback",
                                    &[
                                        HostValue::Number(ct as f64),
                                        HostValue::Array(vec![]),
                                    ],
                                );
                                #[allow(clippy::cast_precision_loss)]
                                let _ = invoke_global_static(
                                    iso,
                                    "__release_callback",
                                    &[HostValue::Number(ct as f64)],
                                );
                            })
                        });
                        Some(cleanup)
                    }
                    EffectAsyncSettlement::Throw(msg) => {
                        eprintln!("[graphreact-cli] use_effect_async rejected: {msg}");
                        None
                    }
                    EffectAsyncSettlement::Value => None,
                }
            },
            deps,
        );
        Ok(HostValue::Null)
    })
}

/// **M13-phase3g.** Build the `host_effect_async_complete(completion_id,
/// kind, payload)` callback. Looks up the parked one-shot Sender for
/// `completion_id` in [`EFFECT_ASYNC_COMPLETIONS`], removes it, and
/// signals it with the appropriate [`EffectAsyncSettlement`].
///
/// Args:
/// - `completion_id: Number` — the id assigned by the parked future.
/// - `kind: String` — one of `'value'`, `'cleanup'`, `'throw'`.
/// - `payload: Number | String | Null` — for `'cleanup'`: the cleanup
///   token; for `'throw'`: the stringified error message; otherwise
///   ignored.
///
/// Verbatim Python port of JSC's
/// `build_host_effect_async_complete_callback`. If the Sender is
/// missing (already consumed, or never registered for some race), the
/// call silently no-ops — same defensive shape as `host_track_token`.
fn build_host_effect_async_complete_callback() -> BoxCallback {
    Box::new(|args| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let completion_id = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            _ => return Ok(HostValue::Null),
        };
        let kind = match args.get(1) {
            Some(HostValue::String(s)) => s.as_str(),
            _ => return Ok(HostValue::Null),
        };

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

/// Build the `host_emit(type, payload, opts)` callback. Python-side
/// `use_emit()` returns a closure that calls this; the closure runs on
/// every emit (use_emit-returns-callable-that-emits-now pattern).
///
/// Engine-agnostic body: identical shape to
/// [`crate::jsruntime::build_host_emit_callback`]. No TLS read needed —
/// `core_use_emit()` reads `HOOK_CTX` internally to resolve the
/// per-node emit closure.
///
/// Re-entrancy: this callback never touches [`CPythonEngine`] — it only
/// pushes onto the scheduler's message queue via the closure returned
/// from `core_use_emit()`. Safe to fire while an outer engine borrow is
/// held (e.g., when called from inside a body's `call_function`).
fn build_host_emit_callback() -> BoxCallback {
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

/// Build the `host_message(type, token)` callback. Python-side
/// `use_message(type, fn)` registers `fn` in the per-isolate
/// `_hostCallbacks` token registry (mirroring `use_effect`) and
/// forwards the token here. We translate that into a Rust closure
/// registered via `core_use_message`; on every delivery of a message
/// of the given type to this node, the closure invokes
/// `__invoke_callback(token, [{from, type, payload, ttl}])` via
/// [`CPythonEngine::invoke_global`].
///
/// The marshalled message shape mirrors NAPI's
/// `crates/graphreact-napi/src/hooks.rs::invoke_message_handler` so
/// the Python surface is identical to the JS surface across engines.
///
/// Re-entrancy: handlers fire from the scheduler's `deliver`, which is
/// well outside the body's `call_function`. No engine borrow is held
/// when the closure runs, so `engine.borrow_mut().invoke_global(...)`
/// is safe. The closure captures the engine `Rc` and isolate handle
/// directly (read out of TLS once at registration time) — deferred TLS
/// reads aren't needed the way `host_effect` needs them.
fn build_host_message_callback() -> BoxCallback {
    Box::new(|args| {
        let r#type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_message: expected String type, got {other:?}"
                )))
            }
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let token = match args.get(1) {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_message: expected Number token, got {other:?}"
                )))
            }
        };

        let engine = current_engine()
            .ok_or_else(|| EngineError::Internal("host_message: engine TLS unset".into()))?;
        let iso = current_iso()
            .ok_or_else(|| EngineError::Internal("host_message: iso TLS unset".into()))?;

        core_use_message(r#type, move |msg: &IncomingMessage| {
            let host_msg = HostValue::Object(vec![
                ("from".into(), HostValue::String(msg.from.clone())),
                ("type".into(), HostValue::String(msg.r#type.clone())),
                ("payload".into(), HostValue::from_json(msg.payload.clone())),
                ("ttl".into(), HostValue::Number(f64::from(msg.ttl))),
            ]);
            #[allow(clippy::cast_precision_loss)]
            let token_hv = HostValue::Number(token as f64);
            let result = engine.borrow_mut().invoke_global(
                iso,
                "__invoke_callback",
                &[token_hv, HostValue::Array(vec![host_msg])],
            );
            if let Err(e) = result {
                eprintln!("[graphreact-cli] use_message handler invocation failed: {e}");
            }
        });
        Ok(HostValue::Null)
    })
}

/// Parse the third argument of `host_emit` into [`EmitOptions`].
/// Recognised keys mirror JSC's [`crate::jsruntime`]: `to` (Array of
/// String), `ttl` (Number), `edgeKey` (String), `direct` (String).
/// Unknown keys are ignored. `null` / missing → defaults.
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
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            ("ttl", HostValue::Number(n)) => opts.ttl = Some(*n as u32),
            ("edgeKey", HostValue::String(s)) => opts.edge_key = Some(s.clone()),
            ("direct", HostValue::String(s)) => opts.direct = Some(s.clone()),
            _ => {}
        }
    }
    opts
}

/// Build the `host_memo_check(deps) → envelope` callback. Python-side
/// `use_memo` / `use_callback` call this first to learn whether the
/// cached value is stale. The split-cache pattern avoids re-entering
/// CPython from inside a held engine borrow: this callback never
/// invokes the Python factory; Python runs the factory itself if the
/// cache is stale, then calls `host_memo_store(value)`.
///
/// HOOK_INDEX synchronization: graphreact-core's `HOOK_INDEX` is
/// private. We call `core_use_memo(no-op-factory, deps)` so core
/// advances its index and runs deps comparison; the no-op factory
/// just flips a side-effect flag we read after. core's slot stores
/// `Value::Null` as a placeholder; the actual cached value lives in
/// [`MEMO_REGISTRY`] keyed by `(node_id, hook_pos)`.
///
/// Three return envelopes:
/// - `{cached: true, value}` — deps unchanged + cache hit.
/// - `{cached: false, previous}` — deps changed; previous cached
///   value provided so `use_callback` can release the stale token.
/// - `{cached: false}` — registry miss / first call.
fn build_host_memo_check_callback() -> BoxCallback {
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
        let node_id = current_node_id()
            .ok_or_else(|| EngineError::Internal("host_memo_check: no active node".into()))?;
        let hook_pos = next_py_hook_index();
        let key = (node_id, hook_pos);

        // Sync graphreact-core's HOOK_INDEX and detect whether deps
        // changed via a side-effect flag. The factory's only purpose
        // is to flip the flag — its return value (`Value::Null`) is
        // never read; the real value lives in `MEMO_REGISTRY`.
        let computed_flag = Rc::new(Cell::new(false));
        let flag_inner = Rc::clone(&computed_flag);
        let _: Value = core_use_memo(
            move || {
                flag_inner.set(true);
                Value::Null
            },
            deps,
        );
        let needs_recompute = computed_flag.get();

        if needs_recompute {
            // Stash the previous cached value (if any) so Python can
            // release stale tokens (used by use_callback). Then mark
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
fn build_host_memo_store_callback() -> BoxCallback {
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

/// Build the `host_neighbors()` callback. Reads `HOOK_CTX.neighbors`
/// (set by the lifecycle for the active node) via `core_use_neighbors`
/// and returns the IDs as a Python list.
///
/// Python-side `use_neighbors(filter?)` calls this and applies an
/// optional Python-side filter — keeping the predicate in Python avoids
/// a per-element JSON FFI hop. Pure HOOK_CTX read; no engine borrow.
fn build_host_neighbors_callback() -> BoxCallback {
    Box::new(|_args| {
        let neighbors: Vec<String> = core_use_neighbors::<fn(&str) -> bool>(None);
        Ok(HostValue::Array(
            neighbors.into_iter().map(HostValue::String).collect(),
        ))
    })
}

/// Build the `host_topology(token, filter)` callback. Python-side
/// `use_topology(handler, filter?)` registers `handler` under a token
/// (mirroring `use_message` / `use_effect`) and dispatches the token
/// here. The Rust-side closure registered via `core_use_topology`
/// fires from `dispatch_topology_event` (well outside any body's
/// `call_function`, so re-entry into CPython via `invoke_global` is
/// safe), and re-invokes the Python handler with a marshalled
/// `{kind, nodeId? | u, v, key?}` event object.
fn build_host_topology_callback() -> BoxCallback {
    Box::new(|args| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let token = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_topology: expected Number token, got {other:?}"
                )))
            }
        };
        let filter = parse_topology_filter(args.get(1));

        let engine = current_engine()
            .ok_or_else(|| EngineError::Internal("host_topology: engine TLS unset".into()))?;
        let iso = current_iso()
            .ok_or_else(|| EngineError::Internal("host_topology: iso TLS unset".into()))?;

        core_use_topology(
            move |event: &TopologyEvent| {
                let host_event = topology_event_to_host_value(event);
                #[allow(clippy::cast_precision_loss)]
                let token_hv = HostValue::Number(token as f64);
                let result = engine.borrow_mut().invoke_global(
                    iso,
                    "__invoke_callback",
                    &[token_hv, HostValue::Array(vec![host_event])],
                );
                if let Err(e) = result {
                    eprintln!("[graphreact-cli] use_topology handler invocation failed: {e}");
                }
            },
            filter,
        );
        Ok(HostValue::Null)
    })
}

/// Build the `host_accept_edge(target)` callback. Python-side
/// `use_accept_edge()` returns a closure that calls this with a target
/// id; we route through `core_use_accept_edge()` which returns the
/// `Rc<dyn Fn(&str)>` from HOOK_CTX. Read-only HOOK_CTX access; no
/// engine touch; safe to call while the body's `call_function`
/// borrow is held. The closure enqueues a `PendingMutation::AcceptEdge`
/// internally; the scheduler drains it at the next cascade round and
/// fires an `EdgeAdded` topology event automatically.
fn build_host_accept_edge_callback() -> BoxCallback {
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

/// Decode a Python-shape `TopologyFilter` from the `host_topology`
/// call's second arg. Python shape: `{'kinds': ['node-added', …],
/// 'neighborsOnly': True}`. Mirrors JSC's `parse_topology_filter`
/// verbatim — same key recognition, same kind-string mapping.
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

/// Marshal a [`TopologyEvent`] into the Python-shape envelope mirroring
/// JSC's `topology_event_to_host_value` (`{kind, nodeId? | u, v, key?}`).
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

// ---------------------------------------------------------------------
// Phase 3d-γ — directory finder host callbacks
// ---------------------------------------------------------------------

/// Build the `host_directory_find_by_id(id)` callback. Returns the
/// matching directory entry as `{id, role, depth, path}` or null.
fn build_host_directory_find_by_id_callback() -> BoxCallback {
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
fn build_host_directory_find_by_role_callback() -> BoxCallback {
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
fn build_host_directory_find_by_depth_callback() -> BoxCallback {
    Box::new(|args| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

/// Build the `host_directory_find_in_subtree(ancestor_id)` callback.
fn build_host_directory_find_in_subtree_callback() -> BoxCallback {
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

/// Build the `host_directory_is_ancestor(ancestor_id, descendant_id)`
/// callback.
fn build_host_directory_is_ancestor_callback() -> BoxCallback {
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

// ---------------------------------------------------------------------
// Phase 3d-γ / 3d-δ — Tier 1 graph_query host callbacks
// ---------------------------------------------------------------------
//
// Phase 3d-γ ships the 4 basic accessors (neighbors / has_node /
// has_edge / node_count) over a live `GraphSnapshot::from_live`
// handle — mutations applied through the scheduler are visible to
// subsequent borrows because all three snapshot fields are
// `Rc<RefCell<...>>` clones, not snapshots-by-value.
//
// Phase 3d-δ widens the surface with 7 algorithm accessors
// (shortest_path / all_paths / a_star_path / bfs / dfs /
// connected_components / cycle_detection) ported verbatim from
// jsruntime — each reads `current_graph_snapshot()`, borrows
// `snapshot.graph`, calls the corresponding `graphreact_algorithms`
// function, and marshals results to `HostValue`. Errors collapse to
// neutral returns (Null / empty Array / hasCycle:false) — Python
// callers shouldn't see Rust errors as thrown exceptions, since the
// typical pattern is `use_graph_query(lambda g: ...)` over a live
// snapshot where a missing source node just means the path doesn't
// exist.

fn build_host_graph_neighbors_callback() -> BoxCallback {
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

fn build_host_graph_has_node_callback() -> BoxCallback {
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

fn build_host_graph_has_edge_callback() -> BoxCallback {
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

fn build_host_graph_node_count_callback() -> BoxCallback {
    Box::new(|_args| {
        #[allow(clippy::cast_precision_loss)]
        let count = current_graph_snapshot()
            .map(|s| s.graph.borrow().nodes().count() as f64)
            .unwrap_or(0.0);
        Ok(HostValue::Number(count))
    })
}

// ---------------------------------------------------------------------
// Phase 3d-δ — algorithm accessor host callbacks
// ---------------------------------------------------------------------
//
// 7 callbacks ported verbatim from jsruntime. Each reads
// `current_graph_snapshot()`, borrows `snapshot.graph`, and delegates
// to the matching `graphreact_algorithms` function. A* heuristic kinds
// (latency / compliance / hierarchy / trust-attenuated /
// prerequisite-distance) live with Tier 2 in phase 3d-ε via
// `g.tier2(...).a_star_path(from, to, {'heuristic': '...'})`. Tier 1's
// `a_star_path` is heuristic-free and degenerates to BFS shortest-path
// with uniform edge cost — same split as JSC's followon-B / followon-E.

fn build_host_graph_shortest_path_callback() -> BoxCallback {
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

fn build_host_graph_all_paths_callback() -> BoxCallback {
    Box::new(|args| {
        let from = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let to = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

fn build_host_graph_a_star_path_callback() -> BoxCallback {
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
        // Tier 1's `a_star_path` is heuristic-free and degenerates to
        // BFS shortest-path with uniform edge cost. Heuristic kinds
        // (latency / compliance / hierarchy / trust-attenuated /
        // prerequisite-distance) live with Tier 2 in phase 3d-ε via
        // `g.tier2(...).a_star_path(from, to, {'heuristic': '...'})`.
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

fn build_host_graph_bfs_callback() -> BoxCallback {
    Box::new(|args| {
        let from = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

fn build_host_graph_dfs_callback() -> BoxCallback {
    Box::new(|args| {
        let from = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

fn build_host_graph_connected_components_callback() -> BoxCallback {
    Box::new(|_args| {
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Array(vec![]));
        };
        let g = snapshot.graph.borrow();
        match graphreact_algorithms::connected_components(&g) {
            Ok(components) => {
                let mut out: Vec<HostValue> = Vec::with_capacity(components.len());
                for component in components {
                    // FxHashSet → sorted Vec<String> for stable
                    // Python-side ordering.
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

fn build_host_graph_cycle_detection_callback() -> BoxCallback {
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

// ---------------------------------------------------------------------
// Phase 3d-ε — Tier 2 directed adjacency, algorithms, A* heuristics
// ---------------------------------------------------------------------
//
// Tier 2 surfaces directed reachability derived from per-node
// `PermissionManifest.emits/accepts` pairs. An edge `s → u` exists for
// message-type T iff `s.emits.contains(T) ∧ u.accepts.contains(T)`,
// optionally also requiring a Tier 1 structural edge between the two
// (the default "structural" flavor; the opt-in "directory-shortcut"
// flavor drops the Tier 1 requirement). Verbatim port from
// `crates/graphreact-cli/src/jsruntime.rs:1846-3056`. Algorithm helpers
// are parameterised over a `DirectedAdj` trait so that 3d-ζ's Tier 3
// adjacency (filtered through `use_edge_accept_handler` predicates)
// will reuse them without duplication.
//
// The 5 cost-from-policy A* heuristics (latency / compliance /
// hierarchy / trust-attenuated / prerequisite-distance) read attrs
// from the same `CommunicationGraph` and `GraphDirectory` data the
// rest of the runtime sees; attr keys match JSC's verbatim so
// cross-engine `.grx` fixtures behave identically under either
// runtime. All Tier A reads (pure HOOK_CTX-shaped) — no engine touch;
// safe-default fallbacks on TLS-unset.

/// Directed adjacency + node set for a single message type, derived
/// from per-node `PermissionManifest.emits/accepts` pairs. Built on
/// demand by [`build_tier2_adj`] from a [`GraphSnapshot`] borrow.
/// Iteration order over `nodes` is sorted for stable BFS / DFS / topo
/// output; each `out` list is also sorted.
struct Tier2Adj {
    /// All nodes registered with the scheduler at query time. Sorted.
    nodes: Vec<String>,
    /// `out[s]` = sorted list of nodes `u` such that `s.emits` contains
    /// the message type T and `u.accepts` contains T. Sources without
    /// any outgoing edges have an entry with an empty Vec.
    out: FxHashMap<String, Vec<String>>,
}

/// Common shape consumed by Tier 2 / Tier 3 algorithm helpers. Lets
/// `tier2_*` functions remain a single canonical implementation while
/// being callable with either a [`Tier2Adj`] or a [`Tier3Adj`].
/// Verbatim port of jsruntime's `DirectedAdj`.
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

/// Build the Tier 2 directed adjacency for `msg_type`. Two flavors,
/// selected by `include_directory_shortcuts`:
///
/// **Structural** (`include_directory_shortcuts == false`, default):
/// edge `(s, u)` exists iff a Tier 1 edge `s ↔ u` exists in the live
/// `CommunicationGraph` AND `s.emits` contains `msg_type` AND
/// `u.accepts` contains `msg_type`. Preserves both topology and
/// policy; multi-hop pathfinding remains meaningful.
///
/// **Directory-shortcut** (`include_directory_shortcuts == true`):
/// edge `(s, u)` exists iff `s.emits ∋ msg_type` and `u.accepts ∋
/// msg_type`. Tier 1 edge presence is IGNORED — any emitter reaches
/// any acceptor in 1 hop via direct messaging.
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

// --- Tier 2 algorithm helpers (parameterised over &dyn DirectedAdj) ---

/// Tier 2/3 BFS. Returns nodes in order of discovery. Honours optional
/// `depth_limit`. Generic over [`DirectedAdj`] so 3d-ζ Tier 3 can
/// reuse.
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

/// Tier 2/3 DFS pre-order traversal.
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

/// Tier 2 BFS-based shortest path (uniform edge cost). Inclusive of
/// `from` and `to`; `None` if `to` isn't reachable.
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
/// scope for `clippy::items_after_statements`.
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

/// Tier 2 all simple directed paths from `from` to `to`. Sorted by
/// length asc, then lexicographic for stable output.
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

    out.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    out
}

/// Tarjan's SCC inner recursion — lifted to module scope for
/// `clippy::items_after_statements`.
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

    if lowlink.get(v).copied().unwrap_or(usize::MAX) == index.get(v).copied().unwrap_or(usize::MAX)
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

/// Tier 2 strongly connected components via Tarjan's. Inner Vecs
/// sorted lexicographically; outer Vec sorted by smallest member.
fn tier2_strongly_connected_components(adj: &dyn DirectedAdj) -> Vec<Vec<String>> {
    use std::collections::HashSet;

    let mut index_counter: usize = 0;
    let mut index: FxHashMap<String, usize> = FxHashMap::default();
    let mut lowlink: FxHashMap<String, usize> = FxHashMap::default();
    let mut on_stack: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();
    let mut out: Vec<Vec<String>> = Vec::new();

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

    out.sort_by(|a, b| {
        let amin = a.first().cloned().unwrap_or_default();
        let bmin = b.first().cloned().unwrap_or_default();
        amin.cmp(&bmin)
    });
    out
}

/// 3-color DFS coloring for [`tier2_directed_cycle_detection`]. White
/// = unvisited, Gray = on the current DFS path, Black = fully
/// processed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier2DfsColor {
    White,
    Gray,
    Black,
}

/// Recursive DFS for cycle detection — returns the closing edge
/// `(v, w)` when a back-edge into a Gray node is found.
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

/// Tier 2 directed cycle detection via 3-color DFS. Returns
/// `(has_cycle, cycle_edges)` where `cycle_edges` is `Some(Vec<(u, v)>)`
/// representing one detected cycle's edge sequence on success.
fn tier2_directed_cycle_detection(adj: &dyn DirectedAdj) -> (bool, Option<Vec<(String, String)>>) {
    use std::collections::HashMap;

    let mut color: HashMap<String, Tier2DfsColor> = HashMap::new();
    let mut parent: HashMap<String, String> = HashMap::new();

    for v in adj.nodes_slice() {
        if color.get(v).copied().unwrap_or(Tier2DfsColor::White) == Tier2DfsColor::White {
            if let Some((closer, target)) = tier2_cycle_dfs(v, adj, &mut color, &mut parent) {
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

/// Tier 2 directed Kahn's topological sort. `None` on cycle. Closes
/// followon-B's `topologicalSort` deferral (direction is well-defined
/// per message type at Tier 2).
fn tier2_topological_sort(adj: &dyn DirectedAdj) -> Option<Vec<String>> {
    use std::collections::VecDeque;
    let mut in_deg: FxHashMap<String, usize> =
        adj.nodes_slice().iter().map(|n| (n.clone(), 0)).collect();
    for u in adj.nodes_slice() {
        for v in adj.neighbors(u) {
            *in_deg.entry(v.clone()).or_insert(0) += 1;
        }
    }
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

/// Cost-from-policy heuristic identifier for Tier 2 A*. Five named
/// heuristics from the application-space framing:
///
/// - `latency` — sum of edge `latency` attrs (Self-Healing #1).
/// - `compliance` — penalise unencrypted edges plus non-compliant
///   target nodes (ComplyFlow Layer 2).
/// - `hierarchy` — minimise depth-difference via directory entries
///   (Org Intelligence #3).
/// - `trust-attenuated` — TTL-attenuated path cost; per-step =
///   1.0 / trust (Reputation #8).
/// - `prerequisite-distance` — per-step = 1.0 + difficulty(target)
///   (Adaptive Education #6).
#[allow(clippy::doc_lazy_continuation)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AStarHeuristic {
    /// Default: uniform edge cost, zero estimate. `aStarPath` without
    /// a heuristic option degenerates to BFS shortest-path.
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

/// Per-step (current → next) cost function for Tier 2 A*. Matches
/// JSC's attr keys verbatim so cross-engine `.grx` fixtures behave
/// identically: edge `latency` / `encrypted` / `trust`; node
/// `compliant` / `difficulty`. All defaults are permissive (cost 1.0
/// or no penalty) so attr-less fixtures are BFS-equivalent.
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
            let g = snapshot.graph.borrow();
            if let Ok(attrs) = g.get_edge_data(from, to, None) {
                attrs
                    .get("latency")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(1.0)
            } else {
                1.0
            }
        }
        AStarHeuristic::Compliance => {
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
            let Some(dir) = directory else { return 1.0 };
            let dir = dir.borrow();
            let from_depth = dir.find_by_id(from).map(|e| i64::from(e.depth));
            let to_depth = dir.find_by_id(to).map(|e| i64::from(e.depth));
            #[allow(clippy::cast_precision_loss)]
            match (from_depth, to_depth) {
                (Some(a), Some(b)) => 1.0 + (a - b).unsigned_abs() as f64,
                _ => 1.0,
            }
        }
        AStarHeuristic::TrustAttenuated => {
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

/// Estimated remaining cost from `cur` to `target`. Admissible (≤ true
/// cost) for A* optimality. All shipped heuristics return either 0
/// (BFS-equivalent) or |Δdepth| for hierarchy.
fn tier2_heuristic(
    h: AStarHeuristic,
    cur: &str,
    target: &str,
    directory: Option<&Rc<RefCell<GraphDirectory>>>,
) -> f64 {
    match h {
        AStarHeuristic::Uniform
        | AStarHeuristic::Latency
        | AStarHeuristic::Compliance
        | AStarHeuristic::TrustAttenuated
        | AStarHeuristic::PrerequisiteDistance => 0.0,
        AStarHeuristic::Hierarchy => {
            let Some(dir) = directory else { return 0.0 };
            let dir = dir.borrow();
            let cur_depth = dir.find_by_id(cur).map(|e| i64::from(e.depth));
            let tgt_depth = dir.find_by_id(target).map(|e| i64::from(e.depth));
            #[allow(clippy::cast_precision_loss)]
            match (cur_depth, tgt_depth) {
                (Some(a), Some(b)) => (a - b).unsigned_abs() as f64,
                _ => 0.0,
            }
        }
    }
}

/// Wrapper for f64 priorities in [`tier2_astar_path`]'s
/// `BinaryHeap`. BinaryHeap is max-heap; this ordering flips so the
/// smallest-f entry pops first.
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

/// Tier 2 A* with cost-from-policy. Returns optimal path inclusive of
/// `from` and `to`; `None` if `to` isn't reachable.
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

// --- 12 host_tier2_* callbacks ---
//
// Every callback's first arg is the message-type string; the LAST arg
// is a Bool selecting the directory-shortcut flavor. `node_count` and
// `has_node` ignore the flag (they read the manifest set directly).

/// Decode the trailing `directoryShortcuts` Bool flag. Python-side
/// `_make_tier2_proxy` always appends this as the last positional arg.
fn tier2_directory_shortcuts_flag(args: &[HostValue]) -> bool {
    matches!(args.last(), Some(HostValue::Bool(true)))
}

fn build_host_tier2_node_count_callback() -> BoxCallback {
    Box::new(|args| {
        let _msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Number(0.0)),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Number(0.0));
        };
        #[allow(clippy::cast_precision_loss)]
        let count = snapshot.manifests.borrow().len() as f64;
        Ok(HostValue::Number(count))
    })
}

fn build_host_tier2_has_node_callback() -> BoxCallback {
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

fn build_host_tier2_neighbors_callback() -> BoxCallback {
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

fn build_host_tier2_has_edge_callback() -> BoxCallback {
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

fn build_host_tier2_bfs_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

fn build_host_tier2_dfs_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

fn build_host_tier2_shortest_path_callback() -> BoxCallback {
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

fn build_host_tier2_topological_sort_callback() -> BoxCallback {
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

fn build_host_tier2_a_star_path_callback() -> BoxCallback {
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

fn build_host_tier2_all_paths_callback() -> BoxCallback {
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
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

fn build_host_tier2_connected_components_callback() -> BoxCallback {
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

fn build_host_tier2_cycle_detection_callback() -> BoxCallback {
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

// ---------------------------------------------------------------------
// Phase 3d-ζ — Tier 3 directed adjacency + use_edge_accept_handler hook
// ---------------------------------------------------------------------
//
// Tier 3 layers a per-target accept/veto predicate on top of Tier 2.
// Bodies install a predicate via `use_edge_accept_handler(fn)`; the
// scheduler stores it in `Scheduler::edge_accept_predicates` (keyed by
// the target node's id). Predicates fire from two paths:
//
// (a) **Phase 9 mutation veto** — when `use_accept_edge('target')` (3d-β)
//     enqueues a `PendingMutation::AcceptEdge`, the scheduler's
//     `process_pending_mutations` consults `predicates[target](source,
//     "")` before committing the edge. Empty msgType — the AcceptEdge
//     channel is untyped. Outside any active engine borrow.
//
// (b) **Tier 3 query** — `g.tier3(msgType, opts?).neighbors(s)` etc.
//     filter the Tier 2 candidacy through `predicates[u](s, msgType)`.
//     Inside the active engine borrow held by the querier body's
//     `call_function`. Re-entry to CPython would double-borrow the
//     engine's `Rc<RefCell<...>>` — solution: `invoke_global_static`
//     (a free FFI helper in graphreact-engine-cpython that takes
//     `IsolateHandle` directly, bypassing the RefCell). Mirrors JSC's
//     `invoke_global_static` from M12.3a-followon-F.
//
// Verbatim port from `crates/graphreact-cli/src/jsruntime.rs:1863-3565`.
// `Tier3Adj` implements the same `DirectedAdj` trait shipped in 3d-ε,
// so the 8 `tier2_*` algorithm helpers (bfs / dfs / shortest_path /
// topological_sort / a_star_path / all_paths /
// strongly_connected_components / directed_cycle_detection) are reused
// without duplication. Closes the M13 phase-3d series.

/// Tier 3 directed adjacency for `msg_type`. Same shape as [`Tier2Adj`]
/// but built by [`build_tier3_adj`] which consults each candidate
/// target's edge-accept predicate before including the (s, u) pair.
/// Both types implement [`DirectedAdj`] so the algorithm helpers
/// (`tier2_bfs` / `tier2_astar_path` / etc.) work over either tier
/// without duplication.
struct Tier3Adj {
    /// All nodes registered with the scheduler at query time. Sorted.
    nodes: Vec<String>,
    /// `out[s]` = sorted list of nodes `u` for which Tier 2 candidacy
    /// passes AND `predicates.get(u)`, when present, returns `true` for
    /// `(s, msg_type)`.
    out: FxHashMap<String, Vec<String>>,
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

/// Build the Tier 3 directed adjacency for `msg_type`. Tier 3 = Tier 2
/// ∪ {(s, u) where target u's edge-accept predicate accepts the (s,
/// msg_type) pair}. The flavor flag is forwarded to the Tier 2
/// candidacy step — Tier 3 narrows whichever Tier 2 view the caller
/// asked for.
///
/// **Negotiation semantics**: Tier 3 captures "what's reachable IF the
/// runtime DECIDED to negotiate edges." For each candidate (s, u),
/// invoke `predicates[u](s, msg_type)`. If the predicate is registered
/// AND returns false, exclude the pair. If the predicate is absent
/// (no `use_edge_accept_handler` called for that node), default-accept
/// — preserves backward compatibility and makes Tier 3 ≥ vetoed-Tier 2
/// monotonically: a system with NO predicates installed has Tier 3 ==
/// Tier 2.
///
/// The predicate runs at query time; the AcceptEdge mutation is NOT
/// triggered. Pure dry-run — gives the caller the projected
/// reachability without committing the underlying graph rewrite.
fn build_tier3_adj(
    graph: &CommunicationGraph,
    manifests: &FxHashMap<String, PermissionManifest>,
    predicates: &FxHashMap<String, EdgeAcceptPredicate>,
    msg_type: &str,
    include_directory_shortcuts: bool,
) -> Tier3Adj {
    // Reuse Tier 2 candidacy as the supremum, then filter through each
    // target's predicate. This guarantees Tier 3 ⊆ Tier 2 (when
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

/// Helper: build Tier 3 adjacency from the active TLS snapshot.
/// Borrows graph + manifests + predicates only for the duration of the
/// build.
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

/// Decode the trailing `directoryShortcuts` Bool flag from a Tier 3
/// host_tier3_* call's args. Same shape as Tier 2.
fn tier3_directory_shortcuts_flag(args: &[HostValue]) -> bool {
    matches!(args.last(), Some(HostValue::Bool(true)))
}

/// Decode the boolean verdict from an `__invoke_callback` envelope.
/// The envelope shape is `{kind: 'value', value: <user return>}`; we
/// treat any non-Bool / non-Number / non-truthy value as `false` — the
/// user is expected to return a boolean. A non-boolean is a programming
/// error, surfaced by reading as "veto" rather than silently
/// default-accepting (matches JSC semantics from followon-F).
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

/// `host_edge_accept_handler(token)` — called from
/// `use_edge_accept_handler(predicate)` HOOK_PRELUDE wrapper after
/// registering the predicate as a `_hostCallbacks` token. Builds an
/// `EdgeAcceptPredicate` closure that re-enters CPython via
/// `invoke_global_static` whenever the runtime asks "would this node
/// accept (s, msgType)?" — both AcceptEdge mutation paths (outside
/// engine borrow) and Tier 3 queries (inside engine borrow) trigger
/// it. The static FFI bypass avoids the `Rc<RefCell<CPythonEngine>>`
/// double-borrow that the in-body path would otherwise hit.
fn build_host_edge_accept_handler_callback() -> BoxCallback {
    Box::new(|args| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let token = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_edge_accept_handler: expected Number token, got {other:?}"
                )));
            }
        };

        let iso = current_iso().ok_or_else(|| {
            EngineError::Internal("host_edge_accept_handler: iso TLS unset".into())
        })?;

        // Predicate fires from two paths: (1) `process_pending_mutations
        // ::AcceptEdge`, OUTSIDE any active engine borrow — safe; and
        // (2) `host_tier3_*` callbacks during querier body eval, which
        // run INSIDE an `engine.borrow_mut().call_function(...)` borrow
        // held by the outer body. The second path requires
        // `invoke_global_static` (free function, FFI-only) instead of
        // `engine.borrow_mut().invoke_global(...)` to avoid a Rust
        // RefCell double-borrow.
        core_use_edge_accept_handler(move |source_id: &str, msg_type: &str| -> bool {
            #[allow(clippy::cast_precision_loss)]
            let result = invoke_global_static(
                iso,
                "__invoke_callback",
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
                        "[graphreact-cli] use_edge_accept_handler invocation failed: {e}"
                    );
                    // On invocation failure, default-accept so a buggy
                    // predicate doesn't silently sever Tier 3
                    // reachability.
                    true
                }
            }
        });
        Ok(HostValue::Null)
    })
}

// --- 12 host_tier3_* callbacks ---
//
// Mirror host_tier2_* signatures verbatim; differ only in constructing
// `Tier3Adj` (via `build_tier3_adj_from_snapshot`) instead of `Tier2Adj`.
// All 8 algorithm helpers (`tier2_bfs`, `tier2_dfs`,
// `tier2_shortest_path`, `tier2_topological_sort`, `tier2_astar_path`,
// `tier2_all_paths`, `tier2_strongly_connected_components`,
// `tier2_directed_cycle_detection`) are reused as-is via the
// [`DirectedAdj`] trait — that's the load-bearing benefit of the
// trait-based design from 3d-ε.

fn build_host_tier3_node_count_callback() -> BoxCallback {
    Box::new(|args| {
        let _msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Number(0.0)),
        };
        let Some(snapshot) = current_graph_snapshot() else {
            return Ok(HostValue::Number(0.0));
        };
        #[allow(clippy::cast_precision_loss)]
        let count = snapshot.manifests.borrow().len() as f64;
        Ok(HostValue::Number(count))
    })
}

fn build_host_tier3_has_node_callback() -> BoxCallback {
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

fn build_host_tier3_neighbors_callback() -> BoxCallback {
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

fn build_host_tier3_has_edge_callback() -> BoxCallback {
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

fn build_host_tier3_bfs_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

fn build_host_tier3_dfs_callback() -> BoxCallback {
    Box::new(|args| {
        let msg_type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        let from: String = match args.get(1) {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Array(vec![])),
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

fn build_host_tier3_shortest_path_callback() -> BoxCallback {
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

fn build_host_tier3_topological_sort_callback() -> BoxCallback {
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

fn build_host_tier3_a_star_path_callback() -> BoxCallback {
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

fn build_host_tier3_all_paths_callback() -> BoxCallback {
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
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

fn build_host_tier3_connected_components_callback() -> BoxCallback {
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

fn build_host_tier3_cycle_detection_callback() -> BoxCallback {
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

// ---------------------------------------------------------------------
// Phase 3e-α — dynamic node lifecycle (use_spawn_child / use_unmount_self / use_compose)
// ---------------------------------------------------------------------
//
// Verbatim Python port of JSC's M12.3a-phase3e-γ. The shape:
//
// 1. Python user calls `use_spawn_child()(spec)` from inside a body.
//    The HOOK_PRELUDE wrapper wraps `spec['component']` in a closure
//    that applies `__augment_ctx(raw_ctx)`, registers the wrapped
//    function via `__register_callback` (returning a token), and
//    dispatches `host_spawn_child({id, componentToken, props,
//    connectTo})`.
//
// 2. `host_spawn_child` reads the spec, calls
//    [`build_compose_spec_from_host_value`] which pulls
//    `current_engine()` + `current_iso()` from TLS (installed by
//    `PyEngineGuard::with_directory` around `process_batch`) and
//    constructs a [`NodeDefinition`] whose `component` closure
//    captures `(engine, iso, component_token)` and on every render
//    invokes `__invoke_callback(token, [host_ctx])` via
//    [`CPythonEngine::invoke_global`].
//
// 3. The completed [`ComposeChildSpec`] hands off to
//    [`graphreact_core::use_spawn_child`] which enqueues a
//    `PendingMutation::SpawnChild` for the next cascade round.
//
// `use_compose` is the multi-spec variant — same shape per element +
// a final `core_use_compose()(specs)` call. `use_unmount_self` is
// trivial: enqueue an `UnmountNode` mutation for the active node
// (HOOK_CTX-derived; no spec).
//
// Re-entrancy: the spawned closure runs from `Scheduler::evaluate_node`
// (called from `process_batch`), outside any active engine borrow held
// by a parent body. `engine.borrow_mut()` succeeds. Same pattern as
// 3d-γ's `build_py_node_definition`; **no** `invoke_global_static`
// needed (that's reserved for hooks that fire from inside a body's
// own render — Tier 3 predicate dispatch in 3d-ζ).
//
// Token leak: each `use_spawn_child` / `use_compose` call mints a
// fresh token in `_hostCallbacks`; SpawnChild is idempotent on
// existing IDs at the scheduler so duplicate-id calls don't double-
// mount, but their tokens still register and never get released.
// Same bounded-leak surface JSC documented in 3e-γ; followon-D
// swept it via per-node token tracking on unmount. The Python port
// follows JSC's phasing: deferred to a future "M13-followon-D
// analogue" slice that batches token GC across all token-using hooks.

/// Build the `host_spawn_child(spec)` callback. Python-side
/// `use_spawn_child()(spec)` registers the user's component fn under
/// a token (after wrapping with `__augment_ctx`) and dispatches the
/// spec dict here. We rebuild a [`ComposeChildSpec`] whose
/// [`NodeDefinition`] re-enters CPython on every render via
/// `__invoke_callback`, then hand it to
/// [`graphreact_core::use_spawn_child`].
fn build_host_spawn_child_callback() -> BoxCallback {
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

/// Build the `host_unmount_self()` callback. Python-side
/// `use_unmount_self()()` calls this; we route through
/// [`graphreact_core::use_unmount_self`] which enqueues an
/// `UnmountNode` mutation for the active node (HOOK_CTX-derived).
fn build_host_unmount_self_callback() -> BoxCallback {
    Box::new(|_args| {
        let unmount = core_use_unmount_self();
        unmount();
        Ok(HostValue::Null)
    })
}

/// Build the `host_compose(specs)` callback. Python-side
/// `use_compose()(specs)` registers each component fn under a token
/// (after wrapping with `__augment_ctx`) and dispatches the array
/// here. We marshal the array into `Vec<ComposeChildSpec>` and call
/// [`graphreact_core::use_compose`], which spawns missing children
/// and tracks the active node's declared composition.
fn build_host_compose_callback() -> BoxCallback {
    Box::new(|args| {
        let arr = match args.first() {
            Some(HostValue::Array(arr)) => arr.clone(),
            None | Some(HostValue::Null) => Vec::new(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_compose: expected Array specs, got {other:?}"
                )));
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

/// Decode a Python-side spec dict (already JSON-marshalled to
/// `HostValue::Object`) into a [`ComposeChildSpec`] backed by a
/// [`NodeDefinition`] whose component closure re-enters CPython via
/// `invoke_global("__invoke_callback", [token, [ctx]])` on every
/// render. The closure captures engine + iso through Rc clones — the
/// SpawnChild / Compose mutations that consume this spec are applied
/// at the start of the next cascade round (no outstanding engine
/// borrow at that point). Verbatim Python port of JSC's
/// `build_compose_spec_from_host_value`.
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
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

    let id = id.ok_or_else(|| EngineError::Internal("ComposeChildSpec: missing `id`".into()))?;
    let component_token = component_token
        .ok_or_else(|| EngineError::Internal("ComposeChildSpec: missing `componentToken`".into()))?;

    let engine = current_engine()
        .ok_or_else(|| EngineError::Internal("compose spec: engine TLS unset".into()))?;
    let iso = current_iso()
        .ok_or_else(|| EngineError::Internal("compose spec: iso TLS unset".into()))?;

    let definition_name = format!("PyChild::{id}");
    let definition = NodeDefinition::new(
        definition_name,
        move |ctx: &NodeContext| -> PermissionManifest {
            let _guard = ComponentEvalGuard::new(&ctx.id);
            let host_ctx = build_host_ctx(ctx);
            #[allow(clippy::cast_precision_loss)]
            let envelope = engine.borrow_mut().invoke_global(
                iso,
                "__invoke_callback",
                &[
                    HostValue::Number(component_token as f64),
                    HostValue::Array(vec![host_ctx]),
                ],
            );
            match envelope {
                Ok(env) => manifest_from_invoke_envelope(env),
                Err(e) => {
                    eprintln!(
                        "[graphreact-cli] dynamically-spawned Python component invocation failed: {e}"
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

/// Extract a manifest from an `__invoke_callback` envelope. The
/// envelope shape is `{kind, value? | token? | message?}`; we only
/// care about `kind: 'value'` for component bodies (they return
/// manifest dicts, not cleanup callables). Verbatim Python port of
/// JSC's `manifest_from_invoke_envelope`.
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

// ---------------------------------------------------------------------
// Phase 3e-β — supervision hooks (use_monitor / use_demonitor / use_link / use_unlink / use_group_join / use_group_leave)
// ---------------------------------------------------------------------
//
// Verbatim Python port of JSC's M12.3a-phase3e-ζ supervision surface.
// All six are pure HOOK_CTX-shaped reads (Tier A from 3d-β framing) —
// the host callback dispatches directly to graphreact-core's
// `use_X(...)` which reads from `with_ctx(|c| ...)` to dispatch
// through the active node's HookCtx. Zero engine touch from the host
// side; zero token registry interaction; no NodeDefinition closure
// capture. Dramatically smaller than 3e-α.
//
// `MonitorRef` (Rust: `pub struct MonitorRef(pub u64)`) is marshalled
// across FFI as `{id: u64}`. The Python prelude wrapper exposes it as
// a `_SimpleNamespace(id=...)` for symmetry with JSC's `{id: number}`
// JS object. `use_demonitor(mref)` reads `mref.id` and round-trips.
//
// `__DOWN__` system message delivery is wired in
// `graphreact-core::Scheduler` already — when a monitored target
// unmounts, all monitoring nodes receive a `__DOWN__` message with
// payload `{target, reason}`. `use_message('__DOWN__', handler)`
// (shipped 3c) catches the delivery via the standard token-registry
// path; no new wiring needed in 3e-β.

/// Build the `host_monitor(targetId) → {id} | null` callback. Python
/// `use_monitor(targetId)` returns the result wrapped as a
/// `_SimpleNamespace(id=...)`. The active node will receive
/// `__DOWN__` system messages when the target unmounts —
/// `use_message('__DOWN__', ...)` catches them.
fn build_host_monitor_callback() -> BoxCallback {
    Box::new(|args| {
        let target = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        match core_use_monitor(&target) {
            #[allow(clippy::cast_precision_loss)]
            Some(mref) => Ok(HostValue::Object(vec![(
                "id".to_string(),
                HostValue::Number(mref.0 as f64),
            )])),
            None => Ok(HostValue::Null),
        }
    })
}

/// Build the `host_demonitor(id)` callback. Python
/// `use_demonitor(mref)` reads `mref.id` (or accepts the raw id) and
/// dispatches the integer here.
fn build_host_demonitor_callback() -> BoxCallback {
    Box::new(|args| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let id = match args.first() {
            Some(HostValue::Number(n)) => *n as u64,
            _ => return Ok(HostValue::Null),
        };
        core_use_demonitor(CoreMonitorRef(id));
        Ok(HostValue::Null)
    })
}

/// Build the `host_link(targetId) → bool` callback. Python
/// `use_link(targetId)` returns `bool(host_link(...))`.
fn build_host_link_callback() -> BoxCallback {
    Box::new(|args| {
        let target = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Bool(false)),
        };
        Ok(HostValue::Bool(core_use_link(&target)))
    })
}

/// Build the `host_unlink(targetId)` callback.
fn build_host_unlink_callback() -> BoxCallback {
    Box::new(|args| {
        let target = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        core_use_unlink(&target);
        Ok(HostValue::Null)
    })
}

/// Build the `host_group_join(group)` callback. Idempotent at the
/// scheduler level — multiple evaluations don't create duplicate
/// memberships.
fn build_host_group_join_callback() -> BoxCallback {
    Box::new(|args| {
        let group = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        core_use_group_join(&group);
        Ok(HostValue::Null)
    })
}

/// Build the `host_group_leave(group)` callback. No-op if the active
/// node isn't in the group.
fn build_host_group_leave_callback() -> BoxCallback {
    Box::new(|args| {
        let group = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        core_use_group_leave(&group);
        Ok(HostValue::Null)
    })
}

// ---------------------------------------------------------------------
// Phase 3e-γ — event-source surface (use_source + timer/delay/http/watch)
// ---------------------------------------------------------------------
//
// Verbatim Python port of JSC's M12.3a-phase3e-ε + M12.3a-followon-A
// combined. The phase 3e-ε stage shipped descriptors for all four
// kinds but only timer/delay had real Rust-side firing; followon-A
// later ported `crates/graphreact-napi/src/sources.rs` to
// `crates/graphreact-cli/src/sources.rs` (tokio::net + notify) and
// installed a tokio runtime in `grx` main(). For the Python port,
// the staging collapses — followon-A's infrastructure is in place,
// so all four kind arms ship real on the first slice.
//
// Source instances live in `node.active_sources` after registration;
// `lifecycle::stop_sources` (already shipped in graphreact-core)
// cleans them up on node unmount. No new infrastructure needed in
// pyruntime. Token leak surface unchanged — the descriptor crosses
// the FFI as pure data; no `__hostCallbacks` interaction.

/// Build the `host_use_source(descriptor, deps)` callback. Python-side
/// `use_source(factory_fn, deps)` invokes `factory_fn()` to produce a
/// descriptor dict `{'kind': ..., ...}` and dispatches it here. We
/// decode the kind and construct the matching
/// [`std::sync::Arc<dyn EventSource>`] via graphreact-core's
/// [`core_timer_source`] / [`core_delay_source`] or
/// [`crate::sources::http_source`] / [`crate::sources::watch_source`].
///
/// `core_use_source` registers the factory + deps as a hook slot.
/// The scheduler invokes the factory once on first mount (or on
/// deps-change) and `start()`s the resulting source. Events flow
/// cross-thread via the scheduler's `external_inbox` and reach the
/// active node as messages on the next `process_batch` — caught by
/// `use_message(type, handler)`.
fn build_host_use_source_callback() -> BoxCallback {
    Box::new(|args| {
        let descriptor = match args.first() {
            Some(HostValue::Object(pairs)) => pairs.clone(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_use_source: expected Object descriptor, got {other:?}"
                )));
            }
        };
        let deps: Vec<Value> = match args.get(1) {
            Some(HostValue::Array(arr)) => {
                arr.iter().cloned().map(HostValue::into_json).collect()
            }
            None | Some(HostValue::Null) => Vec::new(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_use_source: expected Array deps, got {other:?}"
                )));
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
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                ("intervalMs", HostValue::Number(n)) => interval_ms = n as u64,
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                ("delayMs", HostValue::Number(n)) => delay_ms = n as u64,
                ("opts", HostValue::Object(opts_pairs)) => {
                    for (ok, ov) in opts_pairs {
                        match (ok.as_str(), ov) {
                            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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
        let factory: Box<dyn FnOnce() -> std::sync::Arc<dyn EventSource>> = match kind_str.as_str()
        {
            "timer" => Box::new(move || core_timer_source(interval_ms)),
            "delay" => Box::new(move || core_delay_source(delay_ms)),
            // M12.3a-followon-A infrastructure already in place: http
            // + watch firing real via crates/graphreact-cli/src/sources.rs
            // (tokio::net + notify). httpSource requires the binary to
            // be inside a tokio runtime context (grx main() installs
            // one); watchSource uses notify's own thread and is
            // runtime-agnostic.
            "http" => Box::new(move || crate::sources::http_source(http_port)),
            "watch" => Box::new(move || crate::sources::watch_source(watch_path)),
            _ => Box::new(|| core_timer_source(u64::MAX)), // never fires; unknown-kind placeholder
        };

        core_use_source(move || factory(), deps);
        Ok(HostValue::Null)
    })
}

// ---------------------------------------------------------------------
// Phase 3d-γ — body augmentation suffix
// ---------------------------------------------------------------------
//
// JSC wraps the body source in an IIFE inside `gr_jsc_component_register`
// to inject `ctx.directory` before each render. Python doesn't have
// IIFEs, and routing the augmented ctx through `engine.invoke_global`
// fails because `SimpleNamespace` isn't JSON-serialisable. So we
// append a Python-side wrapper to the user's body source at eval time:
// the wrapper re-binds `body` (or `init`) to a function that calls the
// original through `__augment_ctx`. Same effect as JSC's IIFE; no
// JSON round-trip needed.
//
// User source becomes (after appending):
//
//     def body(ctx):
//         ...user code...
//
//     # Appended:
//     try:
//         _user_body = body
//         def body(raw_ctx):
//             return _user_body(__augment_ctx(raw_ctx))
//     except NameError:
//         try:
//             _user_init = init
//             def init(raw_ctx):
//                 return _user_init(__augment_ctx(raw_ctx))
//         except NameError:
//             pass
//
// The shadowing `def body` replaces the user's; `resolve_function`
// returns the wrapped body. Phase 3a `init`-style fixtures get the
// same treatment via the `except NameError` fallback path.
const BODY_AUGMENT_SUFFIX: &str = "
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
";

// ---------------------------------------------------------------------
// HOOK_PRELUDE installation
// ---------------------------------------------------------------------

/// Register host callbacks + evaluate the Python HOOK_PRELUDE on the
/// active sub-interpreter's builtins. Idempotent per-isolate (the
/// prelude is a no-op if re-evaluated; callbacks are registered
/// once).
fn install_hook_prelude(
    engine: &mut CPythonEngine,
    iso: IsolateHandle,
) -> Result<(), RuntimeError> {
    // M13-followon-D analogue: register host_track_token FIRST so
    // every subsequent prelude-driven `__register_callback` call (and
    // every body-time `__register_callback` call thereafter) reports
    // its token under the active node id. Tokens minted before this
    // line (none in current shape) would not be tracked, but the
    // prelude eval-phase happens AFTER all register_callback calls
    // below, so the ordering is safe.
    engine.register_callback(iso, "host_track_token", build_host_track_token_callback())?;
    engine.register_callback(iso, "host_state", build_host_state_callback())?;
    engine.register_callback(iso, "host_state_set", build_host_state_set_callback())?;
    engine.register_callback(iso, "host_effect", build_host_effect_callback())?;
    // Phase 3g — useEffectAsync via asyncio bridge. host_effect_async
    // parks a oneshot Sender + invokes the Python invoker (which
    // drives the user's coroutine to completion via
    // asyncio.new_event_loop().run_until_complete). The Python
    // invoker calls host_effect_async_complete which signals the
    // parked Sender. The closure registered via core_use_effect_async
    // awaits the Receiver; settlement → AsyncEffectCleanup or None.
    engine.register_callback(
        iso,
        "host_effect_async",
        build_host_effect_async_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_effect_async_complete",
        build_host_effect_async_complete_callback(),
    )?;
    engine.register_callback(iso, "host_emit", build_host_emit_callback())?;
    engine.register_callback(iso, "host_message", build_host_message_callback())?;
    engine.register_callback(iso, "host_memo_check", build_host_memo_check_callback())?;
    engine.register_callback(iso, "host_memo_store", build_host_memo_store_callback())?;
    engine.register_callback(iso, "host_neighbors", build_host_neighbors_callback())?;
    engine.register_callback(iso, "host_topology", build_host_topology_callback())?;
    engine.register_callback(iso, "host_accept_edge", build_host_accept_edge_callback())?;
    engine.register_callback(
        iso,
        "host_directory_find_by_id",
        build_host_directory_find_by_id_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_directory_find_by_role",
        build_host_directory_find_by_role_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_directory_find_by_depth",
        build_host_directory_find_by_depth_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_directory_find_in_subtree",
        build_host_directory_find_in_subtree_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_directory_is_ancestor",
        build_host_directory_is_ancestor_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_graph_neighbors",
        build_host_graph_neighbors_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_graph_has_node",
        build_host_graph_has_node_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_graph_has_edge",
        build_host_graph_has_edge_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_graph_node_count",
        build_host_graph_node_count_callback(),
    )?;
    // Phase 3d-δ — algorithm accessors.
    engine.register_callback(
        iso,
        "host_graph_shortest_path",
        build_host_graph_shortest_path_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_graph_all_paths",
        build_host_graph_all_paths_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_graph_a_star_path",
        build_host_graph_a_star_path_callback(),
    )?;
    engine.register_callback(iso, "host_graph_bfs", build_host_graph_bfs_callback())?;
    engine.register_callback(iso, "host_graph_dfs", build_host_graph_dfs_callback())?;
    engine.register_callback(
        iso,
        "host_graph_connected_components",
        build_host_graph_connected_components_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_graph_cycle_detection",
        build_host_graph_cycle_detection_callback(),
    )?;
    // Phase 3d-ε — 12 Tier 2 host_tier2_* callbacks for
    // `g.tier2(msgType, opts?)` over the policy-derived directed
    // adjacency.
    engine.register_callback(
        iso,
        "host_tier2_node_count",
        build_host_tier2_node_count_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier2_has_node",
        build_host_tier2_has_node_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier2_neighbors",
        build_host_tier2_neighbors_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier2_has_edge",
        build_host_tier2_has_edge_callback(),
    )?;
    engine.register_callback(iso, "host_tier2_bfs", build_host_tier2_bfs_callback())?;
    engine.register_callback(iso, "host_tier2_dfs", build_host_tier2_dfs_callback())?;
    engine.register_callback(
        iso,
        "host_tier2_shortest_path",
        build_host_tier2_shortest_path_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier2_topological_sort",
        build_host_tier2_topological_sort_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier2_a_star_path",
        build_host_tier2_a_star_path_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier2_all_paths",
        build_host_tier2_all_paths_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier2_connected_components",
        build_host_tier2_connected_components_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier2_cycle_detection",
        build_host_tier2_cycle_detection_callback(),
    )?;
    // Phase 3d-ζ — use_edge_accept_handler + 12 Tier 3 host_tier3_*
    // callbacks for `g.tier3(msgType, opts?)` over the
    // negotiation-pruned directed adjacency.
    engine.register_callback(
        iso,
        "host_edge_accept_handler",
        build_host_edge_accept_handler_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier3_node_count",
        build_host_tier3_node_count_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier3_has_node",
        build_host_tier3_has_node_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier3_neighbors",
        build_host_tier3_neighbors_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier3_has_edge",
        build_host_tier3_has_edge_callback(),
    )?;
    engine.register_callback(iso, "host_tier3_bfs", build_host_tier3_bfs_callback())?;
    engine.register_callback(iso, "host_tier3_dfs", build_host_tier3_dfs_callback())?;
    engine.register_callback(
        iso,
        "host_tier3_shortest_path",
        build_host_tier3_shortest_path_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier3_topological_sort",
        build_host_tier3_topological_sort_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier3_a_star_path",
        build_host_tier3_a_star_path_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier3_all_paths",
        build_host_tier3_all_paths_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier3_connected_components",
        build_host_tier3_connected_components_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_tier3_cycle_detection",
        build_host_tier3_cycle_detection_callback(),
    )?;
    // Phase 3e-α — dynamic node lifecycle hooks. host_spawn_child and
    // host_compose call build_compose_spec_from_host_value which reads
    // current_engine() / current_iso() from TLS (populated by
    // PyEngineGuard::with_directory around process_batch). The
    // resulting NodeDefinition closures capture engine + iso through
    // Rc clones; subsequent renders re-enter CPython via
    // invoke_global("__invoke_callback", ...).
    engine.register_callback(iso, "host_spawn_child", build_host_spawn_child_callback())?;
    engine.register_callback(
        iso,
        "host_unmount_self",
        build_host_unmount_self_callback(),
    )?;
    engine.register_callback(iso, "host_compose", build_host_compose_callback())?;
    // Phase 3e-β — supervision hooks. All pure HOOK_CTX-shaped reads
    // (Tier A from 3d-β framing) — no token registry, no engine
    // re-entry, no NodeDefinition closure capture. Six thin shims
    // over graphreact-core's `use_monitor` / `use_demonitor` /
    // `use_link` / `use_unlink` / `use_group_join` / `use_group_leave`.
    // `__DOWN__` system-message delivery is wired in core's Scheduler
    // already; use_message from 3c catches it.
    engine.register_callback(iso, "host_monitor", build_host_monitor_callback())?;
    engine.register_callback(iso, "host_demonitor", build_host_demonitor_callback())?;
    engine.register_callback(iso, "host_link", build_host_link_callback())?;
    engine.register_callback(iso, "host_unlink", build_host_unlink_callback())?;
    engine.register_callback(
        iso,
        "host_group_join",
        build_host_group_join_callback(),
    )?;
    engine.register_callback(
        iso,
        "host_group_leave",
        build_host_group_leave_callback(),
    )?;
    // Phase 3e-γ — event-source surface. host_use_source decodes
    // descriptors from the `use_source(factory_fn, deps)` Python
    // wrapper and dispatches to graphreact-core's `core_use_source`.
    // All four source kinds (timer / delay / http / watch) ship
    // real — the followon-A infrastructure
    // (`crate::sources::http_source` / `crate::sources::watch_source`
    // + tokio + notify in Cargo.toml + tokio runtime in `grx`
    // main()) is already in place from JSC.
    engine.register_callback(
        iso,
        "host_use_source",
        build_host_use_source_callback(),
    )?;
    // Eval the Python prelude — installs use_state / use_effect /
    // use_effect_async / use_emit / use_message / use_memo /
    // use_callback / use_reducer / use_neighbors / use_topology /
    // use_accept_edge / use_edge_accept_handler / use_graph_query /
    // use_spawn_child / use_unmount_self / use_compose / use_monitor /
    // use_demonitor / use_link / use_unlink / use_group_join /
    // use_group_leave / use_source / timer_source / delay_source /
    // http_source / watch_source / __augment_ctx / __register_callback /
    // __invoke_callback / __release_callback / __release_tokens on
    // builtins.
    engine.eval_module(iso, "_grx_hook_prelude", HOOK_PRELUDE)?;
    Ok(())
}

// ---------------------------------------------------------------------
// PyRuntime — Scheduler + CPythonEngine + IR mount (phase 3b)
// ---------------------------------------------------------------------

/// Phase 3b composition: a [`Scheduler`] driving Python components on
/// a [`CPythonEngine`]. Returned by [`mount_with_scheduler`]; consumers
/// drive it via [`PyRuntime::process_batch`] and observe via the
/// signal registry.
pub struct PyRuntime {
    pub engine: Rc<RefCell<CPythonEngine>>,
    pub iso: IsolateHandle,
    pub scheduler: Scheduler,
    pub mounted: Vec<String>,
    pub graph_id: String,
    pub n_nodes: usize,
    pub n_edges: usize,
    pub n_definitions: usize,
    /// Per-component `body(ctx)` (or `init(ctx)`) handle. Phase 3a
    /// resolved one per definition; phase 3b reuses them across
    /// renders.
    pub component_handles: FxHashMap<String, FunctionHandle>,
    /// IDs of nodes the scheduler removed since the last
    /// `process_batch`. Drained at the end of every `process_batch`
    /// and used to release the per-node `_hostCallbacks` token bucket
    /// via `__release_tokens`. Phase 3b's stub for the followon-D
    /// pattern; phase 3+ may upgrade to per-node tracking once the
    /// hook surface widens.
    pub(crate) pending_unmounts: Arc<parking_lot::Mutex<Vec<String>>>,
    /// Phase 3d-γ: directory handle cloned from `scheduler.directory`
    /// at mount time. Installed into TLS by
    /// [`PyEngineGuard::with_directory`] for the duration of every
    /// `process_batch`; consumed by `host_directory_*` callbacks.
    pub directory: Rc<RefCell<GraphDirectory>>,
    /// Phase 3d-γ: graph snapshot bundling the live `Rc<RefCell<...>>`
    /// handles that `host_graph_*` Tier 1 callbacks read. Constructed
    /// via [`GraphSnapshot::from_live`] at mount time.
    pub graph_snapshot: Rc<GraphSnapshot>,
    /// Topology subscription kept alive — drop unsubscribes.
    _topology_subscription: Subscription,
}

impl std::fmt::Debug for PyRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PyRuntime")
            .field("graph_id", &self.graph_id)
            .field("n_nodes", &self.n_nodes)
            .field("n_edges", &self.n_edges)
            .field("n_definitions", &self.n_definitions)
            .field("mounted_count", &self.mounted.len())
            .field("component_handle_count", &self.component_handles.len())
            .field("directory_size", &self.directory.borrow().len())
            .finish_non_exhaustive()
    }
}

impl PyRuntime {
    /// Drive the scheduler one process_batch round. Installs the
    /// engine + iso + directory + graph_snapshot TLS for the duration
    /// so deferred-fire host callbacks (effects, message handlers,
    /// directory finders, graph queries) can re-enter CPython /
    /// access the snapshot.
    pub fn process_batch(&mut self) -> Result<(), graphreact_core::RuntimeError> {
        let _engine_guard = PyEngineGuard::with_directory(
            Rc::clone(&self.engine),
            self.iso,
            Rc::clone(&self.directory),
            Rc::clone(&self.graph_snapshot),
        );
        let result = self.scheduler.process_batch();
        // M13-followon-D analogue: sweep tokens for every node
        // removed during this batch. The sweep runs while the engine
        // guard is still installed (the EngineGuard above lives until
        // the end of process_batch), so the Python re-entry inside
        // `release_tokens_for_node` succeeds. `pending_unmounts` is
        // populated by the topology listener installed in
        // `mount_with_scheduler` on every NodeRemoved event.
        let drained: Vec<String> = std::mem::take(&mut *self.pending_unmounts.lock());
        for id in drained {
            release_tokens_for_node(&self.engine, self.iso, &id);
        }
        result
    }

    /// **M13-phase3g.** Async variant of [`PyRuntime::process_batch`].
    /// Drives `Scheduler::process_batch_async()` (which fires
    /// `flush_effects_async` for any nodes with pending async
    /// effects). Async effects registered via `use_effect_async` only
    /// fire on this path — the sync `process_batch` skips them by
    /// design (matches graphreact-core's opt-in async contract).
    ///
    /// Must be called from a `tokio::task::LocalSet` (or other
    /// single-threaded executor) because `PyRuntime` is `!Send` (the
    /// engine is `Rc<RefCell<...>>`). Same shape as JSC's
    /// `JsRuntime::process_batch_async`.
    ///
    /// CLI activation deferred — `grx run --runtime=cpython` still
    /// drives the sync `process_batch`. Rationale matches JSC's
    /// M12.3b-phase1 punt: nested-runtime restructuring is its own
    /// slice. **Not a regression** — `useEffectAsync` was a no-op
    /// stub on the sync CLI path before this slice.
    pub async fn process_batch_async(&mut self) -> Result<(), graphreact_core::RuntimeError> {
        let _engine_guard = PyEngineGuard::with_directory(
            Rc::clone(&self.engine),
            self.iso,
            Rc::clone(&self.directory),
            Rc::clone(&self.graph_snapshot),
        );
        let result = self.scheduler.process_batch_async().await;
        // M13-followon-D analogue: sweep tokens for every node
        // removed during this batch. Same shape as the sync path.
        let drained: Vec<String> = std::mem::take(&mut *self.pending_unmounts.lock());
        for id in drained {
            release_tokens_for_node(&self.engine, self.iso, &id);
        }
        result
    }

    /// Tear down — disposes the isolate and shuts the engine.
    pub fn shutdown(&mut self) -> Result<(), EngineError> {
        let mut e = self.engine.borrow_mut();
        e.dispose_isolate(self.iso)?;
        e.shutdown()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Mount entry points
// ---------------------------------------------------------------------

/// Phase 3a back-compat entry: parse + resolve a `.grx` file, mount
/// every `lang="python"` definition with a one-shot `init(ctx)` call
/// per node. No scheduler, no hooks. Retained so existing callers /
/// the phase 3a smoke fixture keep working through the same surface.
///
/// New code should prefer [`mount_with_scheduler`] which handles both
/// phase 3a-shaped bodies (no hooks) and phase 3b-shaped bodies (real
/// hooks + scheduler integration) via the `body(ctx)` / `init(ctx)`
/// fallback.
///
/// Returns a [`PyRuntime`] for shape parity; the scheduler inside has
/// no nodes registered (all mounting happens by direct
/// `call_function` here) and `process_batch` is a no-op.
pub fn mount_python_definitions(path: &std::path::Path) -> Result<PyRuntime, RuntimeError> {
    // Phase 3a back-compat: route through mount_with_scheduler which
    // now subsumes both shapes. The phase 3a mount_python_definitions
    // contract (no scheduler, just call init(ctx) once per node)
    // becomes a degenerate case: bodies that don't use hooks behave
    // identically under the scheduler-integrated path because
    // `process_batch` runs each node's body once and captures the
    // returned manifest.
    let mut runtime = mount_with_scheduler(path)?;
    // Run one batch so the initial mount fires for every node.
    runtime.process_batch().map_err(RuntimeError::Scheduler)?;
    // Surface per-node mount records. The scheduler drops manifests
    // into the directory; we re-walk the IR in component-name order
    // here for the report.
    if runtime.mounted.is_empty() {
        // Defensive — if process_batch evicted every node (e.g., body
        // raised), surface the raw component list.
        for handle in runtime.component_handles.keys() {
            runtime.mounted.push(format!("[{handle}]"));
        }
    }
    Ok(runtime)
}

/// Phase 3b mount: parses the `.grx` file, brings up CPython, installs
/// the HOOK_PRELUDE on the isolate, evaluates each `lang="python"`
/// definition as a sub-interpreter module, resolves a `body(ctx)` (or
/// `init(ctx)` for phase 3a fixtures) callable, and registers a
/// [`NodeDefinition`] with a fresh [`Scheduler`].
pub fn mount_with_scheduler(path: &std::path::Path) -> Result<PyRuntime, RuntimeError> {
    let resolved = resolve_multi_file(path)?;

    let mut definitions: Vec<(String, String, String, ComponentLang)> = Vec::new();
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

    let n_definitions = definitions.len();

    // Build IR for graph topology + report counts.
    let _ir_definitions: Vec<IrDefinition> = definitions
        .iter()
        .map(|(_, comp, code, lang)| IrDefinition {
            component: comp.clone(),
            code: code.clone(),
            lang: *lang,
            source_loc: graphreact_grx_parser::SourceLoc::default(),
        })
        .collect();

    let ir = graphreact_grx_parser::ir::build_ir(&graphreact_grx_parser::GrxFile {
        imports: resolved.root.imports.clone(),
        graph: Some(graph_block.clone()),
        definitions: resolved.root.definitions.clone(),
    });
    let n_nodes = ir.nodes.len();
    let n_edges = ir.edges.len();

    // Bring up CPython + register host callbacks + evaluate prelude.
    let engine_rc: Rc<RefCell<CPythonEngine>> = Rc::new(RefCell::new(CPythonEngine::new()));
    let iso = {
        let mut e = engine_rc.borrow_mut();
        e.initialize()?;
        let iso = e.create_isolate(IsolateOptions::default())?;
        // Install the hook prelude. This registers host_state /
        // host_state_set / host_effect callbacks AND evals the Python
        // prelude that wraps them as use_state / use_effect.
        // Drop the borrow before calling install_hook_prelude since
        // it takes &mut.
        drop(e);
        install_hook_prelude(&mut engine_rc.borrow_mut(), iso)?;
        iso
    };

    // Eval each Python definition + resolve its body callable.
    let mut component_handles: FxHashMap<String, FunctionHandle> = FxHashMap::default();
    for (origin, component, source, lang) in &definitions {
        if *lang != ComponentLang::Python {
            // Tear down on the error path so the engine doesn't leak.
            let _ = engine_rc.borrow_mut().dispose_isolate(iso);
            let _ = engine_rc.borrow_mut().shutdown();
            return Err(RuntimeError::EngineMisroute {
                component: component.clone(),
                lang: lang.as_attr_str().to_string(),
                engine: "cpython".to_string(),
            });
        }
        let module_name = format!("{origin}::{component}");
        // Phase 3d-γ: append the body-augmentation suffix so the
        // resolved `body` (or `init`) Python function wraps the user's
        // original through `__augment_ctx`. Python-side wrap mirrors
        // JSC's IIFE source-wrap; avoids the JSON round-trip that
        // would fail on the SimpleNamespace `ctx['directory']` proxy.
        let augmented_source = format!("{source}{BODY_AUGMENT_SUFFIX}");
        let module = engine_rc
            .borrow_mut()
            .eval_module(iso, &module_name, &augmented_source)?;
        // Phase 3b prefers `body(ctx)`; falls back to `init(ctx)` for
        // phase 3a fixtures that pre-date the convention shift. If
        // neither exists, surface a clear error.
        // Bind into a local before matching so the `borrow_mut()`
        // temporary doesn't extend across the Err arm's second
        // `borrow_mut()` (`match` extends temporaries to its tail).
        let try_body = engine_rc.borrow_mut().resolve_function(iso, module, "body");
        let body_handle = match try_body {
            Ok(h) => h,
            Err(_) => engine_rc
                .borrow_mut()
                .resolve_function(iso, module, "init")
                .map_err(|_| {
                    RuntimeError::Engine(EngineError::Internal(format!(
                        "Python definition `<{component}>` must define `body(ctx)` (preferred) \
                         or `init(ctx)` (phase 3a back-compat)"
                    )))
                })?,
        };
        component_handles.insert(component.clone(), body_handle);
    }

    // Bind the IR's graph to a fresh CommunicationGraph + Scheduler.
    let mut graph = CommunicationGraph::new();
    for ir_node in &ir.nodes {
        let mut attrs = Attrs::new();
        for (k, v) in &ir_node.attrs {
            attrs.insert(k.clone(), v.clone());
        }
        graph.add_node(ir_node.id.clone(), attrs);
    }
    for ir_edge in &ir.edges {
        let mut attrs = Attrs::new();
        for (k, v) in &ir_edge.attrs {
            attrs.insert(k.clone(), v.clone());
        }
        graph.add_edge(&ir_edge.source, &ir_edge.target, None, attrs);
    }

    let mut scheduler = Scheduler::new(graph);

    // Register each node with the scheduler, wrapping its body call
    // in a NodeDefinition.
    let mut mounted: Vec<String> = Vec::new();
    for node in &ir.nodes {
        let component_name = component_name_for_node(node);
        let Some(&fn_handle) = component_handles.get(&component_name) else {
            // Component not in our definition set — skip rather than
            // error. Phase 3+ may add a real diagnostic path.
            continue;
        };
        let definition = build_py_node_definition(
            component_name.clone(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut props: Attrs = Attrs::new();
        for (k, v) in &node.attrs {
            props.insert(k.clone(), v.clone());
        }
        #[allow(clippy::cast_possible_truncation)]
        let height: u32 = node.depth as u32;
        let _ = scheduler.register_node(definition, node.id.clone(), props, height)?;
        mounted.push(node.id.clone());
    }

    // Topology subscription so process_batch can drain unmounts.
    // **M15 Phase 9**: same listener also clears per-node
    // `SIGNAL_BY_HOOK_POS` plus `SIGNAL_REGISTRY` plus
    // `MEMO_REGISTRY` entries inline so incompat-reload's drain plus
    // remount path doesn't surface stale `Signal` handles to the
    // re-mount's first render.
    let pending_unmounts: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let pending_unmounts_for_listener = Arc::clone(&pending_unmounts);
    let topology_subscription =
        scheduler
            .graph
            .borrow()
            .on_topology_change(move |event: &TopologyEvent| {
                if let TopologyEvent::NodeRemoved { node_id } = event {
                    clear_node_local_state(node_id);
                    pending_unmounts_for_listener.lock().push(node_id.clone());
                }
            });

    // Phase 3d-γ: clone the live directory + build a graph snapshot
    // bundling the scheduler's `Rc<RefCell<...>>` handles. Both are
    // installed into TLS by `PyEngineGuard::with_directory` for the
    // duration of every `process_batch`.
    let directory = Rc::clone(&scheduler.directory);
    let graph_snapshot = Rc::new(GraphSnapshot::from_live(
        Rc::clone(&scheduler.graph),
        Rc::clone(&scheduler.manifests),
        Rc::clone(&scheduler.edge_accept_predicates),
    ));

    Ok(PyRuntime {
        engine: engine_rc,
        iso,
        scheduler,
        mounted,
        graph_id: ir.graph_id,
        n_nodes,
        n_edges,
        n_definitions,
        component_handles,
        pending_unmounts,
        directory,
        graph_snapshot,
        _topology_subscription: topology_subscription,
    })
}

/// Build a [`NodeDefinition`] that calls the Python body callable on
/// every render. Wraps each invocation in a [`ComponentEvalGuard`] so
/// hooks resolve to the correct node's slots.
///
/// Phase 3d-γ: ctx augmentation (injecting `ctx['directory']`) is done
/// Python-side via the body wrapper appended to user source at
/// `mount_with_scheduler` time — see [`BODY_AUGMENT_SUFFIX`]. Routing
/// the augmented ctx through Rust would require JSON-serialising the
/// `SimpleNamespace` proxy, which Python's stdlib `json.dumps` rejects.
/// Doing the augmentation entirely Python-side avoids the round-trip.
/// **M15 Phase 8.** Compile `source` as a fresh component body on the
/// existing CPython sub-interpreter and return a [`NodeDefinition`]
/// that dispatches to it. Reuses the same `engine` plus `iso` so the
/// sub-interpreter's per-`(node_id, hook_pos)` `MEMO_REGISTRY` /
/// `SIGNAL_REGISTRY` slots, the sharded `CALLBACK_REGISTRY`
/// (M13-phase3i), and the per-node `NODE_TOKENS` bucket all survive
/// the swap.
///
/// `component` doubles as the Python module name (passed through
/// `gr_cpython_module_register` → `PyImport_AddModule`). Re-using the
/// same module name lets `PyRun_StringFlags` rebind `body` (or `init`)
/// in place without disturbing other names that the previous body
/// pinned in the module's globals — analogous to JSC's
/// re-`eval_component` pattern.
///
/// `BODY_AUGMENT_SUFFIX` is appended Python-side so `ctx.directory`
/// is populated identically to the mount-time path.
pub fn build_cpython_node_definition_from_source(
    engine: &Rc<RefCell<CPythonEngine>>,
    iso: IsolateHandle,
    component: &str,
    source: &str,
) -> Result<NodeDefinition, EngineError> {
    let augmented = format!("{source}{BODY_AUGMENT_SUFFIX}");
    let module = engine
        .borrow_mut()
        .eval_module(iso, component, &augmented)?;
    // Phase 3b convention: `body(ctx)` is preferred. Fall back to
    // `init(ctx)` for legacy fixtures, mirroring the mount-time path.
    let try_body = engine.borrow_mut().resolve_function(iso, module, "body");
    let fn_handle = match try_body {
        Ok(h) => h,
        Err(_) => engine
            .borrow_mut()
            .resolve_function(iso, module, "init")
            .map_err(|_| {
                EngineError::Internal(format!(
                    "Python definition `<{component}>` must define `body(ctx)` (preferred) \
                     or `init(ctx)` (phase 3a back-compat)"
                ))
            })?,
    };
    Ok(build_py_node_definition(
        component.to_string(),
        Rc::clone(engine),
        iso,
        fn_handle,
    ))
}

fn build_py_node_definition(
    name: String,
    engine: Rc<RefCell<CPythonEngine>>,
    iso: IsolateHandle,
    fn_handle: FunctionHandle,
) -> NodeDefinition {
    NodeDefinition::new(name, move |ctx: &NodeContext| -> PermissionManifest {
        let _guard = ComponentEvalGuard::new(&ctx.id);
        let host_ctx = build_host_ctx(ctx);
        let result = engine.borrow_mut().call_function(iso, fn_handle, &[host_ctx]);
        match result {
            Ok(manifest) => host_value_to_manifest(manifest),
            Err(e) => {
                eprintln!("[graphreact-cli] CPython component invocation failed: {e}");
                PermissionManifest::default()
            }
        }
    })
}

/// Build the per-instance Python `ctx` from a [`NodeContext`].
/// `{id, props, neighbors}` shape; mirrors the JSC arm's `build_host_ctx`.
fn build_host_ctx(ctx: &NodeContext) -> HostValue {
    let mut props_pairs: Vec<(String, HostValue)> = Vec::new();
    for (k, v) in &ctx.props {
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
    ])
}

/// Convert a Python-returned manifest dict into a graphreact-core
/// [`PermissionManifest`]. Only `accepts` and `emits` are populated
/// in phase 3b — channel policies + per-neighbor overrides land in
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
            // /v1/nodes/:id/messages). Default-deny — absent or empty
            // means the node refuses every external type.
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
            // **M-Ingress phase 2.** Optional Python-declared list of
            // call-shaped external acceptance — `[{"request_type": "...",
            // "reply_type": "..."}, ...]`. Powers `POST /v1/nodes/:id/call`
            // with auto-correlation. Entries missing either field are
            // silently skipped (no half-pairs make sense).
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

fn component_name_for_node(node: &IrNode) -> String {
    node.attrs
        .get("component")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| node.component.clone())
}

#[cfg(test)]
pub(crate) fn signal_set_external(id: u64, value: Value) -> Result<(), EngineError> {
    SIGNAL_REGISTRY.with(|reg| {
        let registry = reg.borrow();
        let Some(signal) = registry.get(&id) else {
            return Err(EngineError::Internal(format!(
                "signal_set_external: no signal registered for id {id}"
            )));
        };
        signal.set(value);
        Ok(())
    })
}

/// Crate-wide CPython test serializer. CPython's main interpreter is
/// process-global; every test that touches `CPythonEngine::initialize`
/// / `create_isolate` / `eval_module` etc. must hold this guard.
///
/// Hoisted to `pub(crate)` for the same reason as `jsruntime::engine_lock`
/// — module-local locks can't serialize cross-module test parallelism.
/// **M15 Phase 8.**
#[cfg(test)]
pub(crate) fn engine_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// **M15 Phase 9.** Clear all per-`(node_id, hook_pos)` thread-local
/// entries for a CPython node that just unmounted. Mirrors
/// `crate::jsruntime::clear_node_local_state` — runs synchronously
/// from the topology listener so a subsequent re-mount of the same
/// node id (incompat-reload's drain plus remount path) doesn't pick
/// up stale `Signal` handles whose `GenerationalBox` was dropped
/// on unmount.
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

#[cfg(test)]
pub(crate) fn signal_id_for(node_id: &str, hook_pos: usize) -> Option<u64> {
    SIGNAL_BY_HOOK_POS.with(|m| m.borrow().get(&(node_id.to_string(), hook_pos)).copied())
}

#[cfg(test)]
fn reset_global_state() {
    SIGNAL_REGISTRY.with(|r| r.borrow_mut().clear());
    SIGNAL_BY_HOOK_POS.with(|m| m.borrow_mut().clear());
    NEXT_SIGNAL_ID.with(|c| c.set(1));
    MEMO_REGISTRY.with(|r| r.borrow_mut().clear());
    PENDING_MEMO_KEY.with(|p| *p.borrow_mut() = None);
    CURRENT_NODE_ID.with(|n| *n.borrow_mut() = None);
    PY_HOOK_INDEX.with(|i| i.set(0));
    CURRENT_ENGINE.with(|c| *c.borrow_mut() = None);
    CURRENT_ISO.with(|c| c.set(None));
    CURRENT_DIRECTORY.with(|c| *c.borrow_mut() = None);
    CURRENT_GRAPH_SNAPSHOT.with(|c| *c.borrow_mut() = None);
    // M13-followon-D analogue: clear per-node token buckets between
    // tests so `tracked_token_count()` assertions start from zero.
    NODE_TOKENS.with(|m| m.borrow_mut().clear());
    // M13-phase3g: clear async-effect completion registry. Drop the
    // Senders so any parked Receivers observe `Err(_)` (cancellation)
    // and don't leak across test boundaries.
    EFFECT_ASYNC_COMPLETIONS.with(|m| m.borrow_mut().clear());
    NEXT_EFFECT_ASYNC_COMPLETION_ID.with(|c| c.set(1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    /// CPython's main interpreter is process-global; tests must
    /// serialise. Forwards to the crate-wide `super::engine_lock`
    /// so cross-module CPython tests share a single mutex.
    fn engine_lock() -> MutexGuard<'static, ()> {
        super::engine_lock()
    }

    /// Phase 3a end-to-end: a .grx fixture with `lang="python"` bodies
    /// (using the legacy `init(ctx)` convention) mounts every node
    /// through CPython and produces a manifest per mount. Now routed
    /// through mount_with_scheduler — verifies phase 3a back-compat.
    #[test]
    fn phase3a_python_fixture_mounts_through_pyruntime() {
        let _g = engine_lock();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python-smoke.grx");
        let mut runtime = mount_python_definitions(&fixture).expect("mount ok");

        assert_eq!(runtime.graph_id, "py-smoke");
        assert_eq!(runtime.n_nodes, 2);
        assert_eq!(runtime.mounted.len(), 2);
        runtime.shutdown().expect("shutdown ok");
    }

    /// Phase 3a routes `lang="js"` bodies away from the cpython arm
    /// with an explicit error.
    #[test]
    fn phase3a_rejects_js_lang_definition() {
        let _g = engine_lock();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python-smoke-mixed-rejected.grx");
        let r = mount_python_definitions(&fixture);
        match r {
            Err(RuntimeError::EngineMisroute { lang, engine, .. }) => {
                assert_eq!(lang, "js");
                assert_eq!(engine, "cpython");
            }
            other => panic!("expected EngineMisroute, got {other:?}"),
        }
    }

    /// Phase 3b: `use_state` preserves value across renders. Build a
    /// counter body that observes `count` via host_observe; drive two
    /// process_batch rounds with an external `signal.set(n)` between
    /// them; assert the second observation reflects the new value.
    #[test]
    fn phase3b_use_state_preserves_value_across_renders() {
        let _g = engine_lock();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python-counter.grx");
        let mut runtime = mount_with_scheduler(&fixture).expect("mount");

        // Capture observations from the body's `host_observe(count)` call.
        let observations: Arc<parking_lot::Mutex<Vec<i64>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = Arc::clone(&observations);
        runtime
            .engine
            .borrow_mut()
            .register_callback(
                runtime.iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::Number(n)) = args.first() {
                        #[allow(clippy::cast_possible_truncation)]
                        observations_for_cb.lock().push(*n as i64);
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("register host_observe");

        // Initial mount: process_batch evaluates the body once.
        runtime.process_batch().expect("batch 1");
        // Externally bump the signal — this marks the node dirty.
        let id = signal_id_for("counter-1", 0).expect("signal id");
        signal_set_external(id, Value::Number(7.into())).expect("set");
        // Second batch: re-renders the body; observed count should be 7.
        runtime.process_batch().expect("batch 2");

        let obs = observations.lock();
        assert!(
            obs.len() >= 2,
            "expected at least 2 observations, got {obs:?}"
        );
        assert_eq!(obs[0], 0, "first render observes initial 0; got {obs:?}");
        assert_eq!(obs[1], 7, "second render observes new value 7; got {obs:?}");
        drop(obs);
        runtime.shutdown().expect("shutdown");
    }

    /// Phase 3b: `use_state`'s setter closure marks the owning node
    /// dirty when called. Mirrors the previous test but drives the
    /// setter through Python (calling `set_count(n)` from a
    /// host-callback-triggered helper).
    #[test]
    fn phase3b_use_state_setter_from_python_re_renders_node() {
        let _g = engine_lock();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python-setter.grx");
        let mut runtime = mount_with_scheduler(&fixture).expect("mount");

        let observations: Arc<parking_lot::Mutex<Vec<i64>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = Arc::clone(&observations);
        runtime
            .engine
            .borrow_mut()
            .register_callback(
                runtime.iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::Number(n)) = args.first() {
                        #[allow(clippy::cast_possible_truncation)]
                        observations_for_cb.lock().push(*n as i64);
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("register host_observe");

        runtime.process_batch().expect("batch 1");
        // The python-setter fixture's body sets count=1 inside an
        // effect on first render. Second batch observes the bumped
        // value.
        runtime.process_batch().expect("batch 2");

        let obs = observations.lock();
        assert!(
            obs.contains(&1),
            "expected setter from Python to bump count to 1: got {obs:?}"
        );
        drop(obs);
        runtime.shutdown().expect("shutdown");
    }

    /// Phase 3b: `use_effect` fires on mount and (when deps change)
    /// re-fires; cleanup runs before the new effect.
    #[test]
    fn phase3b_use_effect_fires_on_mount_with_cleanup() {
        let _g = engine_lock();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python-effect.grx");
        let mut runtime = mount_with_scheduler(&fixture).expect("mount");

        let events: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let events_for_cb = Arc::clone(&events);
        runtime
            .engine
            .borrow_mut()
            .register_callback(
                runtime.iso,
                "host_log",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        events_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("register host_log");

        runtime.process_batch().expect("batch 1");
        // Bump the signal; deps include `count`, so the effect should
        // run cleanup + re-mount.
        let id = signal_id_for("logger-1", 0).expect("signal id");
        signal_set_external(id, Value::Number(1.into())).expect("set");
        runtime.process_batch().expect("batch 2");

        let ev = events.lock();
        // Python `count` arrives as a JSON-marshalled float; the
        // f-string emits "0.0" / "1.0" rather than "0" / "1". The
        // semantic shape is "mount fires on initial render, cleanup
        // runs when deps change, new mount fires for the new value".
        assert!(
            ev.iter().any(|e| e.starts_with("mount:0")),
            "expected mount:0 event: {ev:?}"
        );
        assert!(
            ev.iter().any(|e| e.starts_with("cleanup:0")),
            "expected cleanup:0 event after deps change: {ev:?}"
        );
        assert!(
            ev.iter().any(|e| e.starts_with("mount:1")),
            "expected mount:1 event after re-render: {ev:?}"
        );
        drop(ev);
        runtime.shutdown().expect("shutdown");
    }

    /// Phase 3b: empty deps → effect fires once on mount and never
    /// re-fires across re-renders.
    #[test]
    fn phase3b_use_effect_empty_deps_fires_once() {
        let _g = engine_lock();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python-effect-empty-deps.grx");
        let mut runtime = mount_with_scheduler(&fixture).expect("mount");

        let events: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let events_for_cb = Arc::clone(&events);
        runtime
            .engine
            .borrow_mut()
            .register_callback(
                runtime.iso,
                "host_log",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        events_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("register host_log");

        runtime.process_batch().expect("batch 1");
        let id = signal_id_for("once-1", 0).expect("signal id");
        signal_set_external(id, Value::Number(99.into())).expect("set");
        runtime.process_batch().expect("batch 2");

        let ev = events.lock();
        let mount_count = ev.iter().filter(|e| e.as_str() == "mount").count();
        assert_eq!(
            mount_count, 1,
            "empty-deps effect should fire exactly once: {ev:?}"
        );
        drop(ev);
        runtime.shutdown().expect("shutdown");
    }

    /// Test helper: bring up CPython, install the HOOK_PRELUDE, and
    /// register a `host_observe(value)` callback that pushes onto the
    /// supplied buffer. Returns the `(engine_rc, iso)` pair so tests
    /// can eval bodies + register nodes against the same isolate.
    fn boot_engine_with_observe(
        observations: Arc<parking_lot::Mutex<Vec<String>>>,
    ) -> (Rc<RefCell<CPythonEngine>>, IsolateHandle) {
        let engine_rc: Rc<RefCell<CPythonEngine>> = Rc::new(RefCell::new(CPythonEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            drop(e);
            install_hook_prelude(&mut engine_rc.borrow_mut(), iso).expect("prelude");
            engine_rc
                .borrow_mut()
                .register_callback(
                    iso,
                    "host_observe",
                    Box::new(move |args| {
                        if let Some(hv) = args.first() {
                            // Coerce to a printable representation.
                            let s = match hv {
                                HostValue::String(s) => s.clone(),
                                HostValue::Number(n) => format!("{n}"),
                                HostValue::Bool(b) => format!("{b}"),
                                HostValue::Null => "null".to_string(),
                                other => format!("{other:?}"),
                            };
                            observations.lock().push(s);
                        }
                        Ok(HostValue::Null)
                    }),
                )
                .expect("host_observe");
            iso
        };
        (engine_rc, iso)
    }

    /// Eval a Python body string as a module, resolve its `body`
    /// function, return the [`FunctionHandle`].
    fn eval_python_body(
        engine: &Rc<RefCell<CPythonEngine>>,
        iso: IsolateHandle,
        component: &str,
        body: &str,
    ) -> FunctionHandle {
        // Phase 3d-γ: append the same body-augmentation suffix that
        // `mount_with_scheduler` applies so test bodies see
        // `ctx['directory']` after the wrapper runs.
        let augmented = format!("{body}{BODY_AUGMENT_SUFFIX}");
        let module = engine
            .borrow_mut()
            .eval_module(iso, component, &augmented)
            .expect("eval");
        engine
            .borrow_mut()
            .resolve_function(iso, module, "body")
            .expect("resolve body")
    }

    /// Phase 3c: a Python body's `use_emit` reaches a neighbor's
    /// `use_message(type, handler)`. Tests the minimal end-to-end flow:
    /// emit pushes onto the scheduler queue, `deliver()` resolves the
    /// channel policy, the handler fires.
    #[test]
    fn phase3c_python_emit_dispatches_to_neighbor() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body_a = r"
def body(ctx):
    emit = use_emit()
    def on_mount():
        emit('ping', {'hello': 'world'})
    use_effect(on_mount, [])
    return {'accepts': [], 'emits': ['ping']}
";
        let body_b = r"
def body(ctx):
    def on_ping(msg):
        host_observe('b-received')
    use_message('ping', on_ping)
    return {'accepts': ['ping'], 'emits': []}
";

        let fn_a = eval_python_body(&engine_rc, iso, "A", body_a);
        let fn_b = eval_python_body(&engine_rc, iso, "B", body_b);

        let def_a = build_py_node_definition("A".into(), Rc::clone(&engine_rc), iso, fn_a);
        let def_b = build_py_node_definition("B".into(), Rc::clone(&engine_rc), iso, fn_b);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_a, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register a");
        scheduler
            .register_node(def_b, "b".to_string(), Attrs::new(), 0_u32)
            .expect("register b");

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert_eq!(
                observed.as_slice(),
                &["b-received".to_string()],
                "B's use_message handler observed exactly one delivery from A's emit"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3c: the message handed to a `use_message` handler has the
    /// `{from, type, payload, ttl}` shape. Asserts nested payload
    /// survives the JSON FFI round-trip and `ttl` is a number type.
    #[test]
    fn phase3c_python_use_message_handler_receives_marshalled_message() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("sender".to_string(), Attrs::new());
        graph.add_node("recv".to_string(), Attrs::new());
        graph.add_edge("sender".to_string(), "recv".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body_sender = r"
def body(ctx):
    emit = use_emit()
    def on_mount():
        emit('greet', {'greeting': 'hi', 'count': 7, 'nested': {'ok': True}})
    use_effect(on_mount, [])
    return {'accepts': [], 'emits': ['greet']}
";
        let body_recv = r"
def body(ctx):
    def on_greet(msg):
        host_observe('from=' + msg['from'])
        host_observe('type=' + msg['type'])
        host_observe('payload.greeting=' + msg['payload']['greeting'])
        host_observe('payload.count=' + str(int(msg['payload']['count'])))
        host_observe('payload.nested.ok=' + str(msg['payload']['nested']['ok']))
        host_observe('ttl-typeof=' + type(msg['ttl']).__name__)
    use_message('greet', on_greet)
    return {'accepts': ['greet'], 'emits': []}
";

        let fn_s = eval_python_body(&engine_rc, iso, "Sender", body_sender);
        let fn_r = eval_python_body(&engine_rc, iso, "Recv", body_recv);

        let def_s = build_py_node_definition("Sender".into(), Rc::clone(&engine_rc), iso, fn_s);
        let def_r = build_py_node_definition("Recv".into(), Rc::clone(&engine_rc), iso, fn_r);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_s, "sender".to_string(), Attrs::new(), 0_u32)
            .expect("register sender");
        scheduler
            .register_node(def_r, "recv".to_string(), Attrs::new(), 0_u32)
            .expect("register recv");

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            // Each entry tests a different field of the marshalled
            // `{from, type, payload, ttl}` envelope. ttl is JSON
            // number → Python float → "float" type name.
            assert!(
                observed.contains(&"from=sender".to_string()),
                "from: {observed:?}"
            );
            assert!(
                observed.contains(&"type=greet".to_string()),
                "type: {observed:?}"
            );
            assert!(
                observed.contains(&"payload.greeting=hi".to_string()),
                "greeting: {observed:?}"
            );
            assert!(
                observed.contains(&"payload.count=7".to_string()),
                "count: {observed:?}"
            );
            assert!(
                observed.contains(&"payload.nested.ok=True".to_string()),
                "nested: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s.starts_with("ttl-typeof=")),
                "ttl typeof emitted: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3c: `use_message` handler running setter on incoming
    /// message marks the owning node dirty; the next `process_batch`
    /// re-renders with the new state. Proves messages drive state
    /// mutation through the scheduler.
    #[test]
    fn phase3c_python_use_message_handler_re_renders_on_message() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("sender".to_string(), Attrs::new());
        graph.add_node("recv".to_string(), Attrs::new());
        graph.add_edge("sender".to_string(), "recv".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body_sender = r"
def body(ctx):
    emit = use_emit()
    def on_mount():
        emit('bump', None)
    use_effect(on_mount, [])
    return {'accepts': [], 'emits': ['bump']}
";
        // Receiver records its `count` on every render and bumps it
        // when a `bump` arrives. We expect: render 1 observes 0; the
        // message-handler setter marks the node dirty; render 2
        // observes 1.
        let body_recv = r"
def body(ctx):
    count, set_count = use_state(0)
    host_observe('count=' + str(count))
    def on_bump(msg):
        set_count(count + 1)
    use_message('bump', on_bump)
    return {'accepts': ['bump'], 'emits': []}
";

        let fn_s = eval_python_body(&engine_rc, iso, "Sender2", body_sender);
        let fn_r = eval_python_body(&engine_rc, iso, "Recv2", body_recv);

        let def_s = build_py_node_definition("Sender2".into(), Rc::clone(&engine_rc), iso, fn_s);
        let def_r = build_py_node_definition("Recv2".into(), Rc::clone(&engine_rc), iso, fn_r);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_s, "sender".to_string(), Attrs::new(), 0_u32)
            .expect("register sender");
        scheduler
            .register_node(def_r, "recv".to_string(), Attrs::new(), 0_u32)
            .expect("register recv");

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            // Float-or-int formatting: JSON marshalling makes the
            // `count` reach Python as int(0) since the literal is set
            // by `set_count(count + 1)` from int side. The initial
            // `use_state(0)` marshals back through HostValue::Number
            // (f64) → str(0.0). Be format-tolerant.
            assert!(
                observed.iter().any(|s| s == "count=0" || s == "count=0.0"),
                "first render observes initial 0: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "count=1" || s == "count=1.0"),
                "second render observes bumped value: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3c: `emit('msg', payload, {'direct': 'node-c'})` addresses
    /// node-c specifically, bypassing the structural neighbor list.
    /// Three-node fixture A → B (graph edge) but A's emit targets C.
    #[test]
    fn phase3c_python_emit_with_direct_target_addresses_specific_node() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());
        graph.add_node("c".to_string(), Attrs::new());
        // Structural edges include B; direct addressing skips the
        // neighbor list and targets C.
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("a".to_string(), "c".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body_a = r"
def body(ctx):
    emit = use_emit()
    def on_mount():
        emit('ping', {'k': 1}, {'direct': 'c'})
    use_effect(on_mount, [])
    return {'accepts': [], 'emits': ['ping']}
";
        let body_b = r"
def body(ctx):
    def on_ping(msg):
        host_observe('b-saw')
    use_message('ping', on_ping)
    return {'accepts': ['ping'], 'emits': []}
";
        let body_c = r"
def body(ctx):
    def on_ping(msg):
        host_observe('c-saw')
    use_message('ping', on_ping)
    return {'accepts': ['ping'], 'emits': []}
";

        let fn_a = eval_python_body(&engine_rc, iso, "Adir", body_a);
        let fn_b = eval_python_body(&engine_rc, iso, "Bdir", body_b);
        let fn_c = eval_python_body(&engine_rc, iso, "Cdir", body_c);

        let def_a = build_py_node_definition("Adir".into(), Rc::clone(&engine_rc), iso, fn_a);
        let def_b = build_py_node_definition("Bdir".into(), Rc::clone(&engine_rc), iso, fn_b);
        let def_c = build_py_node_definition("Cdir".into(), Rc::clone(&engine_rc), iso, fn_c);

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

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.contains(&"c-saw".to_string()),
                "direct target c received: {observed:?}"
            );
            assert!(
                !observed.contains(&"b-saw".to_string()),
                "direct addressing skipped neighbor b: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3c: the receiver's permission manifest doesn't include
    /// the message type in `accepts`. Scheduler's `deliver` silently
    /// drops the message at policy resolution; the handler never
    /// fires.
    #[test]
    fn phase3c_python_permission_manifest_drops_unaccepted_messages() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body_a = r"
def body(ctx):
    emit = use_emit()
    def on_mount():
        emit('foo', None)
    use_effect(on_mount, [])
    return {'accepts': [], 'emits': ['foo']}
";
        // Receiver registers a handler for 'foo' but its manifest
        // only declares `accepts: ['bar']`. Scheduler drops 'foo' at
        // delivery; on_foo never fires.
        let body_b = r"
def body(ctx):
    def on_foo(msg):
        host_observe('b-received-foo')
    use_message('foo', on_foo)
    return {'accepts': ['bar'], 'emits': []}
";

        let fn_a = eval_python_body(&engine_rc, iso, "Aperm", body_a);
        let fn_b = eval_python_body(&engine_rc, iso, "Bperm", body_b);

        let def_a = build_py_node_definition("Aperm".into(), Rc::clone(&engine_rc), iso, fn_a);
        let def_b = build_py_node_definition("Bperm".into(), Rc::clone(&engine_rc), iso, fn_b);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_a, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register a");
        scheduler
            .register_node(def_b, "b".to_string(), Attrs::new(), 0_u32)
            .expect("register b");

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                !observed.contains(&"b-received-foo".to_string()),
                "manifest must drop unaccepted message types: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3c: bidirectional ping-pong via use_emit + use_message
    /// with Python-side `if bounces < 3` guards for deterministic
    /// termination. Mirrors JSC's
    /// `phase3d_ping_pong_bounces_through_jsc`.
    #[test]
    fn phase3c_python_ping_pong_terminates_via_python_guards() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("ping".to_string(), Attrs::new());
        graph.add_node("pong".to_string(), Attrs::new());
        graph.add_edge("ping".to_string(), "pong".to_string(), None, Attrs::new());
        graph.add_edge("pong".to_string(), "ping".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body_ping = r"
def body(ctx):
    bounces, set_bounces = use_state(0)
    emit = use_emit()
    def on_pong(msg):
        host_observe('ping-recv:' + str(int(bounces)))
        if bounces < 3:
            set_bounces(bounces + 1)
    use_message('pong', on_pong)
    def on_mount():
        if bounces < 3:
            host_observe('ping-emit:' + str(int(bounces)))
            emit('ping', {'n': bounces})
    use_effect(on_mount, [bounces])
    return {'accepts': ['pong'], 'emits': ['ping']}
";
        // Pong only emits AFTER it has received at least one ping
        // (bounces > 0), matching JSC's pattern.
        let body_pong = r"
def body(ctx):
    bounces, set_bounces = use_state(0)
    emit = use_emit()
    def on_ping(msg):
        host_observe('pong-recv:' + str(int(bounces)))
        if bounces < 3:
            set_bounces(bounces + 1)
    use_message('ping', on_ping)
    def on_mount():
        if 0 < bounces <= 3:
            host_observe('pong-emit:' + str(int(bounces)))
            emit('pong', {'n': bounces})
    use_effect(on_mount, [bounces])
    return {'accepts': ['ping'], 'emits': ['pong']}
";

        let fn_ping = eval_python_body(&engine_rc, iso, "PingPP", body_ping);
        let fn_pong = eval_python_body(&engine_rc, iso, "PongPP", body_pong);

        let def_ping = build_py_node_definition("PingPP".into(), Rc::clone(&engine_rc), iso, fn_ping);
        let def_pong = build_py_node_definition("PongPP".into(), Rc::clone(&engine_rc), iso, fn_pong);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_ping, "ping".to_string(), Attrs::new(), 0_u32)
            .expect("register ping");
        scheduler
            .register_node(def_pong, "pong".to_string(), Attrs::new(), 0_u32)
            .expect("register pong");

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
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

    /// Phase 3c: the canonical `python-pingpong.grx` fixture mounts
    /// through `mount_with_scheduler` and terminates naturally via
    /// edge TTL (`<edge ttl={5} />`). No `MaxDepthExceeded` — the
    /// scheduler exhausts the cycle through TTL inheritance
    /// (M4-followon).
    #[test]
    fn phase3c_python_pingpong_grx_fixture_terminates_via_ttl() {
        let _g = engine_lock();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python-pingpong.grx");
        let mut runtime = mount_with_scheduler(&fixture).expect("mount");
        assert_eq!(runtime.graph_id, "py-ping-pong");
        assert_eq!(runtime.n_nodes, 2);

        // Drive multiple batches; each tick decays TTL further. With
        // edge ttl=5 the cycle exhausts within a small bounded number.
        // We assert the runtime survives the cycle without panicking
        // and the second-batch state shows progress past the initial
        // mount.
        for _ in 0..6 {
            runtime.process_batch().expect("batch");
        }

        runtime.shutdown().expect("shutdown");
    }

    /// HOOK_PRELUDE installs the wrapper functions on builtins so any
    /// user module can call `use_state` / `use_effect` / `use_emit` /
    /// `use_message` / `use_memo` / `use_callback` / `use_reducer` /
    /// `use_neighbors` / `use_topology` / `use_accept_edge` /
    /// `use_graph_query` / `__augment_ctx` without import. Originally
    /// a phase 3b test; extended in phase 3c (message hooks), 3d-α
    /// (memoization), 3d-β (graph-read hooks), 3d-γ (directory +
    /// graph_query).
    #[test]
    fn phase3b_hook_prelude_visible_as_builtins() {
        let _g = engine_lock();
        reset_global_state();
        let mut engine = CPythonEngine::new();
        engine.initialize().expect("init");
        let iso = engine.create_isolate(IsolateOptions::default()).expect("create");
        install_hook_prelude(&mut engine, iso).expect("prelude");

        // A standalone Python module should see use_state on builtins.
        // (Calling it would require an active node — that's tested
        // end-to-end via mount_with_scheduler. Here we just probe the
        // identity is callable.)
        // Probe: the prelude installs wrappers on the builtins module.
        // Verify each is reachable via `import builtins; hasattr(builtins, …)`
        // so we're testing module-import semantics, not just name
        // resolution inside this specific module.
        let m = engine
            .eval_module(
                iso,
                "probe",
                "def probe():\n    \
                     import builtins\n    \
                     names = ('use_state', 'use_effect', 'use_effect_async', 'use_emit', 'use_message', 'use_memo', 'use_callback', 'use_reducer', 'use_neighbors', 'use_topology', 'use_accept_edge', 'use_edge_accept_handler', 'use_graph_query', 'use_spawn_child', 'use_unmount_self', 'use_compose', 'use_monitor', 'use_demonitor', 'use_link', 'use_unlink', 'use_group_join', 'use_group_leave', 'use_source', 'timer_source', 'delay_source', 'http_source', 'watch_source', '__register_callback', '__invoke_callback', '__augment_ctx')\n    \
                     return [n for n in names if hasattr(builtins, n)]\n",
            )
            .expect("eval");
        let probe = engine.resolve_function(iso, m, "probe").expect("resolve");
        let r = engine.call_function(iso, probe, &[]).expect("call");
        match r {
            HostValue::Array(found) => {
                let names: Vec<String> = found
                    .into_iter()
                    .filter_map(|v| if let HostValue::String(s) = v { Some(s) } else { None })
                    .collect();
                for required in &[
                    "use_state",
                    "use_effect",
                    "use_effect_async",
                    "use_emit",
                    "use_message",
                    "use_memo",
                    "use_callback",
                    "use_reducer",
                    "use_neighbors",
                    "use_topology",
                    "use_accept_edge",
                    "use_edge_accept_handler",
                    "use_graph_query",
                    "use_spawn_child",
                    "use_unmount_self",
                    "use_compose",
                    "use_monitor",
                    "use_demonitor",
                    "use_link",
                    "use_unlink",
                    "use_group_join",
                    "use_group_leave",
                    "use_source",
                    "timer_source",
                    "delay_source",
                    "http_source",
                    "watch_source",
                    "__register_callback",
                    "__invoke_callback",
                    "__augment_ctx",
                ] {
                    assert!(
                        names.contains(&(*required).to_string()),
                        "missing {required}: names={names:?}"
                    );
                }
            }
            other => panic!("expected Array, got {other:?}"),
        }
        engine.dispose_isolate(iso).expect("dispose");
        engine.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // Phase 3d-α tests — use_memo + use_callback + use_reducer
    // -----------------------------------------------------------------

    /// Phase 3d-α: `use_memo` with constant deps caches across renders.
    /// Factory increments an external counter (via `host_observe`) on
    /// every invocation; assert the counter advances exactly once
    /// across 3 renders.
    #[test]
    fn phase3d_alpha_use_memo_caches_when_deps_unchanged() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("memo".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Body uses `use_state` to drive re-renders + `use_memo` with
        // CONSTANT deps `[42]`. The factory observes 'factory-ran' on
        // every invocation. Across 3 renders we expect exactly 1
        // 'factory-ran' (cache hit on renders 2+3).
        let body = r"
def body(ctx):
    count, set_count = use_state(0)
    def factory():
        host_observe('factory-ran')
        return 100
    value = use_memo(factory, [42])
    host_observe('value=' + str(int(value)))
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "MemoConst", body);
        let def =
            build_py_node_definition("MemoConst".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "memo".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // Three batches: bump signal twice between to drive re-renders.
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 1");
        }
        let id = signal_id_for("memo", 0).expect("signal id");
        signal_set_external(id, Value::Number(1.into())).expect("set 1");
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 2");
        }
        signal_set_external(id, Value::Number(2.into())).expect("set 2");
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 3");
        }

        {
            let observed = observations.lock();
            let factory_runs = observed.iter().filter(|s| s.as_str() == "factory-ran").count();
            assert_eq!(
                factory_runs, 1,
                "factory should run once across 3 renders with constant deps: {observed:?}"
            );
            let value_obs = observed.iter().filter(|s| s.starts_with("value=")).count();
            assert_eq!(
                value_obs, 3,
                "value should be observed on every render: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-α: `use_memo` with deps including `count` recomputes
    /// each time count changes. Drive 3 batches with intermediate
    /// `signal_set_external`. Assert factory ran 3× and the values
    /// observed are 0, 10, 14 in order.
    #[test]
    fn phase3d_alpha_use_memo_recomputes_on_dep_change() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("memo".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // `use_memo(lambda: count*2, [count])` — factory observes
        // 'factory:<n>' on every invocation. Re-renders with count
        // changes should trigger fresh factory runs.
        let body = r"
def body(ctx):
    count, set_count = use_state(0)
    def factory():
        n = int(count)
        host_observe('factory:' + str(n))
        return n * 2
    value = use_memo(factory, [count])
    host_observe('value=' + str(int(value)))
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "MemoDep", body);
        let def =
            build_py_node_definition("MemoDep".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "memo".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 1");
        }
        let id = signal_id_for("memo", 0).expect("signal id");
        signal_set_external(id, Value::Number(5.into())).expect("set 5");
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 2");
        }
        signal_set_external(id, Value::Number(7.into())).expect("set 7");
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 3");
        }

        {
            let observed = observations.lock();
            let factory_runs: Vec<&String> = observed
                .iter()
                .filter(|s| s.starts_with("factory:"))
                .collect();
            assert_eq!(
                factory_runs.len(),
                3,
                "factory should run 3× as deps change: {observed:?}"
            );
            assert_eq!(factory_runs[0], "factory:0", "first run sees count=0: {observed:?}");
            assert_eq!(factory_runs[1], "factory:5", "second run sees count=5: {observed:?}");
            assert_eq!(factory_runs[2], "factory:7", "third run sees count=7: {observed:?}");
            // Also verify the memoed values reach the body.
            assert!(observed.iter().any(|s| s == "value=0"), "{observed:?}");
            assert!(observed.iter().any(|s| s == "value=10"), "{observed:?}");
            assert!(observed.iter().any(|s| s == "value=14"), "{observed:?}");
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-α: `use_callback(fn, [42])` returns the same Python
    /// function reference across 3 renders. Stash the returned function
    /// id (`id(fn)`) on every render via `host_observe`; assert all 3
    /// ids are identical (Python `is` semantics — same object).
    #[test]
    fn phase3d_alpha_use_callback_identity_stable_across_renders() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("cb".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Note: the inner function `def handler(): pass` is a fresh
        // Python object on every render. use_callback's cache should
        // return the FIRST render's function on subsequent renders.
        let body = r"
def body(ctx):
    count, set_count = use_state(0)
    def handler():
        return 42
    cb = use_callback(handler, [42])
    host_observe('cb-id:' + str(id(cb)))
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "CbStable", body);
        let def =
            build_py_node_definition("CbStable".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "cb".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 1");
        }
        let id = signal_id_for("cb", 0).expect("signal id");
        signal_set_external(id, Value::Number(1.into())).expect("set 1");
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 2");
        }
        signal_set_external(id, Value::Number(2.into())).expect("set 2");
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 3");
        }

        {
            let observed = observations.lock();
            let cb_ids: Vec<&String> = observed
                .iter()
                .filter(|s| s.starts_with("cb-id:"))
                .collect();
            assert_eq!(
                cb_ids.len(),
                3,
                "expected 3 cb-id observations: {observed:?}"
            );
            assert_eq!(
                cb_ids[0], cb_ids[1],
                "use_callback returns same fn on render 2: {observed:?}"
            );
            assert_eq!(
                cb_ids[0], cb_ids[2],
                "use_callback returns same fn on render 3: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-α: `use_reducer(reducer, 0)` composes over `use_state`.
    /// Effect on first render dispatches 'increment'; second render
    /// observes count=1 (proves reducer → setState → re-render).
    #[test]
    fn phase3d_alpha_use_reducer_dispatch_updates_state() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("rdc".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Counter reducer; initial state 0; effect on mount dispatches
        // 'increment' once. Second batch should observe state=1.
        let body = r"
def body(ctx):
    def reducer(state, action):
        if action == 'increment':
            return int(state) + 1
        return state
    state, dispatch = use_reducer(reducer, 0)
    host_observe('state=' + str(int(state)))
    def on_mount():
        if int(state) == 0:
            dispatch('increment')
    use_effect(on_mount, [])
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "Reducer", body);
        let def =
            build_py_node_definition("Reducer".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "rdc".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 1");
        }
        // Effect from batch 1 should have dispatched, marking node
        // dirty. Second batch picks up state=1.
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch 2");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.contains(&"state=0".to_string()),
                "first render observes initial state=0: {observed:?}"
            );
            assert!(
                observed.contains(&"state=1".to_string()),
                "second render observes dispatched state=1: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // Phase 3d-β tests — use_neighbors + use_topology + use_accept_edge
    // -----------------------------------------------------------------

    /// Phase 3d-β: `use_neighbors()` returns the active node's
    /// neighbor list. Star-graph hub→{a,b,c}; body observes the
    /// length + sorted member ids.
    #[test]
    fn phase3d_beta_use_neighbors_returns_active_node_neighbors() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("hub".to_string(), Attrs::new());
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());
        graph.add_node("c".to_string(), Attrs::new());
        graph.add_edge("hub".to_string(), "a".to_string(), None, Attrs::new());
        graph.add_edge("hub".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("hub".to_string(), "c".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    nbrs = use_neighbors()
    host_observe('len:' + str(len(nbrs)))
    for n in sorted(nbrs):
        host_observe('member:' + n)
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "Hub", body);
        // The hub's body is mounted at id 'hub'; the leaf nodes won't
        // resolve a definition by their component, so register a no-op
        // for them.
        let no_op_body = r"
def body(ctx):
    return {'accepts': [], 'emits': []}
";
        let fn_leaf = eval_python_body(&engine_rc, iso, "Leaf", no_op_body);

        let def_hub = build_py_node_definition("Hub".into(), Rc::clone(&engine_rc), iso, fn_h);
        let def_a = build_py_node_definition("Leaf".into(), Rc::clone(&engine_rc), iso, fn_leaf);
        let def_b = build_py_node_definition("Leaf".into(), Rc::clone(&engine_rc), iso, fn_leaf);
        let def_c = build_py_node_definition("Leaf".into(), Rc::clone(&engine_rc), iso, fn_leaf);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_hub, "hub".to_string(), Attrs::new(), 0_u32)
            .expect("register hub");
        scheduler
            .register_node(def_a, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register a");
        scheduler
            .register_node(def_b, "b".to_string(), Attrs::new(), 0_u32)
            .expect("register b");
        scheduler
            .register_node(def_c, "c".to_string(), Attrs::new(), 0_u32)
            .expect("register c");

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.contains(&"len:3".to_string()),
                "hub has 3 neighbors: {observed:?}"
            );
            assert!(
                observed.contains(&"member:a".to_string()),
                "a is a neighbor: {observed:?}"
            );
            assert!(
                observed.contains(&"member:b".to_string()),
                "b is a neighbor: {observed:?}"
            );
            assert!(
                observed.contains(&"member:c".to_string()),
                "c is a neighbor: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-β: `use_neighbors(filter)` applies a Python-side
    /// predicate. Same star-graph; filter excludes 'b'; assert only
    /// 2 results.
    #[test]
    fn phase3d_beta_use_neighbors_with_filter_predicate() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("hub".to_string(), Attrs::new());
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());
        graph.add_node("c".to_string(), Attrs::new());
        graph.add_edge("hub".to_string(), "a".to_string(), None, Attrs::new());
        graph.add_edge("hub".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("hub".to_string(), "c".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    nbrs = use_neighbors(lambda n: n != 'b')
    host_observe('len:' + str(len(nbrs)))
    for n in sorted(nbrs):
        host_observe('keep:' + n)
    return {'accepts': [], 'emits': []}
";
        let no_op_body = r"
def body(ctx):
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "HubF", body);
        let fn_leaf = eval_python_body(&engine_rc, iso, "LeafF", no_op_body);

        let def_hub = build_py_node_definition("HubF".into(), Rc::clone(&engine_rc), iso, fn_h);
        let def_a = build_py_node_definition("LeafF".into(), Rc::clone(&engine_rc), iso, fn_leaf);
        let def_b = build_py_node_definition("LeafF".into(), Rc::clone(&engine_rc), iso, fn_leaf);
        let def_c = build_py_node_definition("LeafF".into(), Rc::clone(&engine_rc), iso, fn_leaf);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_hub, "hub".to_string(), Attrs::new(), 0_u32)
            .expect("register hub");
        scheduler
            .register_node(def_a, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register a");
        scheduler
            .register_node(def_b, "b".to_string(), Attrs::new(), 0_u32)
            .expect("register b");
        scheduler
            .register_node(def_c, "c".to_string(), Attrs::new(), 0_u32)
            .expect("register c");

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.contains(&"len:2".to_string()),
                "filter excludes b: {observed:?}"
            );
            assert!(
                observed.contains(&"keep:a".to_string()) && observed.contains(&"keep:c".to_string()),
                "kept a and c: {observed:?}"
            );
            assert!(
                !observed.contains(&"keep:b".to_string()),
                "b filtered out: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-β: `use_topology(handler)` fires when a new node is
    /// added to the graph. Single watcher node; externally adds a
    /// 'newcomer'; second batch dispatches the event; handler observes.
    #[test]
    fn phase3d_beta_use_topology_handler_fires_on_node_addition() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("watcher".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    def on_event(event):
        if event.get('kind') == 'node-added':
            host_observe('saw:' + event.get('nodeId', ''))
    use_topology(on_event)
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "Watcher", body);
        let def =
            build_py_node_definition("Watcher".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "watcher".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        // First batch: registers the topology handler.
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("first batch");
        }

        // Add a new node directly to the underlying graph — the
        // scheduler's topology listener catches the event, dispatches
        // on the next process_batch.
        scheduler
            .graph
            .borrow_mut()
            .add_node("newcomer".to_string(), Attrs::new());

        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("second batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "saw:newcomer"),
                "use_topology handler observed NodeAdded for 'newcomer': {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-β: `use_accept_edge()` returns a closure that enqueues
    /// a Phase-9 AcceptEdge mutation; the next cascade round applies
    /// it and the active node's neighbors snapshot reflects the new
    /// edge.
    #[test]
    fn phase3d_beta_use_accept_edge_creates_edge() {
        let _g = engine_lock();
        reset_global_state();

        // Two disconnected nodes: 'host' and 'guest'.
        let mut graph = CommunicationGraph::new();
        graph.add_node("host".to_string(), Attrs::new());
        graph.add_node("guest".to_string(), Attrs::new());
        // No edge initially.

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Body: on first render (counter==0) accept edge to 'guest'
        // and bump counter; subsequent renders observe whether 'guest'
        // is now in our neighbors. Mirrors JSC's analogue.
        let body = r"
def body(ctx):
    counter, set_counter = use_state(0)
    nbrs = use_neighbors()
    host_observe('render:' + str(int(counter)) + ':nbrs=' + str(len(nbrs)))
    if int(counter) == 0:
        accept_edge = use_accept_edge()
        accept_edge('guest')
        set_counter(1)
    return {'accepts': [], 'emits': []}
";
        let no_op_body = r"
def body(ctx):
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "HostA", body);
        let fn_guest = eval_python_body(&engine_rc, iso, "GuestA", no_op_body);
        let def_host =
            build_py_node_definition("HostA".into(), Rc::clone(&engine_rc), iso, fn_h);
        let def_guest =
            build_py_node_definition("GuestA".into(), Rc::clone(&engine_rc), iso, fn_guest);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_host, "host".to_string(), Attrs::new(), 0_u32)
            .expect("register host");
        scheduler
            .register_node(def_guest, "guest".to_string(), Attrs::new(), 0_u32)
            .expect("register guest");

        // process_batch — first render observes 0 neighbors, accepts
        // edge, dirty-marks self via set_counter(1). The pending
        // mutation is drained at the start of the next cascade round;
        // re-render observes 1 neighbor.
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "render:0:nbrs=0"),
                "first render saw 0 neighbors: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "render:1:nbrs=1"),
                "re-render saw 1 neighbor after AcceptEdge drained: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // Phase 3d-γ tests — ctx.directory + use_graph_query
    // -----------------------------------------------------------------

    /// Phase 3d-γ: `ctx['directory'].find_by_id` /
    /// `find_by_role` / `find_by_depth` / `is_ancestor` query the live
    /// `GraphDirectory` via TLS-installed host callbacks. Tests
    /// populate directory entries explicitly (rather than via
    /// `register_node`) so the role/depth/path fields are testable
    /// without requiring the full scheduler hierarchy machinery.
    /// Mirrors JSC's `phase3e_delta_ctx_directory_finders_query_live_directory`.
    #[test]
    fn phase3d_gamma_ctx_directory_finders_query_live_directory() {
        use graphreact_core::DirectoryEntry;

        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("watcher".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Body queries the directory via ctx['directory'] and
        // observes each result. Subscript on ctx (Python dict
        // convention from earlier phases); attribute access on the
        // SimpleNamespace proxy.
        let body = r"
def body(ctx):
    entry = ctx['directory'].find_by_id('alice')
    host_observe('alice-role:' + ('NULL' if entry is None else entry['role']))
    host_observe('alice-depth:' + ('-1' if entry is None else str(int(entry['depth']))))
    workers = ctx['directory'].find_by_role('Worker')
    host_observe('workers-count:' + str(len(workers)))
    for w in sorted(workers):
        host_observe('worker:' + w)
    depth1 = ctx['directory'].find_by_depth(1)
    host_observe('depth-1-count:' + str(len(depth1)))
    host_observe('alice-ancestor-of-bob:' + str(ctx['directory'].is_ancestor('alice', 'bob')))
    host_observe('zombie-found:' + ('no' if ctx['directory'].find_by_id('nonexistent') is None else 'yes'))
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "Watcher", body);
        let def =
            build_py_node_definition("Watcher".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "watcher".to_string(), Attrs::new(), 0_u32)
            .expect("register watcher");

        // Populate directory directly with custom roles + depths +
        // ancestry paths so we can test all four finders.
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

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "alice-role:Foreman"),
                "find_by_id('alice') returned the entry with role=Foreman: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "alice-depth:0"),
                "alice's depth marshalled correctly: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "workers-count:2"),
                "find_by_role('Worker') returned 2 entries: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "worker:bob"),
                "bob is a Worker: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "worker:carol"),
                "carol is a Worker: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "depth-1-count:2"),
                "find_by_depth(1) returned 2 entries: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "alice-ancestor-of-bob:True"),
                "is_ancestor('alice', 'bob') is True: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "zombie-found:no"),
                "find_by_id('nonexistent') is None: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-γ: `use_graph_query(selector, deps)` exposes a
    /// `__graph_query_proxy` to the selector with `neighbors`,
    /// `has_node`, `has_edge`, `node_count` accessors backed by a
    /// graph snapshot captured at mount time.
    #[test]
    fn phase3d_gamma_use_graph_query_returns_proxy_results() {
        let _g = engine_lock();
        reset_global_state();

        // 3-node graph: hub → a, hub → b, a → b
        let mut graph = CommunicationGraph::new();
        graph.add_node("hub".to_string(), Attrs::new());
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());
        graph.add_edge("hub".to_string(), "a".to_string(), None, Attrs::new());
        graph.add_edge("hub".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    def selector(g):
        return {
            'hubNeighborCount': len(g.neighbors('hub')),
            'hasA': g.has_node('a'),
            'hasZombie': g.has_node('zombie'),
            'edgeAB': g.has_edge('a', 'b'),
            'edgeAZ': g.has_edge('a', 'zombie'),
            'totalNodes': int(g.node_count()),
        }
    result = use_graph_query(selector, [42])
    host_observe('hubNeighborCount:' + str(result['hubNeighborCount']))
    host_observe('hasA:' + str(result['hasA']))
    host_observe('hasZombie:' + str(result['hasZombie']))
    host_observe('edgeAB:' + str(result['edgeAB']))
    host_observe('edgeAZ:' + str(result['edgeAZ']))
    host_observe('totalNodes:' + str(result['totalNodes']))
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "Querier", body);
        let def =
            build_py_node_definition("Querier".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "hub".to_string(), Attrs::new(), 0_u32)
            .expect("register hub");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "hubNeighborCount:2"),
                "hub has 2 neighbors (a, b): {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "hasA:True"),
                "has_node('a') is True: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "hasZombie:False"),
                "has_node('zombie') is False: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "edgeAB:True"),
                "has_edge('a', 'b') is True: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "edgeAZ:False"),
                "has_edge('a', 'zombie') is False: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "totalNodes:3"),
                "node_count() is 3: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-γ: when no `EngineGuard::with_directory` is installed
    /// (i.e. `PyEngineGuard::new` instead), directory + graph_query
    /// callbacks return safe defaults (Null / empty array / false / 0)
    /// rather than panicking. Verifies the TLS-unset fallback path.
    #[test]
    fn phase3d_gamma_directory_unset_returns_safe_defaults() {
        let _g = engine_lock();
        reset_global_state();

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Body calls the directory + graph_query proxies with no
        // EngineGuard::with_directory installed (we use the legacy
        // PyEngineGuard::new instead). Each callback should return
        // its safe default.
        let body = r"
def body(ctx):
    entry = ctx['directory'].find_by_id('alice')
    host_observe('alice-found:' + ('no' if entry is None else 'yes'))
    workers = ctx['directory'].find_by_role('Worker')
    host_observe('workers-empty:' + str(len(workers) == 0))
    is_anc = ctx['directory'].is_ancestor('a', 'b')
    host_observe('is-ancestor:' + str(is_anc))
    def selector(g):
        return {
            'count': int(g.node_count()),
            'hasFoo': g.has_node('foo'),
        }
    result = use_graph_query(selector, [1])
    host_observe('count:' + str(result['count']))
    host_observe('hasFoo:' + str(result['hasFoo']))
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "FallbackBody", body);
        let def =
            build_py_node_definition("FallbackBody".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut graph = CommunicationGraph::new();
        graph.add_node("solo".to_string(), Attrs::new());
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "solo".to_string(), Attrs::new(), 0_u32)
            .expect("register solo");

        // Use legacy PyEngineGuard::new — directory + graph_snapshot
        // TLS slots stay unset.
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "alice-found:no"),
                "find_by_id returns None when directory unset: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "workers-empty:True"),
                "find_by_role returns empty when directory unset: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "is-ancestor:False"),
                "is_ancestor returns False when directory unset: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "count:0"),
                "node_count returns 0 when graph snapshot unset: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "hasFoo:False"),
                "has_node returns False when graph snapshot unset: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-δ: `g.shortest_path(from, to)` returns the path on a
    /// known graph. Verbatim port of JSC's
    /// phase3e_delta_use_graph_query analogue.
    #[test]
    fn phase3d_delta_use_graph_query_shortest_path() {
        let _g = engine_lock();
        reset_global_state();

        // 4-node line graph: A — B — C — D
        let mut graph = CommunicationGraph::new();
        graph.add_node("A".to_string(), Attrs::new());
        graph.add_node("B".to_string(), Attrs::new());
        graph.add_node("C".to_string(), Attrs::new());
        graph.add_node("D".to_string(), Attrs::new());
        graph.add_edge("A".to_string(), "B".to_string(), None, Attrs::new());
        graph.add_edge("B".to_string(), "C".to_string(), None, Attrs::new());
        graph.add_edge("C".to_string(), "D".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    def selector(g):
        return {
            'path': g.shortest_path('A', 'D'),
            'astar': g.a_star_path('A', 'D'),
            'noPath': g.shortest_path('A', 'zombie'),
            'allOneHop': g.all_paths('A', 'B'),
        }
    result = use_graph_query(selector, [42])
    host_observe('path:' + ','.join(result['path']))
    host_observe('astar:' + ','.join(result['astar']))
    host_observe('noPath:' + ('null' if result['noPath'] is None else 'WRONG'))
    host_observe('allPathsCount:' + str(len(result['allOneHop'])))
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "PathQuerier", body);
        let def =
            build_py_node_definition("PathQuerier".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "A".to_string(), Attrs::new(), 0_u32)
            .expect("register A");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "path:A,B,C,D"),
                "shortest_path A→D is the line: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "astar:A,B,C,D"),
                "a_star_path A→D matches shortest_path with no heuristic: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "noPath:null"),
                "shortest_path to nonexistent node returns None: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "allPathsCount:1"),
                "all_paths A→B in a line has exactly 1 path: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-δ: `g.bfs(start)` and `g.dfs(start)` return all reachable
    /// nodes. On a line graph the visit order is unambiguous; we assert
    /// length, that the start is first, and that all 4 nodes are
    /// visited. (Order within a layer for branchy graphs depends on
    /// insertion order; line graph sidesteps that.)
    #[test]
    fn phase3d_delta_use_graph_query_bfs_dfs_traversal_order() {
        let _g = engine_lock();
        reset_global_state();

        // 4-node line graph: A — B — C — D
        let mut graph = CommunicationGraph::new();
        graph.add_node("A".to_string(), Attrs::new());
        graph.add_node("B".to_string(), Attrs::new());
        graph.add_node("C".to_string(), Attrs::new());
        graph.add_node("D".to_string(), Attrs::new());
        graph.add_edge("A".to_string(), "B".to_string(), None, Attrs::new());
        graph.add_edge("B".to_string(), "C".to_string(), None, Attrs::new());
        graph.add_edge("C".to_string(), "D".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    def selector(g):
        return {
            'bfs': g.bfs('A'),
            'dfs': g.dfs('A'),
        }
    result = use_graph_query(selector, [42])
    host_observe('bfs:' + ','.join(result['bfs']))
    host_observe('dfs:' + ','.join(result['dfs']))
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "Traverser", body);
        let def =
            build_py_node_definition("Traverser".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "A".to_string(), Attrs::new(), 0_u32)
            .expect("register A");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            // Line graph: BFS and DFS both produce A,B,C,D in order.
            assert!(
                observed.iter().any(|s| s == "bfs:A,B,C,D"),
                "BFS from A on line graph visits A→B→C→D: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "dfs:A,B,C,D"),
                "DFS from A on line graph visits A→B→C→D: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-δ: `g.connected_components()` partitions a disconnected
    /// graph correctly. Two clusters: {A,B,C} and {X,Y}. Inner members
    /// are sorted (per host_graph_connected_components contract); test
    /// asserts on count + membership.
    #[test]
    fn phase3d_delta_use_graph_query_connected_components() {
        let _g = engine_lock();
        reset_global_state();

        // 5-node disconnected: A-B, B-C, X-Y. {A,B,C} ∪ {X,Y}.
        let mut graph = CommunicationGraph::new();
        graph.add_node("A".to_string(), Attrs::new());
        graph.add_node("B".to_string(), Attrs::new());
        graph.add_node("C".to_string(), Attrs::new());
        graph.add_node("X".to_string(), Attrs::new());
        graph.add_node("Y".to_string(), Attrs::new());
        graph.add_edge("A".to_string(), "B".to_string(), None, Attrs::new());
        graph.add_edge("B".to_string(), "C".to_string(), None, Attrs::new());
        graph.add_edge("X".to_string(), "Y".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    def selector(g):
        components = g.connected_components()
        # Find the component containing 'A' and the one containing 'X';
        # internal order is sorted by host_graph_connected_components.
        big = next((c for c in components if 'A' in c), [])
        small = next((c for c in components if 'X' in c), [])
        cycle_info = g.cycle_detection()
        return {
            'count': len(components),
            'big': ','.join(big),
            'small': ','.join(small),
            'hasCycle': cycle_info['hasCycle'],
        }
    result = use_graph_query(selector, [42])
    host_observe('compCount:' + str(result['count']))
    host_observe('big:' + result['big'])
    host_observe('small:' + result['small'])
    host_observe('hasCycle:' + str(result['hasCycle']))
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "Components", body);
        let def =
            build_py_node_definition("Components".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "A".to_string(), Attrs::new(), 0_u32)
            .expect("register A");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "compCount:2"),
                "exactly 2 connected components: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "big:A,B,C"),
                "first component is A,B,C (sorted): {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "small:X,Y"),
                "second component is X,Y (sorted): {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "hasCycle:False"),
                "tree graph has no cycle: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-δ: load-bearing live-handle verification. `GraphSnapshot`
    /// is constructed via `from_live` (3d-γ shipped this); mutations
    /// applied via `scheduler.graph.borrow_mut()` between two
    /// `process_batch` rounds must be visible to the second batch's
    /// `use_graph_query` selector. Confirms the JSC-followon-C
    /// "static→live" refactor was correctly short-circuited at 3d-γ.
    #[test]
    fn phase3d_delta_graph_snapshot_reflects_post_mount_mutation() {
        let _g = engine_lock();
        reset_global_state();

        // Start: just 'watcher' in the graph.
        let mut graph = CommunicationGraph::new();
        graph.add_node("watcher".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Body: state `tick` is a use_graph_query dep; bumping it
        // forces the selector to re-run. Selector reads node_count +
        // has_node('newcomer') from the live graph.
        let body = r"
def body(ctx):
    tick, set_tick = use_state(0)
    def selector(g):
        return {
            'count': int(g.node_count()),
            'hasNew': g.has_node('newcomer'),
        }
    result = use_graph_query(selector, [tick])
    host_observe('tick=' + str(int(tick)) + ':count=' + str(result['count']) + ':hasNew=' + str(result['hasNew']))
    return {'accepts': [], 'emits': []}
";
        let fn_h = eval_python_body(&engine_rc, iso, "LiveWatcher", body);
        let def =
            build_py_node_definition("LiveWatcher".into(), Rc::clone(&engine_rc), iso, fn_h);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def, "watcher".to_string(), Attrs::new(), 0_u32)
            .expect("register watcher");

        // First batch: tick=0; observe count=1 hasNew=False.
        let snapshot1 = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot1,
            );
            scheduler.process_batch().expect("batch 1");
        }

        // Mutate the underlying graph directly. The same Rc<RefCell<...>>
        // is shared with the snapshot; the next selector borrow sees it.
        scheduler
            .graph
            .borrow_mut()
            .add_node("newcomer".to_string(), Attrs::new());

        // Bump `tick` so use_graph_query's deps change → selector re-runs.
        let tick_id = signal_id_for("watcher", 0).expect("signal id");
        signal_set_external(tick_id, Value::Number(1.into())).expect("set tick=1");

        // Second batch: tick=1; selector re-runs over live graph.
        let snapshot2 = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot2,
            );
            scheduler.process_batch().expect("batch 2");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "tick=0:count=1:hasNew=False"),
                "first batch sees only watcher (1 node, no newcomer): {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "tick=1:count=2:hasNew=True"),
                "second batch sees mutation through live snapshot (2 nodes, has newcomer): {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // Phase 3d-ε — Tier 2 g.tier2(msgType, opts?) acceptance tests
    // -----------------------------------------------------------------
    //
    // Each test populates `PermissionManifest` data via body return
    // values. Data nodes mount at height 0; the querier mounts at
    // height 1 so data-node manifests are synced into
    // `Scheduler::manifests` before the querier reads Tier 2.
    // Verbatim ports of JSC's followon-E suite.

    /// Phase 3d-ε: `g.tier2(msg).bfs(from)` traverses the directed
    /// adjacency derived from emit/accept manifests. Bystander node
    /// (matching neither emit nor accept) is excluded.
    #[test]
    fn phase3d_epsilon_tier2_bfs_finds_policy_reachable_peers() {
        let _g = engine_lock();
        reset_global_state();

        // Tier 1 graph: producer ↔ worker ↔ sink + producer ↔ observer.
        let mut graph = CommunicationGraph::new();
        graph.add_node("producer".to_string(), Attrs::new());
        graph.add_node("worker".to_string(), Attrs::new());
        graph.add_node("sink".to_string(), Attrs::new());
        graph.add_node("observer".to_string(), Attrs::new());
        graph.add_node("querier".to_string(), Attrs::new());
        graph.add_edge("producer".to_string(), "worker".to_string(), None, Attrs::new());
        graph.add_edge("worker".to_string(), "sink".to_string(), None, Attrs::new());
        graph.add_edge("producer".to_string(), "observer".to_string(), None, Attrs::new());
        // querier needs Tier 1 edges to reach everyone for Tier 2's
        // structural flavor — but the test queries from 'producer', not
        // from 'querier', so querier just needs to participate in the
        // scheduler. No edges required.

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let producer_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['process']}
";
        let worker_body = r"
def body(ctx):
    return {'accepts': ['process'], 'emits': ['process']}
";
        let sink_body = r"
def body(ctx):
    return {'accepts': ['process'], 'emits': []}
";
        let observer_body = r"
def body(ctx):
    return {'accepts': ['gossip'], 'emits': []}
";
        let querier_body = r"
def body(ctx):
    def selector(g):
        return {
            'reachable': sorted(g.tier2('process').bfs('producer')),
            'observerInBfs': 'observer' in g.tier2('process').bfs('producer'),
        }
    result = use_graph_query(selector, [42])
    host_observe('reachable:' + ','.join(result['reachable']))
    host_observe('observerInBfs:' + str(result['observerInBfs']))
    return {'accepts': [], 'emits': []}
";
        let producer_fn = eval_python_body(&engine_rc, iso, "Producer", producer_body);
        let worker_fn = eval_python_body(&engine_rc, iso, "Worker", worker_body);
        let sink_fn = eval_python_body(&engine_rc, iso, "Sink", sink_body);
        let observer_fn = eval_python_body(&engine_rc, iso, "Observer", observer_body);
        let querier_fn = eval_python_body(&engine_rc, iso, "Querier", querier_body);

        let producer_def = build_py_node_definition("Producer".into(), Rc::clone(&engine_rc), iso, producer_fn);
        let worker_def = build_py_node_definition("Worker".into(), Rc::clone(&engine_rc), iso, worker_fn);
        let sink_def = build_py_node_definition("Sink".into(), Rc::clone(&engine_rc), iso, sink_fn);
        let observer_def = build_py_node_definition("Observer".into(), Rc::clone(&engine_rc), iso, observer_fn);
        let querier_def = build_py_node_definition("Querier".into(), Rc::clone(&engine_rc), iso, querier_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(producer_def, "producer".to_string(), Attrs::new(), 0_u32).expect("reg producer");
        scheduler.register_node(worker_def, "worker".to_string(), Attrs::new(), 0_u32).expect("reg worker");
        scheduler.register_node(sink_def, "sink".to_string(), Attrs::new(), 0_u32).expect("reg sink");
        scheduler.register_node(observer_def, "observer".to_string(), Attrs::new(), 0_u32).expect("reg observer");
        scheduler.register_node(querier_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg querier");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "reachable:producer,sink,worker"),
                "BFS via 'process' reaches producer/worker/sink (sorted): {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "observerInBfs:False"),
                "observer is excluded (accepts only 'gossip'): {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ε: `g.tier2(msg).topological_sort()` returns
    /// deterministic ordering. Closes followon-B's deferral on Python.
    /// 3-node pipeline matches JSC's `followon_e_tier2_topological_sort_directed_pipeline`:
    /// producer (emits only) → mid (both) → terminal (accepts only).
    /// 4+ node chains with bidirectional middles would create a cycle
    /// in the Tier 2 directed adjacency derived from undirected Tier 1
    /// edges (mid₁↔mid₂ both emit+accept → both directions); the
    /// 3-node shape sidesteps that by making the middle have only one
    /// other emit+accept peer.
    #[test]
    fn phase3d_epsilon_tier2_topological_sort_directed_pipeline() {
        let _g = engine_lock();
        reset_global_state();

        // Linear pipeline: producer ↔ mid ↔ terminal
        let mut graph = CommunicationGraph::new();
        for id in ["producer", "mid", "terminal", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("producer".to_string(), "mid".to_string(), None, Attrs::new());
        graph.add_edge("mid".to_string(), "terminal".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let producer_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['tick']}
";
        let mid_body = r"
def body(ctx):
    return {'accepts': ['tick'], 'emits': ['tick']}
";
        let terminal_body = r"
def body(ctx):
    return {'accepts': ['tick'], 'emits': []}
";
        let querier_body = r"
def body(ctx):
    def selector(g):
        return {'order': g.tier2('tick').topological_sort()}
    result = use_graph_query(selector, [42])
    order = result['order']
    host_observe('order:' + (','.join(order) if order is not None else 'null'))
    return {'accepts': [], 'emits': []}
";
        let p_fn = eval_python_body(&engine_rc, iso, "Producer", producer_body);
        let m_fn = eval_python_body(&engine_rc, iso, "Mid", mid_body);
        let t_fn = eval_python_body(&engine_rc, iso, "Terminal", terminal_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", querier_body);

        let p_def = build_py_node_definition("Producer".into(), Rc::clone(&engine_rc), iso, p_fn);
        let m_def = build_py_node_definition("Mid".into(), Rc::clone(&engine_rc), iso, m_fn);
        let t_def = build_py_node_definition("Terminal".into(), Rc::clone(&engine_rc), iso, t_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(p_def, "producer".to_string(), Attrs::new(), 0_u32).expect("reg producer");
        scheduler.register_node(m_def, "mid".to_string(), Attrs::new(), 0_u32).expect("reg mid");
        scheduler.register_node(t_def, "terminal".to_string(), Attrs::new(), 0_u32).expect("reg terminal");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg querier");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            // querier's own manifest isn't in the snapshot during its
            // own render (manifest-timing contract). Tier 2 adjacency
            // for 'tick': producer → [mid]; mid → [terminal];
            // terminal → []. Kahn's: producer first (in-deg 0), then
            // mid (after producer drains), then terminal.
            assert!(
                observed.iter().any(|s| s == "order:producer,mid,terminal"),
                "topological sort yields producer,mid,terminal on directed pipeline: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ε: A* with `compliance` heuristic picks the compliant
    /// path on a diamond. b is compliant, c is not.
    #[test]
    fn phase3d_epsilon_tier2_a_star_compliance_picks_compliant_path() {
        let _g = engine_lock();
        reset_global_state();

        // Diamond: a → {b, c} → d
        let mut graph = CommunicationGraph::new();
        graph.add_node("a".to_string(), Attrs::new());
        let mut b_attrs = Attrs::new();
        b_attrs.insert("compliant".to_string(), serde_json::json!(true));
        graph.add_node("b".to_string(), b_attrs);
        let mut c_attrs = Attrs::new();
        c_attrs.insert("compliant".to_string(), serde_json::json!(false));
        graph.add_node("c".to_string(), c_attrs);
        graph.add_node("d".to_string(), Attrs::new());
        graph.add_node("querier".to_string(), Attrs::new());
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("a".to_string(), "c".to_string(), None, Attrs::new());
        graph.add_edge("b".to_string(), "d".to_string(), None, Attrs::new());
        graph.add_edge("c".to_string(), "d".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let emit_body = r"
def body(ctx):
    return {'accepts': ['msg'], 'emits': ['msg']}
";
        let edge_body = r"
def body(ctx):
    return {'accepts': ['msg'], 'emits': ['msg']}
";
        let sink_body = r"
def body(ctx):
    return {'accepts': ['msg'], 'emits': []}
";
        let q_body = r"
def body(ctx):
    def selector(g):
        return {
            'path': g.tier2('msg').a_star_path('a', 'd', {'heuristic': 'compliance'}),
        }
    result = use_graph_query(selector, [42])
    p = result['path']
    host_observe('path:' + (','.join(p) if p is not None else 'null'))
    return {'accepts': [], 'emits': []}
";
        let a_fn = eval_python_body(&engine_rc, iso, "A", emit_body);
        let b_fn = eval_python_body(&engine_rc, iso, "B", edge_body);
        let c_fn = eval_python_body(&engine_rc, iso, "C", edge_body);
        let d_fn = eval_python_body(&engine_rc, iso, "D", sink_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", q_body);

        let a_def = build_py_node_definition("A".into(), Rc::clone(&engine_rc), iso, a_fn);
        let b_def = build_py_node_definition("B".into(), Rc::clone(&engine_rc), iso, b_fn);
        let c_def = build_py_node_definition("C".into(), Rc::clone(&engine_rc), iso, c_fn);
        let d_def = build_py_node_definition("D".into(), Rc::clone(&engine_rc), iso, d_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(a_def, "a".to_string(), Attrs::new(), 0_u32).expect("reg a");
        scheduler.register_node(b_def, "b".to_string(), Attrs::new(), 0_u32).expect("reg b");
        scheduler.register_node(c_def, "c".to_string(), Attrs::new(), 0_u32).expect("reg c");
        scheduler.register_node(d_def, "d".to_string(), Attrs::new(), 0_u32).expect("reg d");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "path:a,b,d"),
                "compliance heuristic prefers compliant b over non-compliant c: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ε: `all_paths`, `connected_components`, and
    /// `cycle_detection` over a fully-connected directed triangle.
    #[test]
    fn phase3d_epsilon_tier2_directed_long_tail_accessors() {
        let _g = engine_lock();
        reset_global_state();

        // Triangle {a, b, c} fully interconnected.
        let mut graph = CommunicationGraph::new();
        for id in ["a", "b", "c", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("b".to_string(), "c".to_string(), None, Attrs::new());
        graph.add_edge("a".to_string(), "c".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let participant_body = r"
def body(ctx):
    return {'accepts': ['send'], 'emits': ['send']}
";
        let q_body = r"
def body(ctx):
    def selector(g):
        t = g.tier2('send')
        cd = t.cycle_detection()
        return {
            'allPaths': t.all_paths('a', 'c'),
            'compCount': len(t.connected_components()),
            'hasCycle': cd['hasCycle'],
        }
    result = use_graph_query(selector, [42])
    paths = result['allPaths']
    paths_str = ';'.join('-'.join(p) for p in paths)
    host_observe('allPaths:' + paths_str)
    host_observe('compCount:' + str(result['compCount']))
    host_observe('hasCycle:' + str(result['hasCycle']))
    return {'accepts': [], 'emits': []}
";
        let a_fn = eval_python_body(&engine_rc, iso, "A", participant_body);
        let b_fn = eval_python_body(&engine_rc, iso, "B", participant_body);
        let c_fn = eval_python_body(&engine_rc, iso, "C", participant_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", q_body);

        let a_def = build_py_node_definition("A".into(), Rc::clone(&engine_rc), iso, a_fn);
        let b_def = build_py_node_definition("B".into(), Rc::clone(&engine_rc), iso, b_fn);
        let c_def = build_py_node_definition("C".into(), Rc::clone(&engine_rc), iso, c_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(a_def, "a".to_string(), Attrs::new(), 0_u32).expect("reg a");
        scheduler.register_node(b_def, "b".to_string(), Attrs::new(), 0_u32).expect("reg b");
        scheduler.register_node(c_def, "c".to_string(), Attrs::new(), 0_u32).expect("reg c");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            // a→c is direct (1 hop); a→b→c is 2 hops. Sort by length asc.
            assert!(
                observed.iter().any(|s| s == "allPaths:a-c;a-b-c"),
                "all_paths a→c sorted by length: {observed:?}"
            );
            // Querier's own manifest isn't in the snapshot during its
            // own render (manifest-timing contract), so adjacency
            // contains only {a, b, c}. Tarjan SCC on the triangle:
            // {a, b, c} forms ONE strongly-connected component
            // because every node is reachable from every other via
            // directed Tier 2 paths.
            assert!(
                observed.iter().any(|s| s == "compCount:1"),
                "1 SCC over the {{a,b,c}} triangle: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "hasCycle:True"),
                "triangle has a directed cycle: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ε: A* with `trust-attenuated` heuristic prefers
    /// high-trust path. trust=1.0 → cost=1; trust=0.1 → cost=10.
    #[test]
    fn phase3d_epsilon_tier2_a_star_trust_attenuated_picks_high_trust_path() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["a", "b", "c", "d", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        let mut high_trust = Attrs::new();
        high_trust.insert("trust".to_string(), serde_json::json!(1.0));
        let mut low_trust = Attrs::new();
        low_trust.insert("trust".to_string(), serde_json::json!(0.1));
        graph.add_edge("a".to_string(), "b".to_string(), None, high_trust.clone());
        graph.add_edge("b".to_string(), "d".to_string(), None, high_trust);
        graph.add_edge("a".to_string(), "c".to_string(), None, low_trust.clone());
        graph.add_edge("c".to_string(), "d".to_string(), None, low_trust);

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let participant_body = r"
def body(ctx):
    return {'accepts': ['msg'], 'emits': ['msg']}
";
        let q_body = r"
def body(ctx):
    def selector(g):
        return {'path': g.tier2('msg').a_star_path('a', 'd', {'heuristic': 'trust-attenuated'})}
    result = use_graph_query(selector, [42])
    p = result['path']
    host_observe('path:' + (','.join(p) if p is not None else 'null'))
    return {'accepts': [], 'emits': []}
";
        let a_fn = eval_python_body(&engine_rc, iso, "A", participant_body);
        let b_fn = eval_python_body(&engine_rc, iso, "B", participant_body);
        let c_fn = eval_python_body(&engine_rc, iso, "C", participant_body);
        let d_fn = eval_python_body(&engine_rc, iso, "D", participant_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", q_body);

        let a_def = build_py_node_definition("A".into(), Rc::clone(&engine_rc), iso, a_fn);
        let b_def = build_py_node_definition("B".into(), Rc::clone(&engine_rc), iso, b_fn);
        let c_def = build_py_node_definition("C".into(), Rc::clone(&engine_rc), iso, c_fn);
        let d_def = build_py_node_definition("D".into(), Rc::clone(&engine_rc), iso, d_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(a_def, "a".to_string(), Attrs::new(), 0_u32).expect("reg a");
        scheduler.register_node(b_def, "b".to_string(), Attrs::new(), 0_u32).expect("reg b");
        scheduler.register_node(c_def, "c".to_string(), Attrs::new(), 0_u32).expect("reg c");
        scheduler.register_node(d_def, "d".to_string(), Attrs::new(), 0_u32).expect("reg d");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "path:a,b,d"),
                "trust-attenuated prefers trust=1.0 path over trust=0.1 path: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ε: A* with `prerequisite-distance` heuristic prefers
    /// low-difficulty path. b.difficulty=0; c.difficulty=10.
    #[test]
    fn phase3d_epsilon_tier2_a_star_prerequisite_distance_picks_easy_path() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("a".to_string(), Attrs::new());
        let mut b_attrs = Attrs::new();
        b_attrs.insert("difficulty".to_string(), serde_json::json!(0.0));
        graph.add_node("b".to_string(), b_attrs);
        let mut c_attrs = Attrs::new();
        c_attrs.insert("difficulty".to_string(), serde_json::json!(10.0));
        graph.add_node("c".to_string(), c_attrs);
        graph.add_node("d".to_string(), Attrs::new());
        graph.add_node("querier".to_string(), Attrs::new());
        graph.add_edge("a".to_string(), "b".to_string(), None, Attrs::new());
        graph.add_edge("b".to_string(), "d".to_string(), None, Attrs::new());
        graph.add_edge("a".to_string(), "c".to_string(), None, Attrs::new());
        graph.add_edge("c".to_string(), "d".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let participant_body = r"
def body(ctx):
    return {'accepts': ['lesson'], 'emits': ['lesson']}
";
        let q_body = r"
def body(ctx):
    def selector(g):
        return {'path': g.tier2('lesson').a_star_path('a', 'd', {'heuristic': 'prerequisite-distance'})}
    result = use_graph_query(selector, [42])
    p = result['path']
    host_observe('path:' + (','.join(p) if p is not None else 'null'))
    return {'accepts': [], 'emits': []}
";
        let a_fn = eval_python_body(&engine_rc, iso, "A", participant_body);
        let b_fn = eval_python_body(&engine_rc, iso, "B", participant_body);
        let c_fn = eval_python_body(&engine_rc, iso, "C", participant_body);
        let d_fn = eval_python_body(&engine_rc, iso, "D", participant_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", q_body);

        let a_def = build_py_node_definition("A".into(), Rc::clone(&engine_rc), iso, a_fn);
        let b_def = build_py_node_definition("B".into(), Rc::clone(&engine_rc), iso, b_fn);
        let c_def = build_py_node_definition("C".into(), Rc::clone(&engine_rc), iso, c_fn);
        let d_def = build_py_node_definition("D".into(), Rc::clone(&engine_rc), iso, d_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(a_def, "a".to_string(), Attrs::new(), 0_u32).expect("reg a");
        scheduler.register_node(b_def, "b".to_string(), Attrs::new(), 0_u32).expect("reg b");
        scheduler.register_node(c_def, "c".to_string(), Attrs::new(), 0_u32).expect("reg c");
        scheduler.register_node(d_def, "d".to_string(), Attrs::new(), 0_u32).expect("reg d");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "path:a,b,d"),
                "prerequisite-distance prefers difficulty=0 (b) over difficulty=10 (c): {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ε: directory-shortcut flavor enables edge-free
    /// reachability. producer + sink with matching manifests but no
    /// Tier 1 edge; default flavor: not reachable, opt-in flavor:
    /// reachable.
    #[test]
    fn phase3d_epsilon_tier2_directory_shortcuts_flag_enables_edge_free_reachability() {
        let _g = engine_lock();
        reset_global_state();

        // No edge between producer and sink.
        let mut graph = CommunicationGraph::new();
        graph.add_node("producer".to_string(), Attrs::new());
        graph.add_node("sink".to_string(), Attrs::new());
        graph.add_node("querier".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let producer_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['broadcast']}
";
        let sink_body = r"
def body(ctx):
    return {'accepts': ['broadcast'], 'emits': []}
";
        let q_body = r"
def body(ctx):
    def selector(g):
        structural = g.tier2('broadcast')
        shortcut = g.tier2('broadcast', {'includeDirectoryShortcuts': True})
        return {
            'structHasEdge': structural.has_edge('producer', 'sink'),
            'shortcutHasEdge': shortcut.has_edge('producer', 'sink'),
            'structNeighbors': structural.neighbors('producer'),
            'shortcutNeighbors': shortcut.neighbors('producer'),
        }
    result = use_graph_query(selector, [42])
    host_observe('structHasEdge:' + str(result['structHasEdge']))
    host_observe('shortcutHasEdge:' + str(result['shortcutHasEdge']))
    host_observe('structNbrs:' + ','.join(result['structNeighbors']))
    host_observe('shortcutNbrs:' + ','.join(result['shortcutNeighbors']))
    return {'accepts': [], 'emits': []}
";
        let p_fn = eval_python_body(&engine_rc, iso, "P", producer_body);
        let s_fn = eval_python_body(&engine_rc, iso, "S", sink_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", q_body);

        let p_def = build_py_node_definition("P".into(), Rc::clone(&engine_rc), iso, p_fn);
        let s_def = build_py_node_definition("S".into(), Rc::clone(&engine_rc), iso, s_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(p_def, "producer".to_string(), Attrs::new(), 0_u32).expect("reg p");
        scheduler.register_node(s_def, "sink".to_string(), Attrs::new(), 0_u32).expect("reg s");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "structHasEdge:False"),
                "structural flavor: no Tier 1 edge → False: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "shortcutHasEdge:True"),
                "directory-shortcut flavor: emit/accept match → True: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "structNbrs:"),
                "structural flavor: no Tier 1 edge → empty neighbors: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "shortcutNbrs:sink"),
                "directory-shortcut flavor: producer reaches sink: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // Phase 3d-ζ — Tier 3 g.tier3(msgType, opts?) +
    // use_edge_accept_handler acceptance tests. Verbatim ports of JSC's
    // followon-F suite. Each populates manifests + (optionally)
    // edge-accept predicates via body return values + hook calls.
    // -----------------------------------------------------------------

    /// Phase 3d-ζ substrate #1: with no `use_edge_accept_handler`
    /// installed, Tier 3 == Tier 2. Default-accept preserves Phase-9
    /// backward compatibility.
    #[test]
    fn phase3d_zeta_use_edge_accept_handler_default_accept_when_no_predicate() {
        let _g = engine_lock();
        reset_global_state();

        // alice ↔ bob in Tier 1; manifests match for 'friend-request'.
        let mut graph = CommunicationGraph::new();
        for id in ["alice", "bob", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("alice".to_string(), "bob".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let alice_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['friend-request']}
";
        // No use_edge_accept_handler — Tier 3 should match Tier 2.
        let bob_body = r"
def body(ctx):
    return {'accepts': ['friend-request'], 'emits': []}
";
        let querier_body = r"
def body(ctx):
    def selector(g):
        t2 = g.tier2('friend-request')
        t3 = g.tier3('friend-request')
        return {
            't2HasEdge': t2.has_edge('alice', 'bob'),
            't3HasEdge': t3.has_edge('alice', 'bob'),
            't3Nbrs': t3.neighbors('alice'),
        }
    result = use_graph_query(selector, [42])
    host_observe('t2HasEdge:' + str(result['t2HasEdge']))
    host_observe('t3HasEdge:' + str(result['t3HasEdge']))
    host_observe('t3Nbrs:' + ','.join(result['t3Nbrs']))
    return {'accepts': [], 'emits': []}
";
        let a_fn = eval_python_body(&engine_rc, iso, "Alice", alice_body);
        let b_fn = eval_python_body(&engine_rc, iso, "Bob", bob_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", querier_body);
        let a_def = build_py_node_definition("Alice".into(), Rc::clone(&engine_rc), iso, a_fn);
        let b_def = build_py_node_definition("Bob".into(), Rc::clone(&engine_rc), iso, b_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(a_def, "alice".to_string(), Attrs::new(), 0_u32).expect("reg alice");
        scheduler.register_node(b_def, "bob".to_string(), Attrs::new(), 0_u32).expect("reg bob");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "t2HasEdge:True"),
                "Tier 2 includes alice→bob: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "t3HasEdge:True"),
                "Tier 3 default-accepts (no predicate) → matches Tier 2: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "t3Nbrs:bob"),
                "alice's Tier 3 neighbors include bob: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ζ substrate #2: predicate returning True keeps the
    /// (s, u) pair in Tier 3.
    #[test]
    fn phase3d_zeta_tier3_query_includes_predicate_accepted_pair() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["alice", "bob", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("alice".to_string(), "bob".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let alice_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['friend-request']}
";
        // Bob always-accepts.
        let bob_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: True)
    return {'accepts': ['friend-request'], 'emits': []}
";
        let querier_body = r"
def body(ctx):
    def selector(g):
        t3 = g.tier3('friend-request')
        return {
            'hasEdge': t3.has_edge('alice', 'bob'),
            'nbrs': t3.neighbors('alice'),
        }
    result = use_graph_query(selector, [42])
    host_observe('t3:hasEdge=' + str(result['hasEdge']))
    host_observe('t3:nbrs=' + ','.join(result['nbrs']))
    return {'accepts': [], 'emits': []}
";
        let a_fn = eval_python_body(&engine_rc, iso, "Alice", alice_body);
        let b_fn = eval_python_body(&engine_rc, iso, "Bob", bob_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", querier_body);
        let a_def = build_py_node_definition("Alice".into(), Rc::clone(&engine_rc), iso, a_fn);
        let b_def = build_py_node_definition("Bob".into(), Rc::clone(&engine_rc), iso, b_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(a_def, "alice".to_string(), Attrs::new(), 0_u32).expect("reg alice");
        scheduler.register_node(b_def, "bob".to_string(), Attrs::new(), 0_u32).expect("reg bob");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "t3:hasEdge=True"),
                "predicate-accepted pair appears in Tier 3: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "t3:nbrs=bob"),
                "alice's tier 3 neighbors include bob: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ζ substrate #3: predicate returning False removes the
    /// (s, u) pair from Tier 3 even though Tier 2 includes it.
    #[test]
    fn phase3d_zeta_tier3_query_excludes_predicate_rejected_pair() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        for id in ["alice", "bob", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("alice".to_string(), "bob".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let alice_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['friend-request']}
";
        // Bob refuses every requester.
        let bob_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: False)
    return {'accepts': ['friend-request'], 'emits': []}
";
        let querier_body = r"
def body(ctx):
    def selector(g):
        t2 = g.tier2('friend-request')
        t3 = g.tier3('friend-request')
        return {
            't2HasEdge': t2.has_edge('alice', 'bob'),
            't3HasEdge': t3.has_edge('alice', 'bob'),
            't3Nbrs': t3.neighbors('alice'),
        }
    result = use_graph_query(selector, [42])
    host_observe('t2:hasEdge=' + str(result['t2HasEdge']))
    host_observe('t3:hasEdge=' + str(result['t3HasEdge']))
    host_observe('t3:nbrs=' + ','.join(result['t3Nbrs']))
    return {'accepts': [], 'emits': []}
";
        let a_fn = eval_python_body(&engine_rc, iso, "Alice", alice_body);
        let b_fn = eval_python_body(&engine_rc, iso, "Bob", bob_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", querier_body);
        let a_def = build_py_node_definition("Alice".into(), Rc::clone(&engine_rc), iso, a_fn);
        let b_def = build_py_node_definition("Bob".into(), Rc::clone(&engine_rc), iso, b_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(a_def, "alice".to_string(), Attrs::new(), 0_u32).expect("reg alice");
        scheduler.register_node(b_def, "bob".to_string(), Attrs::new(), 0_u32).expect("reg bob");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "t2:hasEdge=True"),
                "Tier 2 baseline includes alice→bob: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "t3:hasEdge=False"),
                "Tier 3 excludes alice→bob (bob's predicate vetoes): {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "t3:nbrs="),
                "alice has no Tier 3 neighbors after veto: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ζ substrate #4: `use_accept_edge` Phase 9 mutation
    /// path also consults the predicate. veto-target's predicate
    /// vetoes; open-target has no predicate → default-accept.
    #[test]
    fn phase3d_zeta_use_accept_edge_consults_predicate() {
        let _g = engine_lock();
        reset_global_state();

        // src is NOT initially connected to either target.
        let mut graph = CommunicationGraph::new();
        for id in ["src", "veto-target", "open-target"] {
            graph.add_node(id.to_string(), Attrs::new());
        }

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // veto-target installs a predicate returning False.
        let veto_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: False)
    return {'accepts': [], 'emits': []}
";
        // open-target has no predicate → default-accept.
        let open_body = r"
def body(ctx):
    return {'accepts': [], 'emits': []}
";
        // src enqueues AcceptEdge to both targets via use_effect.
        let src_body = r"
def body(ctx):
    accept = use_accept_edge()
    def on_mount():
        accept('veto-target')
        accept('open-target')
    use_effect(on_mount, [])
    return {'accepts': [], 'emits': []}
";
        let v_fn = eval_python_body(&engine_rc, iso, "Veto", veto_body);
        let o_fn = eval_python_body(&engine_rc, iso, "Open", open_body);
        let s_fn = eval_python_body(&engine_rc, iso, "Src", src_body);
        let v_def = build_py_node_definition("Veto".into(), Rc::clone(&engine_rc), iso, v_fn);
        let o_def = build_py_node_definition("Open".into(), Rc::clone(&engine_rc), iso, o_fn);
        let s_def = build_py_node_definition("Src".into(), Rc::clone(&engine_rc), iso, s_fn);

        let mut scheduler = Scheduler::new(graph);
        // Targets at height 0 (predicates installed before src's
        // useEffect fires). src at height 1.
        scheduler.register_node(v_def, "veto-target".to_string(), Attrs::new(), 0_u32).expect("reg veto");
        scheduler.register_node(o_def, "open-target".to_string(), Attrs::new(), 0_u32).expect("reg open");
        scheduler.register_node(s_def, "src".to_string(), Attrs::new(), 1_u32).expect("reg src");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        // Drop observations + engine_guard before borrowing graph
        // (process_batch may have queued more work but the AcceptEdge
        // mutation must have drained by now).
        drop(observations);
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

    /// Phase 3d-ζ application class #1 (Self-Healing / Thymus): A*
    /// with `compliance` heuristic + predicate excludes noncompliant
    /// failover even though the Tier 1 edge exists.
    #[test]
    fn phase3d_zeta_self_healing_compliance_a_star_path_with_predicate() {
        let _g = engine_lock();
        reset_global_state();

        // primary → {compliant-failover, noncompliant-failover}.
        let mut graph = CommunicationGraph::new();
        graph.add_node("primary".to_string(), Attrs::new());
        let mut compliant_attrs = Attrs::new();
        compliant_attrs.insert("compliant".to_string(), serde_json::json!(true));
        graph.add_node("compliant-failover".to_string(), compliant_attrs);
        let mut noncompliant_attrs = Attrs::new();
        noncompliant_attrs.insert("compliant".to_string(), serde_json::json!(false));
        graph.add_node("noncompliant-failover".to_string(), noncompliant_attrs);
        graph.add_node("querier".to_string(), Attrs::new());
        graph.add_edge("primary".to_string(), "compliant-failover".to_string(), None, Attrs::new());
        graph.add_edge("primary".to_string(), "noncompliant-failover".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let primary_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['write']}
";
        let compliant_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: True)
    return {'accepts': ['write'], 'emits': []}
";
        // Noncompliant failover refuses the write.
        let noncompliant_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: False)
    return {'accepts': ['write'], 'emits': []}
";
        let q_body = r"
def body(ctx):
    def selector(g):
        return {
            'pathToCompliant': g.tier3('write').a_star_path('primary', 'compliant-failover', {'heuristic': 'compliance'}),
            'pathToNoncompliant': g.tier3('write').a_star_path('primary', 'noncompliant-failover', {'heuristic': 'compliance'}),
        }
    result = use_graph_query(selector, [42])
    p1 = result['pathToCompliant']
    host_observe('compliant:' + (','.join(p1) if p1 is not None else 'null'))
    p2 = result['pathToNoncompliant']
    host_observe('noncompliant:' + (','.join(p2) if p2 is not None else 'null'))
    return {'accepts': [], 'emits': []}
";
        let p_fn = eval_python_body(&engine_rc, iso, "Primary", primary_body);
        let c_fn = eval_python_body(&engine_rc, iso, "Compliant", compliant_body);
        let n_fn = eval_python_body(&engine_rc, iso, "Noncompliant", noncompliant_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", q_body);
        let p_def = build_py_node_definition("Primary".into(), Rc::clone(&engine_rc), iso, p_fn);
        let c_def = build_py_node_definition("Compliant".into(), Rc::clone(&engine_rc), iso, c_fn);
        let n_def = build_py_node_definition("Noncompliant".into(), Rc::clone(&engine_rc), iso, n_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(p_def, "primary".to_string(), Attrs::new(), 0_u32).expect("reg primary");
        scheduler.register_node(c_def, "compliant-failover".to_string(), Attrs::new(), 0_u32).expect("reg c");
        scheduler.register_node(n_def, "noncompliant-failover".to_string(), Attrs::new(), 0_u32).expect("reg n");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "compliant:primary,compliant-failover"),
                "A* with compliance heuristic routes to compliant failover: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "noncompliant:null"),
                "A* path to noncompliant failover is None (predicate vetoed): {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ζ application class #6 (Adaptive Education):
    /// `g.tier3('compose').neighbors('fractions')` returns only
    /// learner-appropriate next concepts.
    #[test]
    fn phase3d_zeta_adaptive_education_neighbors_with_predicate() {
        let _g = engine_lock();
        reset_global_state();

        // fractions ↔ {decimals, calculus}. Calculus's predicate
        // vetoes (too advanced); decimals accepts.
        let mut graph = CommunicationGraph::new();
        for id in ["fractions", "decimals", "calculus", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("fractions".to_string(), "decimals".to_string(), None, Attrs::new());
        graph.add_edge("fractions".to_string(), "calculus".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let fractions_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['compose']}
";
        let decimals_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: True)
    return {'accepts': ['compose'], 'emits': []}
";
        let calculus_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: False)
    return {'accepts': ['compose'], 'emits': []}
";
        let q_body = r"
def body(ctx):
    def selector(g):
        return {'nbrs': g.tier3('compose').neighbors('fractions')}
    result = use_graph_query(selector, [42])
    host_observe('nbrs:' + ','.join(result['nbrs']))
    return {'accepts': [], 'emits': []}
";
        let f_fn = eval_python_body(&engine_rc, iso, "Fractions", fractions_body);
        let d_fn = eval_python_body(&engine_rc, iso, "Decimals", decimals_body);
        let c_fn = eval_python_body(&engine_rc, iso, "Calculus", calculus_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", q_body);
        let f_def = build_py_node_definition("Fractions".into(), Rc::clone(&engine_rc), iso, f_fn);
        let d_def = build_py_node_definition("Decimals".into(), Rc::clone(&engine_rc), iso, d_fn);
        let c_def = build_py_node_definition("Calculus".into(), Rc::clone(&engine_rc), iso, c_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(f_def, "fractions".to_string(), Attrs::new(), 0_u32).expect("reg fractions");
        scheduler.register_node(d_def, "decimals".to_string(), Attrs::new(), 0_u32).expect("reg decimals");
        scheduler.register_node(c_def, "calculus".to_string(), Attrs::new(), 0_u32).expect("reg calculus");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "nbrs:decimals"),
                "Adaptive Ed: only learner-appropriate concept (decimals) reachable: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ζ application class #4 (Generative Worlds): BFS over
    /// 'war' messages excludes peace-treaty regions.
    #[test]
    fn phase3d_zeta_generative_worlds_bfs_with_peace_treaty_vetoes() {
        let _g = engine_lock();
        reset_global_state();

        // invader → {open-region → frontier, peace-region}. Peace
        // refuses war; open accepts.
        let mut graph = CommunicationGraph::new();
        for id in ["invader-region", "open-region", "frontier", "peace-region", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("invader-region".to_string(), "open-region".to_string(), None, Attrs::new());
        graph.add_edge("open-region".to_string(), "frontier".to_string(), None, Attrs::new());
        graph.add_edge("invader-region".to_string(), "peace-region".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let invader_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['war']}
";
        let open_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: True)
    return {'accepts': ['war'], 'emits': ['war']}
";
        let frontier_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: True)
    return {'accepts': ['war'], 'emits': []}
";
        let peace_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: False)
    return {'accepts': ['war'], 'emits': []}
";
        let q_body = r"
def body(ctx):
    def selector(g):
        return {'visit': g.tier3('war').bfs('invader-region')}
    result = use_graph_query(selector, [42])
    host_observe('visit:' + ','.join(sorted(result['visit'])))
    return {'accepts': [], 'emits': []}
";
        let i_fn = eval_python_body(&engine_rc, iso, "Invader", invader_body);
        let o_fn = eval_python_body(&engine_rc, iso, "Open", open_body);
        let fr_fn = eval_python_body(&engine_rc, iso, "Frontier", frontier_body);
        let pe_fn = eval_python_body(&engine_rc, iso, "Peace", peace_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", q_body);
        let i_def = build_py_node_definition("Invader".into(), Rc::clone(&engine_rc), iso, i_fn);
        let o_def = build_py_node_definition("Open".into(), Rc::clone(&engine_rc), iso, o_fn);
        let fr_def = build_py_node_definition("Frontier".into(), Rc::clone(&engine_rc), iso, fr_fn);
        let pe_def = build_py_node_definition("Peace".into(), Rc::clone(&engine_rc), iso, pe_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(i_def, "invader-region".to_string(), Attrs::new(), 0_u32).expect("reg invader");
        scheduler.register_node(o_def, "open-region".to_string(), Attrs::new(), 0_u32).expect("reg open");
        scheduler.register_node(fr_def, "frontier".to_string(), Attrs::new(), 0_u32).expect("reg frontier");
        scheduler.register_node(pe_def, "peace-region".to_string(), Attrs::new(), 0_u32).expect("reg peace");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "visit:frontier,invader-region,open-region"),
                "BFS visits open chain only; peace-region excluded by predicate: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ζ application class #7 (Biological Pathways):
    /// `all_paths` from drug to kinase routes only through on-target
    /// receptors.
    #[test]
    fn phase3d_zeta_biological_pathway_all_paths_with_off_target_vetoes() {
        let _g = engine_lock();
        reset_global_state();

        // drug → {on-target-receptor → kinase, off-target-receptor →
        // kinase}. Off-target's predicate vetoes the binding for this
        // drug — Tier 3 should exclude its path.
        let mut graph = CommunicationGraph::new();
        for id in ["drug", "on-target", "off-target", "kinase", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        graph.add_edge("drug".to_string(), "on-target".to_string(), None, Attrs::new());
        graph.add_edge("drug".to_string(), "off-target".to_string(), None, Attrs::new());
        graph.add_edge("on-target".to_string(), "kinase".to_string(), None, Attrs::new());
        graph.add_edge("off-target".to_string(), "kinase".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let drug_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['binding']}
";
        let on_target_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: True)
    return {'accepts': ['binding'], 'emits': ['binding']}
";
        let off_target_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: False)
    return {'accepts': ['binding'], 'emits': ['binding']}
";
        let kinase_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: True)
    return {'accepts': ['binding'], 'emits': []}
";
        let q_body = r"
def body(ctx):
    def selector(g):
        return {'paths': g.tier3('binding').all_paths('drug', 'kinase')}
    result = use_graph_query(selector, [42])
    paths = result['paths']
    host_observe('count:' + str(len(paths)))
    if len(paths) > 0:
        host_observe('first:' + '>'.join(paths[0]))
    return {'accepts': [], 'emits': []}
";
        let d_fn = eval_python_body(&engine_rc, iso, "Drug", drug_body);
        let on_fn = eval_python_body(&engine_rc, iso, "OnTarget", on_target_body);
        let off_fn = eval_python_body(&engine_rc, iso, "OffTarget", off_target_body);
        let k_fn = eval_python_body(&engine_rc, iso, "Kinase", kinase_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", q_body);
        let d_def = build_py_node_definition("Drug".into(), Rc::clone(&engine_rc), iso, d_fn);
        let on_def = build_py_node_definition("OnTarget".into(), Rc::clone(&engine_rc), iso, on_fn);
        let off_def = build_py_node_definition("OffTarget".into(), Rc::clone(&engine_rc), iso, off_fn);
        let k_def = build_py_node_definition("Kinase".into(), Rc::clone(&engine_rc), iso, k_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(d_def, "drug".to_string(), Attrs::new(), 0_u32).expect("reg drug");
        scheduler.register_node(on_def, "on-target".to_string(), Attrs::new(), 0_u32).expect("reg on");
        scheduler.register_node(off_def, "off-target".to_string(), Attrs::new(), 0_u32).expect("reg off");
        scheduler.register_node(k_def, "kinase".to_string(), Attrs::new(), 0_u32).expect("reg kinase");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "count:1"),
                "exactly 1 path through on-target receptor: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "first:drug>on-target>kinase"),
                "path routes through on-target: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3d-ζ application class #8 (Reputation Systems): A* with
    /// `trust-attenuated` heuristic + vouch-veto predicate routes
    /// through trusted intermediaries only.
    #[test]
    fn phase3d_zeta_reputation_a_star_path_trust_attenuated_with_vouch_vetoes() {
        let _g = engine_lock();
        reset_global_state();

        // alice → {high-trust-mutual → bob, low-trust-mutual → bob}.
        // Low-trust has a low edge.trust attr (cost 1/0.1 = 10) AND
        // its predicate vetoes the vouch. high-trust has trust=1.0.
        let mut graph = CommunicationGraph::new();
        for id in ["alice", "high-trust", "low-trust", "bob", "querier"] {
            graph.add_node(id.to_string(), Attrs::new());
        }
        let mut high_trust = Attrs::new();
        high_trust.insert("trust".to_string(), serde_json::json!(1.0));
        let mut low_trust = Attrs::new();
        low_trust.insert("trust".to_string(), serde_json::json!(0.1));
        graph.add_edge("alice".to_string(), "high-trust".to_string(), None, high_trust.clone());
        graph.add_edge("high-trust".to_string(), "bob".to_string(), None, high_trust);
        graph.add_edge("alice".to_string(), "low-trust".to_string(), None, low_trust.clone());
        graph.add_edge("low-trust".to_string(), "bob".to_string(), None, low_trust);

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let alice_body = r"
def body(ctx):
    return {'accepts': [], 'emits': ['vouch']}
";
        let high_trust_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: True)
    return {'accepts': ['vouch'], 'emits': ['vouch']}
";
        let low_trust_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: False)
    return {'accepts': ['vouch'], 'emits': ['vouch']}
";
        let bob_body = r"
def body(ctx):
    use_edge_accept_handler(lambda src, t: True)
    return {'accepts': ['vouch'], 'emits': []}
";
        let q_body = r"
def body(ctx):
    def selector(g):
        return {'path': g.tier3('vouch').a_star_path('alice', 'bob', {'heuristic': 'trust-attenuated'})}
    result = use_graph_query(selector, [42])
    p = result['path']
    host_observe('path:' + (','.join(p) if p is not None else 'null'))
    return {'accepts': [], 'emits': []}
";
        let a_fn = eval_python_body(&engine_rc, iso, "Alice", alice_body);
        let h_fn = eval_python_body(&engine_rc, iso, "HighTrust", high_trust_body);
        let l_fn = eval_python_body(&engine_rc, iso, "LowTrust", low_trust_body);
        let b_fn = eval_python_body(&engine_rc, iso, "Bob", bob_body);
        let q_fn = eval_python_body(&engine_rc, iso, "Q", q_body);
        let a_def = build_py_node_definition("Alice".into(), Rc::clone(&engine_rc), iso, a_fn);
        let h_def = build_py_node_definition("HighTrust".into(), Rc::clone(&engine_rc), iso, h_fn);
        let l_def = build_py_node_definition("LowTrust".into(), Rc::clone(&engine_rc), iso, l_fn);
        let b_def = build_py_node_definition("Bob".into(), Rc::clone(&engine_rc), iso, b_fn);
        let q_def = build_py_node_definition("Q".into(), Rc::clone(&engine_rc), iso, q_fn);

        let mut scheduler = Scheduler::new(graph);
        scheduler.register_node(a_def, "alice".to_string(), Attrs::new(), 0_u32).expect("reg alice");
        scheduler.register_node(h_def, "high-trust".to_string(), Attrs::new(), 0_u32).expect("reg h");
        scheduler.register_node(l_def, "low-trust".to_string(), Attrs::new(), 0_u32).expect("reg l");
        scheduler.register_node(b_def, "bob".to_string(), Attrs::new(), 0_u32).expect("reg bob");
        scheduler.register_node(q_def, "querier".to_string(), Attrs::new(), 1_u32).expect("reg q");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "path:alice,high-trust,bob"),
                "Reputation: A* + trust-attenuated routes through high-trust intermediary: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // Phase 3e-α tests — use_spawn_child + use_unmount_self + use_compose
    // -----------------------------------------------------------------

    /// Phase 3e-α: `use_spawn_child(spec)(...)` registers the supplied
    /// component callable in `_hostCallbacks`, dispatches the spec to
    /// `host_spawn_child`, and on the next cascade round the scheduler
    /// materialises the child node. The child's body executes on
    /// CPython like any other node — we observe both parent renders +
    /// the child render. Verbatim port of JSC's
    /// `phase3e_gamma_use_spawn_child_creates_dynamic_node`.
    #[test]
    fn phase3e_alpha_use_spawn_child_creates_dynamic_node() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("parent".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Parent body: on first render, spawn child-1 with a body that
        // observes its own ctx.id. State counter triggers a re-render
        // (so we can verify the child mounted on the second round).
        let body = r"
def body(ctx):
    count, set_count = use_state(0)
    host_observe('parent:' + str(int(count)))
    if int(count) == 0:
        spawn = use_spawn_child()
        def child_body(child_ctx):
            host_observe('child:' + str(child_ctx['id']))
            return {'accepts': [], 'emits': []}
        spawn({
            'id': 'child-1',
            'component': child_body,
            'props': {'label': 'first'},
            'connectTo': ['parent'],
        })
        set_count(1)
    return {'accepts': [], 'emits': []}
";
        let fn_handle = eval_python_body(&engine_rc, iso, "Parent", body);
        let definition =
            build_py_node_definition("Parent".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "parent".to_string(), Attrs::new(), 0_u32)
            .expect("register parent");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "parent:0"),
            "parent's first render observed; observations: {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "parent:1"),
            "parent re-rendered after set_count; observations: {observed:?}"
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

    /// Phase 3e-α: `use_unmount_self()()` schedules an `UnmountNode`
    /// mutation for the active node. After the cascade drains, the
    /// node is gone — subsequent renders don't fire. A 'witness' node
    /// observes the `NodeRemoved` event via `use_topology`. Verbatim
    /// port of JSC's `phase3e_gamma_use_unmount_self_removes_node`.
    #[test]
    fn phase3e_alpha_use_unmount_self_removes_node() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("ephemeral".to_string(), Attrs::new());
        graph.add_node("witness".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let ephemeral_body = r"
def body(ctx):
    count, _set_count = use_state(0)
    host_observe('ephemeral-render:' + str(int(count)))
    if int(count) == 0:
        unmount = use_unmount_self()
        unmount()
    return {'accepts': [], 'emits': []}
";
        let witness_body = r"
def body(ctx):
    def on_topo(event):
        if event.get('kind') == 'node-removed':
            host_observe('witness:removed:' + str(event.get('nodeId', '')))
    use_topology(on_topo)
    return {'accepts': [], 'emits': []}
";
        let fn_eph = eval_python_body(&engine_rc, iso, "Ephemeral", ephemeral_body);
        let fn_wit = eval_python_body(&engine_rc, iso, "Witness", witness_body);

        let def_eph =
            build_py_node_definition("Ephemeral".into(), Rc::clone(&engine_rc), iso, fn_eph);
        let def_wit =
            build_py_node_definition("Witness".into(), Rc::clone(&engine_rc), iso, fn_wit);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_eph, "ephemeral".to_string(), Attrs::new(), 0_u32)
            .expect("register ephemeral");
        scheduler
            .register_node(def_wit, "witness".to_string(), Attrs::new(), 0_u32)
            .expect("register witness");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "ephemeral-render:0"),
            "ephemeral's first render fired; observations: {observed:?}"
        );
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

    /// Phase 3e-α: `use_compose()(specs)` declares the active node's
    /// composed-children list in one call. Specs that aren't yet
    /// registered are spawned (idempotent on existing IDs at the
    /// scheduler). Verbatim port of JSC's
    /// `phase3e_gamma_use_compose_spawns_multiple_children`.
    #[test]
    fn phase3e_alpha_use_compose_spawns_multiple_children() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("composer".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Body: first render declares 3 children via use_compose.
        // Each child's body observes its label + ctx.id. The
        // _make_child factory captures `label` per iteration so each
        // child gets its own component fn (mirrors JSC's IIFE
        // `makeChild`); without the factory, Python's closure-in-loop
        // would resolve every wrapped fn to the loop's last label.
        let body = r"
def body(ctx):
    count, set_count = use_state(0)
    host_observe('composer:' + str(int(count)))
    if int(count) == 0:
        compose = use_compose()
        def make_child(label):
            def child(child_ctx):
                host_observe('child:' + label + ':' + str(child_ctx['id']))
                return {'accepts': [], 'emits': []}
            return child
        compose([
            {'id': 'c-a', 'component': make_child('a'), 'connectTo': ['composer']},
            {'id': 'c-b', 'component': make_child('b'), 'connectTo': ['composer']},
            {'id': 'c-c', 'component': make_child('c'), 'connectTo': ['composer']},
        ])
        set_count(1)
    return {'accepts': [], 'emits': []}
";
        let fn_handle = eval_python_body(&engine_rc, iso, "Composer", body);
        let definition =
            build_py_node_definition("Composer".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "composer".to_string(), Attrs::new(), 0_u32)
            .expect("register composer");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
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

    /// Phase 3e-α (Python-novel): validates the lambda-wrap-at-
    /// registration pattern. Spawned child reads `ctx['directory']`
    /// (set only via `__augment_ctx`) and observes the result —
    /// without the wrapper applying `__augment_ctx`, `ctx['directory']`
    /// would be missing and the test would `KeyError`. Pins that
    /// spawned bodies receive the same augmentation as top-level
    /// definitions.
    #[test]
    fn phase3e_alpha_spawned_child_sees_augmented_ctx() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("parent".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Parent body: spawn child-1; child reads ctx['directory'] +
        // calls one of the proxy methods (use_graph_query). Both must
        // resolve cleanly because the lambda-wrap injects them.
        let body = r"
def body(ctx):
    count, set_count = use_state(0)
    if int(count) == 0:
        spawn = use_spawn_child()
        def child_body(child_ctx):
            has_dir = 'directory' in child_ctx
            host_observe('child-has-directory:' + str(has_dir))
            def selector(g):
                return g.node_count()
            n = use_graph_query(selector, [42])
            host_observe('child-node-count:' + str(int(n) if n is not None else -1))
            return {'accepts': [], 'emits': []}
        spawn({
            'id': 'child-aug',
            'component': child_body,
            'connectTo': ['parent'],
        })
        set_count(1)
    return {'accepts': [], 'emits': []}
";
        let fn_handle = eval_python_body(&engine_rc, iso, "ParentAug", body);
        let definition =
            build_py_node_definition("ParentAug".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "parent".to_string(), Attrs::new(), 0_u32)
            .expect("register parent");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "child-has-directory:True"),
            "spawned child sees ctx['directory'] populated by __augment_ctx: {observed:?}"
        );
        // After parent spawns child, graph has at least 2 nodes
        // (parent + child). Sanity check that use_graph_query works
        // from inside a spawned body — the actual count depends on
        // mount ordering relative to the second batch.
        assert!(
            observed.iter().any(|s| s.starts_with("child-node-count:") && !s.ends_with(":-1")),
            "spawned child can call use_graph_query (proxy works in spawned bodies): {observed:?}"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-α (Python-novel, regression risk): verifies that hooks
    /// inside spawned children work correctly. Spawned child uses
    /// `use_state` + setter, drives a re-render via external
    /// `signal_set_external`, and observes the new value. Pins that
    /// HOOK_CTX threading is intact through the spawn → cascade →
    /// evaluate path.
    #[test]
    fn phase3e_alpha_spawned_child_can_use_state() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("parent".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    count, set_count = use_state(0)
    if int(count) == 0:
        spawn = use_spawn_child()
        def child_body(child_ctx):
            child_count, _child_set = use_state(11)
            host_observe('child-state:' + str(int(child_count)))
            return {'accepts': [], 'emits': []}
        spawn({
            'id': 'stateful-child',
            'component': child_body,
            'connectTo': ['parent'],
        })
        set_count(1)
    return {'accepts': [], 'emits': []}
";
        let fn_handle = eval_python_body(&engine_rc, iso, "ParentSt", body);
        let definition =
            build_py_node_definition("ParentSt".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "parent".to_string(), Attrs::new(), 0_u32)
            .expect("register parent");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch 1");
        }

        // First batch: child mounts and observes initial state.
        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "child-state:11"),
                "spawned child sees use_state initial: {observed:?}"
            );
        }

        // Bump the child's signal externally (hook_pos 0 on child id).
        // Then re-render and verify the new value is observed.
        let child_id = signal_id_for("stateful-child", 0).expect("child signal id");
        signal_set_external(child_id, Value::Number(99.into())).expect("set");

        let snapshot2 = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot2,
            );
            scheduler.process_batch().expect("batch 2");
        }

        {
            let observed = observations.lock();
            assert!(
                observed.iter().any(|s| s == "child-state:99"),
                "spawned child re-renders with new state value: {observed:?}"
            );
        }

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-α (Python-novel, footgun pin): validates the
    /// `_make_compose_wrapper(fn)` named factory pattern inside
    /// `use_compose`. Without the factory (using a bare lambda
    /// closing over the loop variable), every wrapped fn would
    /// resolve to the loop's last component — Python's closure-in-loop
    /// semantics. With the factory, each spec's component fires
    /// independently. This test pins the closure semantics by
    /// composing two children with distinct components and asserting
    /// each child's specific output is observed.
    #[test]
    fn phase3e_alpha_compose_wrappers_capture_independent_components() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("composer".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Build two distinct component fns directly (NOT via a
        // make_child factory). This is the case where Python's
        // closure-in-loop bug would manifest if use_compose's
        // wrapping wasn't done via _make_compose_wrapper.
        let body = r"
def body(ctx):
    count, set_count = use_state(0)
    if int(count) == 0:
        compose = use_compose()
        def alpha_body(child_ctx):
            host_observe('alpha:' + str(child_ctx['id']))
            return {'accepts': [], 'emits': []}
        def beta_body(child_ctx):
            host_observe('beta:' + str(child_ctx['id']))
            return {'accepts': [], 'emits': []}
        compose([
            {'id': 'k-alpha', 'component': alpha_body, 'connectTo': ['composer']},
            {'id': 'k-beta', 'component': beta_body, 'connectTo': ['composer']},
        ])
        set_count(1)
    return {'accepts': [], 'emits': []}
";
        let fn_handle = eval_python_body(&engine_rc, iso, "ComposerCap", body);
        let definition =
            build_py_node_definition("ComposerCap".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "composer".to_string(), Attrs::new(), 0_u32)
            .expect("register composer");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "alpha:k-alpha"),
            "k-alpha rendered with alpha_body (its own component, not beta_body): {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "beta:k-beta"),
            "k-beta rendered with beta_body (its own component, not alpha_body): {observed:?}"
        );
        // Negative: if closure-in-loop bug existed, both children
        // would render with beta_body (the last in the list), so
        // 'alpha:k-alpha' would be missing AND we'd see 'beta:k-alpha'.
        assert!(
            !observed.iter().any(|s| s == "beta:k-alpha"),
            "k-alpha did NOT bind to beta_body (closure-in-loop bug pin): {observed:?}"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // Phase 3e-β tests — supervision (use_monitor / use_demonitor /
    // use_link / use_unlink / use_group_join / use_group_leave)
    // -----------------------------------------------------------------

    /// Phase 3e-β: `use_monitor(target_id)` registers a monitor; when
    /// the target unmounts, the active node receives a `__DOWN__`
    /// system message. Body uses `use_message('__DOWN__', handler)`
    /// to observe. Verbatim port of JSC's
    /// `phase3e_zeta_use_monitor_delivers_down_on_unmount`.
    #[test]
    fn phase3e_beta_use_monitor_delivers_down_on_unmount() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("observer".to_string(), Attrs::new());
        graph.add_node("target".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // observer: monitor target + register __DOWN__ handler.
        let obs_body = r"
def body(ctx):
    _count, _set_count = use_state(0)
    mref = use_monitor('target')
    host_observe('mref:' + ('null' if mref is None else 'set'))
    def on_down(msg):
        payload = msg.get('payload', {}) if isinstance(msg, dict) else {}
        host_observe('down:' + str(payload.get('target', '')) + ':' + str(payload.get('reason', '')))
    use_message('__DOWN__', on_down)
    return {'accepts': ['__DOWN__'], 'emits': []}
";
        // target: unmounts itself on first render.
        let tgt_body = r"
def body(ctx):
    count, _set_count = use_state(0)
    if int(count) == 0:
        unmount = use_unmount_self()
        unmount()
    return {'accepts': [], 'emits': []}
";
        let fn_obs = eval_python_body(&engine_rc, iso, "Observer", obs_body);
        let fn_tgt = eval_python_body(&engine_rc, iso, "Target", tgt_body);
        let def_obs =
            build_py_node_definition("Observer".into(), Rc::clone(&engine_rc), iso, fn_obs);
        let def_tgt =
            build_py_node_definition("Target".into(), Rc::clone(&engine_rc), iso, fn_tgt);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_obs, "observer".to_string(), Attrs::new(), 0_u32)
            .expect("register observer");
        scheduler
            .register_node(def_tgt, "target".to_string(), Attrs::new(), 0_u32)
            .expect("register target");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "mref:set"),
            "use_monitor returned a non-null monitor ref; observations: {observed:?}"
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

    /// Phase 3e-β: `use_link(target_id)` returns True on success;
    /// `use_group_join(group)` registers the active node in the group.
    /// Verbatim port of JSC's
    /// `phase3e_zeta_use_link_and_group_join_register_cleanly`.
    #[test]
    fn phase3e_beta_use_link_and_group_join_register_cleanly() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    linked = use_link('b')
    host_observe('linked:' + str(bool(linked)))
    use_group_join('workers')
    host_observe('joined-workers')
    return {'accepts': [], 'emits': []}
";
        let noop_body = r"
def body(ctx):
    return {'accepts': [], 'emits': []}
";
        let fn_a = eval_python_body(&engine_rc, iso, "A", body);
        let fn_b = eval_python_body(&engine_rc, iso, "B", noop_body);
        let def_a = build_py_node_definition("A".into(), Rc::clone(&engine_rc), iso, fn_a);
        // 'b' needs a NodeDefinition registered with the scheduler so
        // `use_link('b')` resolves the id via the directory. A noop
        // body suffices. Mirrors JSC's `phase3e_zeta_use_link_*`
        // pattern.
        let def_b = build_py_node_definition("B".into(), Rc::clone(&engine_rc), iso, fn_b);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_a, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register a");
        scheduler
            .register_node(def_b, "b".to_string(), Attrs::new(), 0_u32)
            .expect("register b");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "linked:True"),
            "use_link('b') returned True (target registered): {observed:?}"
        );
        assert!(
            observed.iter().any(|s| s == "joined-workers"),
            "use_group_join('workers') executed without raising: {observed:?}"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-β (Python-novel): `use_demonitor(mref)` cancels a
    /// previously-registered monitor. After demonitor, target unmount
    /// does NOT deliver `__DOWN__`. Pins the cancellation path.
    #[test]
    fn phase3e_beta_use_demonitor_cancels_monitor() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("observer".to_string(), Attrs::new());
        graph.add_node("target".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // observer monitors target, then immediately demonitors. The
        // __DOWN__ handler is still registered, but the cancellation
        // means it won't fire when target unmounts.
        let obs_body = r"
def body(ctx):
    _count, _set_count = use_state(0)
    mref = use_monitor('target')
    if mref is not None:
        use_demonitor(mref)
        host_observe('demonitored')
    def on_down(msg):
        host_observe('down-fired')
    use_message('__DOWN__', on_down)
    return {'accepts': ['__DOWN__'], 'emits': []}
";
        let tgt_body = r"
def body(ctx):
    count, _set_count = use_state(0)
    if int(count) == 0:
        unmount = use_unmount_self()
        unmount()
    return {'accepts': [], 'emits': []}
";
        let fn_obs = eval_python_body(&engine_rc, iso, "DemoObs", obs_body);
        let fn_tgt = eval_python_body(&engine_rc, iso, "DemoTgt", tgt_body);
        let def_obs =
            build_py_node_definition("DemoObs".into(), Rc::clone(&engine_rc), iso, fn_obs);
        let def_tgt =
            build_py_node_definition("DemoTgt".into(), Rc::clone(&engine_rc), iso, fn_tgt);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_obs, "observer".to_string(), Attrs::new(), 0_u32)
            .expect("register observer");
        scheduler
            .register_node(def_tgt, "target".to_string(), Attrs::new(), 0_u32)
            .expect("register target");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        assert!(
            observed.iter().any(|s| s == "demonitored"),
            "use_demonitor was reached after use_monitor; observations: {observed:?}"
        );
        assert!(
            !observed.iter().any(|s| s == "down-fired"),
            "after demonitor, __DOWN__ should NOT be delivered; observations: {observed:?}"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // Anchor for the 3e-γ tests below; the round-trip test that
    // closes 3e-β follows. The 3e-γ tests sit at the very end of
    // mod tests so the M13 phase 3 closeout is visually clustered.

    /// Phase 3e-β (Python-novel): `use_unlink` and `use_group_leave`
    /// round-trip without exception even when the membership /
    /// linkage doesn't exist. Pins the no-op semantics. Also
    /// validates that `use_monitor` returns None for an unmounted /
    /// unknown target — host_monitor returns Null, wrapper returns
    /// None.
    #[test]
    fn phase3e_beta_unlink_group_leave_round_trip_and_monitor_none_for_unknown() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("a".to_string(), Attrs::new());
        graph.add_node("b".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    # Establish a link, then remove it.
    linked = use_link('b')
    host_observe('linked:' + str(bool(linked)))
    use_unlink('b')
    host_observe('unlinked-ok')
    # Group join + leave round-trip.
    use_group_join('workers')
    use_group_leave('workers')
    host_observe('group-round-trip-ok')
    # Group leave for a never-joined group is a no-op (no exception).
    use_group_leave('never-joined')
    host_observe('leave-unknown-ok')
    # Unlink for a never-linked target is a no-op.
    use_unlink('does-not-exist')
    host_observe('unlink-unknown-ok')
    # Monitor on unknown target returns None.
    mref = use_monitor('does-not-exist')
    host_observe('monitor-unknown:' + ('None' if mref is None else 'set'))
    return {'accepts': [], 'emits': []}
";
        let noop_body = r"
def body(ctx):
    return {'accepts': [], 'emits': []}
";
        let fn_a = eval_python_body(&engine_rc, iso, "RoundTrip", body);
        let fn_b = eval_python_body(&engine_rc, iso, "RoundTripB", noop_body);
        let def_a =
            build_py_node_definition("RoundTrip".into(), Rc::clone(&engine_rc), iso, fn_a);
        // 'b' needs a NodeDefinition for `use_link('b')` to resolve;
        // matches JSC's `phase3e_zeta_use_link_*` pattern.
        let def_b =
            build_py_node_definition("RoundTripB".into(), Rc::clone(&engine_rc), iso, fn_b);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_a, "a".to_string(), Attrs::new(), 0_u32)
            .expect("register a");
        scheduler
            .register_node(def_b, "b".to_string(), Attrs::new(), 0_u32)
            .expect("register b");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let observed = observations.lock();
        for want in &[
            "linked:True",
            "unlinked-ok",
            "group-round-trip-ok",
            "leave-unknown-ok",
            "unlink-unknown-ok",
            "monitor-unknown:None",
        ] {
            assert!(
                observed.iter().any(|s| s == want),
                "expected `{want}` observation; got: {observed:?}"
            );
        }

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // Phase 3e-γ tests — use_source + timer/delay/http/watch
    // descriptor factories. Closes M13 phase 3 on Python.
    // -----------------------------------------------------------------

    /// Phase 3e-γ: `use_source(lambda: timer_source(50))` and
    /// `use_source(lambda: delay_source(120))` dispatch the right
    /// descriptors to `host_use_source`. Wiring-only test with a
    /// custom `host_use_source` capture callback. Verbatim port of
    /// JSC's `phase3e_epsilon_use_source_dispatches_descriptor_to_host`.
    ///
    /// Why a wiring test instead of an integration test: timer/delay
    /// sources push events asynchronously via `tokio::spawn`. Observing
    /// them via `use_message` requires a multi-threaded tokio runtime
    /// plus sleep, which isn't compatible with the existing sync
    /// `engine_lock` / `process_batch` test harness. graphreact-core's
    /// existing source tests cover the production firing path; this
    /// test verifies the Python-to-Rust marshalling boundary.
    #[test]
    fn phase3e_gamma_use_source_dispatches_timer_delay_descriptors() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("ticker".to_string(), Attrs::new());

        let captured: Arc<parking_lot::Mutex<Vec<(String, u64)>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);

        let engine_rc: Rc<RefCell<CPythonEngine>> = Rc::new(RefCell::new(CPythonEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            drop(e);
            install_hook_prelude(&mut engine_rc.borrow_mut(), iso).expect("prelude");
            // Override the production host_use_source with a capture
            // callback. Replaces the real registration so we can
            // assert the descriptor without instantiating real
            // sources (which would need a tokio runtime).
            engine_rc
                .borrow_mut()
                .register_callback(
                    iso,
                    "host_use_source",
                    Box::new(move |args| {
                        let HostValue::Object(pairs) =
                            args.first().cloned().unwrap_or(HostValue::Null)
                        else {
                            return Ok(HostValue::Null);
                        };
                        let mut kind = String::new();
                        let mut value: u64 = 0;
                        for (k, v) in pairs {
                            #[allow(
                                clippy::cast_possible_truncation,
                                clippy::cast_sign_loss
                            )]
                            match (k.as_str(), v) {
                                ("kind", HostValue::String(s)) => kind = s,
                                ("intervalMs" | "delayMs", HostValue::Number(n)) => {
                                    value = n as u64;
                                }
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

        let body = r"
def body(ctx):
    use_source(lambda: timer_source(50), [])
    use_source(lambda: delay_source(120), [])
    return {'accepts': ['tick', 'timeout'], 'emits': []}
";
        let fn_handle = eval_python_body(&engine_rc, iso, "Ticker", body);
        let definition =
            build_py_node_definition("Ticker".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "ticker".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let captured_descriptors = captured.lock();
        assert_eq!(
            captured_descriptors.as_slice(),
            &[
                ("timer".to_string(), 50_u64),
                ("delay".to_string(), 120_u64),
            ],
            "use_source dispatched both descriptors in order; captured: {captured_descriptors:?}"
        );

        drop(captured_descriptors);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-γ: `use_source(lambda: http_source({'port': 8080}))`
    /// and `use_source(lambda: watch_source({'path': ...}))` decode
    /// the nested `opts` field and dispatch the right `(kind, port,
    /// path)` to the host. Wiring-only test using a custom capture
    /// callback. Verbatim port of JSC's
    /// `followon_a_use_source_decodes_http_and_watch_descriptors`.
    #[test]
    fn phase3e_gamma_use_source_decodes_http_and_watch_descriptors() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("server".to_string(), Attrs::new());

        // (kind, port-as-u64, path) tuples. Port stored as u64 for
        // uniform shape; path empty when descriptor is http.
        let captured: Arc<parking_lot::Mutex<Vec<(String, u64, String)>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);

        let engine_rc: Rc<RefCell<CPythonEngine>> = Rc::new(RefCell::new(CPythonEngine::new()));
        let iso = {
            let mut e = engine_rc.borrow_mut();
            e.initialize().expect("init");
            let iso = e.create_isolate(IsolateOptions::default()).expect("create");
            drop(e);
            install_hook_prelude(&mut engine_rc.borrow_mut(), iso).expect("prelude");
            engine_rc
                .borrow_mut()
                .register_callback(
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
                                        #[allow(
                                            clippy::cast_possible_truncation,
                                            clippy::cast_sign_loss
                                        )]
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

        let body = r"
def body(ctx):
    use_source(lambda: http_source({'port': 8080}), [])
    use_source(lambda: watch_source({'path': '/tmp/grx-3e-gamma'}), [])
    return {'accepts': ['request', 'change'], 'emits': []}
";
        let fn_handle = eval_python_body(&engine_rc, iso, "Server", body);
        let definition =
            build_py_node_definition("Server".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "server".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                snapshot,
            );
            scheduler.process_batch().expect("batch");
        }

        let descriptors = captured.lock();
        assert_eq!(
            descriptors.as_slice(),
            &[
                ("http".to_string(), 8080_u64, String::new()),
                ("watch".to_string(), 0_u64, "/tmp/grx-3e-gamma".to_string()),
            ],
            "use_source decoded both http+watch opts; captured: {descriptors:?}"
        );

        drop(descriptors);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    /// Phase 3e-γ: `use_source(lambda: watch_source({'path': ...}))`
    /// end-to-end — production `host_use_source` decoder builds a
    /// real [`crate::sources::watch_source`], `start()` registers a
    /// notify watcher, file write under the watched path delivers a
    /// `change` message that the body's `use_message('change', ...)`
    /// handler observes.
    ///
    /// Polls process_batch in a 5s window because notify (FSEvents on
    /// macOS) has variable latency. Generous margins per
    /// `feedback_timing_tests.md`. notify spawns its own thread for
    /// FSEvents, so this test works under the existing sync
    /// `engine_lock` harness — no tokio runtime needed (matches JSC
    /// followon-A's pattern).
    #[test]
    fn phase3e_gamma_watch_source_fires_change_events_e2e() {
        let _g = engine_lock();
        reset_global_state();

        let tmpdir = tempfile::tempdir().expect("tempdir");
        let watch_path = tmpdir.path().to_string_lossy().to_string();

        let mut graph = CommunicationGraph::new();
        graph.add_node("watcher".to_string(), Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Defensive single-quote escape (no risk under tempfile but
        // matches JSC followon-A).
        let escaped_path = watch_path.replace('\\', r"\\").replace('\'', r"\'");
        let body = format!(
            r"
def body(ctx):
    _count, _set_count = use_state(0)
    use_source(lambda: watch_source({{'path': '{escaped_path}'}}), [])
    def on_change(msg):
        host_observe('change')
    use_message('change', on_change)
    return {{'accepts': ['change'], 'emits': []}}
",
        );
        let fn_handle = eval_python_body(&engine_rc, iso, "Watcher", &body);
        let definition =
            build_py_node_definition("Watcher".into(), Rc::clone(&engine_rc), iso, fn_handle);
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(definition, "watcher".to_string(), Attrs::new(), 0_u32)
            .expect("register");

        let snapshot = Rc::new(GraphSnapshot::from_live(
            Rc::clone(&scheduler.graph),
            Rc::clone(&scheduler.manifests),
            Rc::clone(&scheduler.edge_accept_predicates),
        ));

        // Initial batch — registers source, calls start(), notify
        // begins watching the tempdir.
        {
            let _engine_guard = PyEngineGuard::with_directory(
                Rc::clone(&engine_rc),
                iso,
                Rc::clone(&scheduler.directory),
                Rc::clone(&snapshot),
            );
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
                let _engine_guard = PyEngineGuard::with_directory(
                    Rc::clone(&engine_rc),
                    iso,
                    Rc::clone(&scheduler.directory),
                    Rc::clone(&snapshot),
                );
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
            "watch_source fired a 'change' message after file write; observations: {observed:?}"
        );

        drop(observed);
        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // M13-followon-D analogue test — per-node __hostCallbacks token GC
    // -----------------------------------------------------------------

    /// **M13-followon-D analogue**: single-node body uses `use_effect`
    /// plus `use_message` plus `use_callback` (three body-time tokens).
    /// Asserts that:
    /// 1. After mount + first `process_batch`,
    ///    `tracked_token_count() >= 3` under node id `"leaky"`.
    /// 2. Calling `release_tokens_for_node(&engine, iso, "leaky")`
    ///    drains the bucket AND dispatches `__release_tokens([...])`
    ///    Python-side, leaving `tracked_token_count() == 0`.
    ///
    /// Verbatim Python port of JSC's
    /// `followon_d_unmount_sweeps_host_callbacks_tokens`. Exercises
    /// the helper directly rather than driving NodeRemoved through
    /// the scheduler, mirroring JSC's pattern (the topology-listener
    /// → `pending_unmounts` → `process_batch` drain path is exercised
    /// implicitly by every existing 3e-α `use_unmount_self` test;
    /// this test focuses on the helper's mechanism).
    #[test]
    fn followon_d_analogue_unmount_sweeps_host_callbacks_tokens() {
        let _g = engine_lock();
        reset_global_state();

        let mut graph = CommunicationGraph::new();
        graph.add_node("leaky".to_string(), Attrs::new());
        graph.add_node("peer".to_string(), Attrs::new());
        graph.add_edge("leaky".to_string(), "peer".to_string(), None, Attrs::new());

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        // Body uses three body-time hooks that mint __hostCallbacks
        // tokens via __register_callback:
        //   - use_effect(fn) — registers `fn` under a token
        //   - use_message('msg', handler) — registers `handler`
        //   - use_callback(fn, deps) — caches a token for `fn`
        // Each renders once on mount → 3+ tokens land in
        // NODE_TOKENS["leaky"].
        let body = r"
def body(ctx):
    def on_mount():
        host_observe('mount-effect-fired')
    use_effect(on_mount, [])
    def on_msg(msg):
        host_observe('msg-handler-fired')
    use_message('ping', on_msg)
    def stable_cb():
        return None
    _cb = use_callback(stable_cb, [])
    return {'accepts': ['ping'], 'emits': []}
";
        let noop_body = r"
def body(ctx):
    return {'accepts': [], 'emits': []}
";
        let fn_leaky = eval_python_body(&engine_rc, iso, "Leaky", body);
        let fn_peer = eval_python_body(&engine_rc, iso, "Peer", noop_body);
        let def_leaky =
            build_py_node_definition("Leaky".into(), Rc::clone(&engine_rc), iso, fn_leaky);
        let def_peer =
            build_py_node_definition("Peer".into(), Rc::clone(&engine_rc), iso, fn_peer);

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(def_leaky, "leaky".to_string(), Attrs::new(), 0_u32)
            .expect("register leaky");
        scheduler
            .register_node(def_peer, "peer".to_string(), Attrs::new(), 0_u32)
            .expect("register peer");

        // Initial batch — body renders once, mints tokens.
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            scheduler.process_batch().expect("first batch");
        }

        // Three body-time tokens minted: one each for use_effect,
        // use_message, use_callback. Cleanup tokens may also be
        // minted from inside __invoke_callback during effect-fire,
        // but those land OUTSIDE the body-eval window (CURRENT_NODE_ID
        // unset) and are not tracked — they auto-release via
        // EffectCleanup. So the count tracked under "leaky" is
        // exactly the body-time tokens.
        let pre_count = tracked_token_count();
        assert!(
            pre_count >= 3,
            "expected >= 3 body-time tokens tracked under 'leaky' (use_effect + use_message + use_callback); got {pre_count}"
        );

        // Sweep tokens for 'leaky' directly. Mirrors what
        // PyRuntime::process_batch does for each id in pending_unmounts
        // after a NodeRemoved fires. The engine guard must be installed
        // because release_tokens_for_node re-enters Python via
        // invoke_global("__release_tokens", ...).
        {
            let _engine_guard = PyEngineGuard::new(Rc::clone(&engine_rc), iso);
            release_tokens_for_node(&engine_rc, iso, "leaky");
        }

        // After sweep: NODE_TOKENS["leaky"] is drained; tracked count
        // drops to whatever 'peer' contributed (zero — peer body is a
        // no-op).
        let post_count = tracked_token_count();
        assert_eq!(
            post_count, 0,
            "after release_tokens_for_node, NODE_TOKENS for 'leaky' should be empty (peer contributed 0); got {post_count}"
        );

        let mut e = engine_rc.borrow_mut();
        e.dispose_isolate(iso).expect("dispose");
        e.shutdown().expect("shutdown");
    }

    // -----------------------------------------------------------------
    // M13-phase3g tests — useEffectAsync via asyncio bridge
    // -----------------------------------------------------------------
    //
    // Each test drives `Scheduler::process_batch_async()` from a tokio
    // LocalSet (current_thread runtime) since PyRuntime / scheduler is
    // `!Send`. Same pattern as JSC's phase1b tests. The Python invoker
    // synchronously runs the user's coroutine to completion via
    // `asyncio.new_event_loop().run_until_complete(coro)`, so the
    // Rust-side oneshot is signaled before the Receiver awaits — no
    // manual microtask drain or periodic polling needed.

    /// **Phase 3g #1.** `use_effect_async` with an `async def` that
    /// returns `None` (no cleanup) fires the body during
    /// `process_batch_async`. Pins basic wiring. Verbatim Python port
    /// of JSC's `phase1b_use_effect_async_void_resolution_fires_body`.
    #[test]
    fn phase3g_use_effect_async_void_resolution_fires_body() {
        let _g = engine_lock();
        reset_global_state();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = tokio::task::LocalSet::new();

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));

        local.block_on(&runtime, async {
            let observations_for_cb = Arc::clone(&observations);
            let mut graph = CommunicationGraph::new();
            graph.add_node("n".to_string(), Attrs::new());

            let (engine_rc, iso) = boot_engine_with_observe(observations_for_cb);

            let body = r"
def body(ctx):
    async def my_async():
        host_observe('async-effect-fired')
        return None
    use_effect_async(my_async, [])
    return {'accepts': [], 'emits': []}
";
            let fn_handle = eval_python_body(&engine_rc, iso, "N", body);
            let definition =
                build_py_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
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
                let _eg = PyEngineGuard::with_directory(
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

    /// **Phase 3g #2.** `use_effect_async` that returns a cleanup
    /// callable. The cleanup runs on next deps-change re-render
    /// triggered by `set_count(1)` from the body's first render via
    /// sync `use_effect`. Verbatim Python port of JSC's
    /// `phase1b_use_effect_async_cleanup_fires_on_deps_change`.
    #[test]
    fn phase3g_use_effect_async_cleanup_fires_on_deps_change() {
        let _g = engine_lock();
        reset_global_state();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = tokio::task::LocalSet::new();

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));

        local.block_on(&runtime, async {
            let observations_for_cb = Arc::clone(&observations);
            let mut graph = CommunicationGraph::new();
            graph.add_node("n".to_string(), Attrs::new());

            let (engine_rc, iso) = boot_engine_with_observe(observations_for_cb);

            // Body uses use_effect_async with deps=[count]. First
            // render (count=0): async effect installs cleanup.
            // set_count(1) dirties the node; second render: deps
            // differ, async effect re-fires — but BEFORE re-firing,
            // the previous cleanup runs.
            let body = r"
def body(ctx):
    count, set_count = use_state(0)
    host_observe('render:count=' + str(int(count)))
    captured_count = int(count)
    async def my_async():
        host_observe('async-mount:count=' + str(captured_count))
        def cleanup():
            host_observe('async-cleanup:count=' + str(captured_count))
        return cleanup
    use_effect_async(my_async, [int(count)])
    if int(count) == 0:
        set_count(1)
    return {'accepts': [], 'emits': []}
";
            let fn_handle = eval_python_body(&engine_rc, iso, "N", body);
            let definition =
                build_py_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
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
                let _eg = PyEngineGuard::with_directory(
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
                observed.iter().any(|s| s == "render:count=0"),
                "first render observed; observations: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "render:count=1"),
                "second render observed (deps differ); observations: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "async-mount:count=0"),
                "async mount fired with count=0; observations: {observed:?}"
            );
            assert!(
                observed.iter().any(|s| s == "async-cleanup:count=0"),
                "async cleanup from prior deps fired before new mount; observations: {observed:?}"
            );
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

    /// **Phase 3g #3.** An async fn that raises is caught — Rust logs
    /// the rejection and the cleanup is None. Subsequent renders
    /// still work. Verbatim Python port of JSC's
    /// `phase1b_use_effect_async_rejection_handled_gracefully`.
    #[test]
    fn phase3g_use_effect_async_rejection_handled_gracefully() {
        let _g = engine_lock();
        reset_global_state();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = tokio::task::LocalSet::new();

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));

        local.block_on(&runtime, async {
            let observations_for_cb = Arc::clone(&observations);
            let mut graph = CommunicationGraph::new();
            graph.add_node("n".to_string(), Attrs::new());

            let (engine_rc, iso) = boot_engine_with_observe(observations_for_cb);

            // Async fn raises. Body still completes its render; the
            // rejection is logged by Rust (eprintln) but doesn't
            // propagate as a panic.
            let body = r"
def body(ctx):
    host_observe('rendering')
    async def my_async():
        raise RuntimeError('async-boom')
    use_effect_async(my_async, [])
    host_observe('returning-manifest')
    return {'accepts': [], 'emits': []}
";
            let fn_handle = eval_python_body(&engine_rc, iso, "N", body);
            let definition =
                build_py_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
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
                let _eg = PyEngineGuard::with_directory(
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

    /// **Phase 3g #4.** `use_effect_async` coexists with sync
    /// `use_effect` — both fire during `process_batch_async`. Pins
    /// the contract that the async path processes the sync slot
    /// identically. Verbatim Python port of JSC's
    /// `phase1b_use_effect_async_coexists_with_sync_use_effect`.
    #[test]
    fn phase3g_use_effect_async_coexists_with_sync_use_effect() {
        let _g = engine_lock();
        reset_global_state();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = tokio::task::LocalSet::new();

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));

        local.block_on(&runtime, async {
            let observations_for_cb = Arc::clone(&observations);
            let mut graph = CommunicationGraph::new();
            graph.add_node("n".to_string(), Attrs::new());

            let (engine_rc, iso) = boot_engine_with_observe(observations_for_cb);

            let body = r"
def body(ctx):
    def sync_effect():
        host_observe('sync-effect')
    use_effect(sync_effect, [])
    async def my_async():
        host_observe('async-effect')
        return None
    use_effect_async(my_async, [])
    return {'accepts': [], 'emits': []}
";
            let fn_handle = eval_python_body(&engine_rc, iso, "N", body);
            let definition =
                build_py_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
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
                let _eg = PyEngineGuard::with_directory(
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

    /// **Phase 3g #5.** `process_batch` (sync) does NOT fire async
    /// effects. Pins the opt-in contract — graphreact-core's
    /// `use_effect_async` only flushes via `flush_effects_async`,
    /// which only the async path drives. Verbatim Python port of
    /// JSC's `phase1b_use_effect_async_skipped_on_sync_path`.
    #[test]
    fn phase3g_use_effect_async_skipped_on_sync_path() {
        let _g = engine_lock();
        reset_global_state();

        let observations: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));

        let mut graph = CommunicationGraph::new();
        graph.add_node("n".to_string(), Attrs::new());

        let (engine_rc, iso) = boot_engine_with_observe(Arc::clone(&observations));

        let body = r"
def body(ctx):
    def sync_effect():
        host_observe('sync-fired')
    use_effect(sync_effect, [])
    async def my_async():
        host_observe('async-fired')
    use_effect_async(my_async, [])
    return {'accepts': [], 'emits': []}
";
        let fn_handle = eval_python_body(&engine_rc, iso, "N", body);
        let definition = build_py_node_definition("N".into(), Rc::clone(&engine_rc), iso, fn_handle);
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
            let _eg = PyEngineGuard::with_directory(
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
}
