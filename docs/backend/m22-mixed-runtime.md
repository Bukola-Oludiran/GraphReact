# M22 — Mixed-runtime systems (JS + Python + Go in one `.grx`)

**Status: SHIPPED 2026-06-17 (phases 1-4).** Per-component engine routing,
per-engine lifecycle ordering with init-all-before-mount + reverse-order
shutdown, `--runtime=mixed` CLI flag with `SchedulerHost::Mixed`
orchestration, and per-engine Prometheus counters
(`grx_engine_mount_count{engine}` + `grx_engine_eval_count{engine}`).

Cross-engine messaging is local-to-engine in phases 1-4 (each engine owns
its own scheduler); cross-engine wire routing + per-lang hot-reload diff
lanes + per-engine `--debug` attach are scoped to **M22 phase 5+** and
listed in the "open work" table at the bottom.

## What this milestone delivers

A daemon that runs JS, Python, and Go bodies **in the same `.grx` file**
without forcing the operator to pick a single engine slot at `grx run`
time. Per-component dispatch is closure-based (each `NodeDefinition` body
closure captures its engine `Rc`), so the existing per-engine scheduler-
mount paths are reused verbatim — the M22 orchestrator is purely
compositional, not a scheduler refactor.

| Surface | Status |
|---|---|
| Per-component `EngineMisroute` relaxation across JSC + CPython + Go mount paths | ✓ Phase 1 |
| `MixedRuntime` orchestrator (per-engine schedulers; co-owns three runtimes) | ✓ Phase 2 |
| Initialize-all-before-mount, reverse-order shutdown, partial-init unwind | ✓ Phase 2 |
| `RuntimeArg::Mixed` + `EngineKind::Mixed` + `SchedulerHost::Mixed` | ✓ Phase 3 |
| `grx run --runtime=mixed <fixture.grx>` end-to-end | ✓ Phase 3 |
| `grx cluster daemon --runtime=mixed` end-to-end | ✓ Phase 3 |
| `grx_engine_mount_count{engine}` + `grx_engine_eval_count{engine}` Prometheus counters | ✓ Phase 4 |
| Single label vocabulary `{jsc, cpython, go, null, echo}` shared by `EngineSlot` + `EngineKind::engine_label` | ✓ Phase 4 |
| `examples/mixed-runtime-supervisor.grx` acceptance fixture (JS supervisor + Python child + Go child) | ✓ Phase 3 |
| Cross-engine emit/message routing through a unified bus | Open — M22 phase 5+ |
| `--watch --runtime=mixed` (per-lang diff classification) | Open — M22 phase 5+ |
| `--debug --runtime=mixed` (multi-engine attach) | Open — M22 phase 5+ |
| Per-engine binary split (`grx-jsc` / `grx-py` / `grx-go`) | Coordinated with Cleanup-Pass |

## Architecture

### Per-engine sub-schedulers (NOT a shared scheduler)

The M22 ship choice is **three independent schedulers** (one per engine
slot) co-owned by a `MixedRuntime`. Each engine's `mount_with_scheduler`
mounts its own lang slice against its own freshly-constructed
`Scheduler`; no shared graph crosses engine boundaries today.

Discovery-time the alternative — a single Scheduler with per-node
engine tags so message delivery can hop engines mid-cascade — would
require:

- An `engine_kind: ComponentLang` metadata field on
  `NodeDefinition` (so the Scheduler can pick the right TLS engine
  guard at dispatch time).
- A multi-engine `process_batch` that installs different engines'
  TLS guards within a single batch.
- Cross-engine FFI re-entrancy audits (CPython hooks invoking Rust
  closures that call into JSC siblings, etc).

That's the M22 phase 5+ scope. The per-engine-scheduler shape ships
the visible operator surface (`--runtime=mixed`, per-engine metrics,
all three engines alive in one daemon) without the scheduler refactor.

### `EngineMisroute` relaxation rationale

Pre-M22 each `mount_with_scheduler` hard-errored with
`RuntimeError::EngineMisroute` when fed a foreign-lang definition. That
gate kept single-engine callers out of opaque downstream errors (a
Python body fed to JSC would otherwise surface as a JS `SyntaxError` at
eval time).

Post-M22 the orchestrator wants to feed the SAME IR to every engine
and have each pick up its own lang slice. The simplest path was to
relax the strict misroute check into a silent skip (with a
`tracing::debug` event for diagnostics). Single-engine callers see
zero regression because:

- A pure-JS `.grx` fed to `--runtime=jsc` still mounts every node
  (no foreign-lang definitions to skip).
- A `.grx` with a stray foreign-lang body under `--runtime=jsc` now
  silently skips that body; the IR node it tagged simply doesn't
  register with the JSC scheduler. The same node would NOT have
  rendered correctly under the old strict-misroute regime either
  (the daemon would have errored out at mount time); the new
  behavior is strictly more permissive AND can be composed in the
  mixed orchestrator.

`RuntimeError::EngineMisroute` is kept as a variant for the future
hard-routing diagnostic case (a mixed orchestrator that can't bring up
a required engine slot at all).

### Lifecycle ordering

`MixedRuntime::mount_mixed(path)` walks the resolved IR's definitions
and decides which engines to bring up:

```text
required_engines: ∀ definition, definition.lang → EngineSlot
  Js     → Jsc
  Python → Cpython
  Go     → Go
```

