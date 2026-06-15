# graphreact-engine-null

*Null engine — implements the Engine trait without executing guest code; default for `grx run --runtime=null` topology dry-runs and CI smoke coverage of the parser→runtime path.*

## Milestone

Ships as part of [M12 substrate — see narrative](../../docs/backend/m6.6-engine-parity.md). The null engine is the reference implementation of the `Engine` trait introduced by the M12 architecture and exercised by the parity audit; the same trait is implemented by the JSC ([M12.3a / M12.3b](../../docs/backend/m12.3b-webkit-vendoring.md)) and CPython ([M13](../../docs/backend/m13-phases.md)) bridges.

## Role in the runtime

`graphreact-engine-null` sits in the **engine tier** of the workspace, directly above the `graphreact-engine-zig` substrate that defines the `Engine` trait + handle types. It solves two problems at once: (1) it lets the lifecycle architecture (initialise → create-isolate → register-callback → eval-module → resolve-function → call-function → drain → dispose) be tested without linking a real JS or Python VM, and (2) it gives `grx run --runtime=null` a real engine to mount so the parser → IR → scheduler path stays smoke-covered in CI environments without the JSC toolchain. It does NOT execute guest bodies — `eval_module` records the source, `call_function` validates the handle and returns `HostValue::Null`.

## Key types

- **`NullEngine`** — the engine itself; cheap to construct via `NullEngine::new()` / `Default::default()`, each instance owns its own handle counters and isolate table. See [`src/lib.rs:31`](src/lib.rs#L31).
- **`impl Engine for NullEngine`** — the trait implementation; mirrors the lifecycle expected by every engine in the workspace. See [`src/lib.rs:99`](src/lib.rs#L99).
- **`NullEngine::live_isolates`** — diagnostic accessor returning the current isolate count; load-bearing for the stress test that asserts disposes balance creates.

## Composes with

| Crate | Role |
|---|---|
| [`graphreact-engine-zig`](../graphreact-engine-zig/) | Defines the `Engine` trait, `IsolateHandle` / `ModuleHandle` / `FunctionHandle` / `CallbackId` / `HostValue` / `EngineError` that this crate implements against. |
| `parking_lot` | `Mutex` around the per-engine isolate table. |
| `rustc-hash` | `FxHashMap` for handle → state lookups. |

Depended on by:

- [`graphreact-cli`](../graphreact-cli/) — wires `NullEngine` behind the `grx run --runtime=null` flag and uses it as the default engine for topology dry-runs.

## Running the tests

```sh
cargo test -p graphreact-engine-null --release
```

The crate ships nine in-file unit tests covering the full lifecycle plus a 1000-cycle create/dispose stress test asserting no isolate leaks.

## See also

- Parent milestone narrative: [`../../docs/backend/m6.6-engine-parity.md`](../../docs/backend/m6.6-engine-parity.md)
- Decision: [`../../docs/decisions/0001-v8-engine-question.md`](../../docs/decisions/0001-v8-engine-question.md) — engine-substrate choice; JSC over V8, with the same `Engine` trait null-, JSC-, and CPython-engines all implement.
- Decision: [`../../docs/decisions/0002-graphreact-is-backend-only.md`](../../docs/decisions/0002-graphreact-is-backend-only.md) — frames why a null engine matters for CI / no-JS-engine environments.
- Workspace onboarding: [`../../docs/onboarding.md`](../../docs/onboarding.md)
- Architecture overview: [`../../docs/architecture.md`](../../docs/architecture.md)
