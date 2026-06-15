# graphreact-engine-zig

*Engine-agnostic Rust trait surface for embedding host languages (JSC / CPython / V8 / JVM) — concrete impls live in sister `graphreact-engine-*` crates.*

## Milestone

Ships as part of the M12 substrate that bootstrapped the standalone runtime + engine-bridge architecture. M12 itself is a shared-substrate milestone whose JSC track is narrated across [`../../docs/backend/m12.3b-webkit-vendoring.md`](../../docs/backend/m12.3b-webkit-vendoring.md) (cross-platform JSC vendoring) and whose CPython track lives at [`../../docs/backend/m13-phases.md`](../../docs/backend/m13-phases.md). The per-engine reachability matrix for the trait surface this crate pins is at [`../../docs/backend/m6.6-engine-parity.md`](../../docs/backend/m6.6-engine-parity.md).

## Role in the runtime

This crate sits one layer above `graphreact-core` and one layer below every concrete engine adapter. The runtime calls into [`Engine`](src/traits.rs) via the trait — it never knows which engine is loaded; `graphreact-cli` picks one at startup by `--runtime=jsc|cpython|null|echo`. The crate has **zero `graphreact-*` dependencies** by design: it is the substrate seam, and pulling in `graphreact-core` here would invert the dependency graph. Concrete engines vary in their implementation language (JSC in Zig per M12.3a; CPython via Zig + PyO3 patterns per M13; V8 in Zig per M17), but each implements the same trait so the CLI + scheduler can swap them transparently.

## Key types

- [`Engine`](src/traits.rs#L38) — the trait every concrete engine implements: `initialize` / `create_isolate` / `eval_module` / `call_function` / `register_callback` / `drain_microtasks` / `dispose` / `shutdown`.
- [`EngineError` / `EngineResult`](src/error.rs#L6) — typed error taxonomy for every fallible engine entry point; `thiserror`-derived so concrete engines can wrap their native error shapes.
- [`IsolateHandle` / `ModuleHandle` / `FunctionHandle`](src/handles.rs) — opaque `u64` handles the runtime hands back to the engine without knowing their native layout.
- [`IsolateOptions` / `MicrotaskPolicy`](src/options.rs) — startup-time isolate configuration, including the microtask-drain contract the scheduler relies on.
- [`HostValue` / `HostObject`](src/value.rs#L26) — JSON-shaped value envelope for host-callback argument/return marshalling, identical across engines.

## Composes with

This crate depends on **no other graphreact crates** — it is the trait substrate. Crates that depend on it:

| Dependent crate | Role |
|---|---|
| `graphreact-cli` | Selects the concrete engine at startup via `--runtime`. |
| `graphreact-engine-jsc` | Implements `Engine` over JavaScriptCore (M12.3a/b). |
| `graphreact-engine-cpython` | Implements `Engine` over embedded CPython (M13). |
| `graphreact-engine-echo` | Test-only `Engine` impl that echoes calls back. |
| `graphreact-engine-null` | No-op `Engine` for runtime work that needs no host language. |

## Running the tests

```sh
cargo test -p graphreact-engine-zig --release
```

Trait-surface unit tests; a handful of round-trip cases pin `HostValue` JSON shape and the handle types.

## See also

- M12 substrate narratives: [`../../docs/backend/m12.3b-webkit-vendoring.md`](../../docs/backend/m12.3b-webkit-vendoring.md), [`../../docs/backend/m13-phases.md`](../../docs/backend/m13-phases.md)
- Per-engine reachability matrix: [`../../docs/backend/m6.6-engine-parity.md`](../../docs/backend/m6.6-engine-parity.md)
- Decision 0001 (V8 engine question): [`../../docs/decisions/0001-v8-engine-question.md`](../../docs/decisions/0001-v8-engine-question.md)
- Workspace onboarding: [`../../docs/onboarding.md`](../../docs/onboarding.md)
- Architecture overview: [`../../docs/architecture.md`](../../docs/architecture.md)