The init order is deterministic JSC → CPython → Go. For each required
slot:

1. Call that engine's `mount_with_scheduler` (which does the engine's
   own `initialize` + `create_isolate` + host-callback registration
   + per-lang definition eval + per-node scheduler registration).
2. On success, push the slot into `init_order` and store the runtime
   in the corresponding `Option<Box<...>>`.
3. On error: unwind the already-initialized engines in reverse order
   via each runtime's `shutdown`. Return the original `RuntimeError`
   so the caller sees the cause.

`MixedRuntime::shutdown()` mirrors the success-path reverse order:
Go → CPython → JSC. Per-engine shutdown errors are tracing-warned and
the chain continues so no engine is leaked; the LAST error is
returned (matches the daemon-teardown best-effort shape).

### Per-component dispatch

Per-component dispatch happens via existing closure shape — each
`NodeDefinition`'s body closure captures its engine `Rc<RefCell<...>>`
+ isolate handle + function handle. The Scheduler is engine-agnostic;
the engine-specific dispatch happens inside the closure. The mixed
runtime simply has three separate `Scheduler`s, each with closures
that capture different engines.

A future M22 phase 5+ refactor that adopts a shared Scheduler would
add a `engine_kind: ComponentLang` field on `NodeDefinition` for
metrics/diagnostics + a per-engine TLS guard installer that the
Scheduler consults before dispatching each node.

### Engine version reporting

`MixedRuntime::engine_version_banner` returns one `<engine> <version>`
chunk per active slot, joined by `; `. Example for the supervisor
fixture:

```text
jsc jsc-0.1.0 (M12.3b-phase1.5...); cpython cpython-0.1.0-m13-phase3h (...); go go-0.1.0 (M17 phase 3a-3b...)
```

`grx run --runtime=mixed` prints this banner under
`running '...' on engine 'mixed' (<banner>)`.

## Acceptance gates

| Gate | Phase | How verified |
|---|---|---|
| Per-engine mount paths skip foreign-lang definitions instead of erroring | Phase 1 | `pyruntime::tests::m22_phase1_skips_non_python_definitions_under_mixed_relax`, `goruntime::scenario_skips_non_go_lang_under_mixed_relax` |
| Pure-JS fixture brings up only the JSC slot | Phase 2 | `mixedruntime::tests::pure_js_fixture_brings_up_jsc_only` |
| Pure-Python fixture brings up only the CPython slot | Phase 2 | `mixedruntime::tests::pure_python_fixture_brings_up_cpython_only` |
| Mixed JS+Python fixture brings up JSC THEN CPython in deterministic order | Phase 2 | `mixedruntime::tests::mixed_js_python_fixture_init_order_is_deterministic` |
| Full three-engine fixture mounts all three slots + reverse-order shutdown | Phase 3 | `mixedruntime::tests::phase3_acceptance_full_mixed_supervisor_fixture` (skipped when `go` is missing) |
| Per-engine Prometheus counters populate per active slot only | Phase 4 | `mixedruntime::tests::phase4_acceptance_engine_mount_count_populates_per_slot` |

## Open work (M22 phase 5+)

- **Cross-engine emit/message routing.** Today each engine owns its
  own `Scheduler`; emits inside a JSC body land in the JSC scheduler's
  queue and are NEVER delivered to a CPython/Go sibling. Phase 5+
  adds a unified message bus (shared Scheduler with per-node engine
  tag OR an inter-scheduler relay layer).
- **`--watch --runtime=mixed`.** The existing diff classifier keys on
  a single runtime; per-lang lane classification (so a Python edit
  doesn't trigger a JSC remount) is phase 5+.
- **`--debug --runtime=mixed`.** The flag identifies a single engine
  slot to attach against. Multi-engine attach (one `--debug` per
  engine) is phase 5+.
- **Per-engine RSS sampling.** `/v1/metrics` reports process-wide RSS
  today. Engine-attributed RSS sampling (JSC heap vs CPython
  arena vs Go-runtime) requires per-engine instrumentation hooks
  that aren't trivially additive across engines.
- **Cluster dist-sink multiplex.** `SchedulerHost::scheduler_mut` on
  Mixed returns the FIRST active engine's scheduler, so
  `Cluster::start` installs dist sinks only on that engine. Cross-
  engine cluster dispatch needs a multiplex sink shape (phase 5+).

## Cross-references

- [`m13-phases.md`](m13-phases.md) — M13 phase 3a's `EngineMisroute`
  is now a soft routing input (Phase 1 relaxation), not a hard
  failure. Single-engine paths still see clean errors when the user
  picks a foreign-lang fixture.
- [`m6.6-engine-parity.md`](m6.6-engine-parity.md) — same per-engine
  hook surface is reachable from mixed mounts; the mixed orchestrator
  doesn't gate any hook.
- [`../decisions/0003-graphreact-bifurcates-into-backend-and-db.md`](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md)
  — ADR-0003 listed `M22 = JS + Python + JVM` pre-2026-06-15;
  Java → Go pivot updates the third language slot to Go.
- `crates/graphreact-cli/src/mixedruntime.rs` — orchestrator
  implementation.
- `crates/graphreact-cli/tests/fixtures/mixed-runtime-supervisor.grx`
  — acceptance fixture.
