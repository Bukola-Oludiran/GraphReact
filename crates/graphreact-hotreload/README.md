# graphreact-hotreload

_Hot reload (second BEAM-class property) — file watcher, IR diff classifier, apply orchestrator, rollback on first-tick failure; per-language hook-AST diffing via swc + rustpython._

## Milestone

Ships as part of [M15 — Hot reload (second BEAM-class property) — SHIPPED 2026-05-07](../../docs/backend/m15-hot-reload.md). Second BEAM-class property after M14 distribution: the runtime feature that makes "edit a `.grx` and watch the graph pick it up without restart" routine across the application space (Thymus self-healing policy rollouts, ComplyFlow live regulatory rules, NewsCaster / Generative Worlds live world logic, all eight named application classes).

## Role in the runtime

Sits in the substrate tier alongside [`graphreact-grx-parser`](../graphreact-grx-parser/), one layer above [`graphreact-core`](../graphreact-core/) and orthogonal to the engine crates (JSC, CPython). The crate owns the full reload pipeline as plain library code — `notify`-backed file watching, IR re-parse, structural diff classification, apply orchestration against `Scheduler`, and rollback on first-tick failure — and exposes engine-bridge points (`EngineAdapter` trait) that the CLI's per-isolate adapters implement. Engine-free by construction: it depends on `graphreact-core` for `Scheduler` / `Message` and on `graphreact-grx-parser` for `GrxIr`, but knows nothing about JSC isolates or CPython interpreters directly. Cluster-wide reload propagation lives in `graphreact-dist` (M15 Phase 10) and lands here via the bridge channel set up by `graphreact-cli`.

## Key types

- [`FileWatcher` / `WatchEvent`](src/watcher.rs) — `notify::RecommendedWatcher`-backed watcher with 50ms atomic-save coalescing window and a bounded mpsc event channel.
- [`classify` / `ReloadDelta`](src/diff.rs) — structural IR diff classifier; returns `Compatible | Incompatible | TopologyOnly | Mixed` based on hook-signature comparison against the `DiffCache`.
- [`ReloadOrchestrator` / `ReloadOutcome`](src/orchestrator.rs) — `apply(scheduler, delta)` entry point; dispatches `replace_definition` for compatible swaps, drain+remount for incompatible, and pending topology mutations for edge-only deltas.
- [`RollbackGuard`](src/rollback.rs) — `catch_unwind`-wrapped first-tick guard; restores prior definition + edge state if the new code panics or returns `Err` on its first scheduled batch.
- [`HookSignature` / `extract_hook_signature`](src/hook_signature.rs) — per-language hook-AST extractor (swc for JavaScript, rustpython for Python) that powers the compatible-vs-incompatible classification.

## Composes with

| Crate | Role |
|---|---|
| [`graphreact-core`](../graphreact-core/) | `Scheduler` + `Message` + `replace_definition` + quiescence wait — the substrate the orchestrator drives. |
| [`graphreact-grx-parser`](../graphreact-grx-parser/) | `.grx` re-parse and `GrxIr` shape — the input the diff classifier consumes. |
| [`graphreact-cli`](../graphreact-cli/) | Downstream consumer; wires `FileWatcher` + `ReloadOrchestrator` into `grx run --watch` and bridges to the engine isolates and to `graphreact-dist` for cluster fan-out. |

## Running the tests

```sh
cargo test -p graphreact-hotreload --release
```

48 release tests pass, 2 ignored (long-running watcher fixtures).

## See also

- [M15 milestone narrative](../../docs/backend/m15-hot-reload.md) — full sub-phase log, acceptance numbers, deferrals.
- [Decision 0003 — bifurcation of Backend vs DB products](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md) — why hot reload is a Backend-product property.
- [Decision 0002 — GraphReact is backend-only](../../docs/decisions/0002-graphreact-is-backend-only.md) — substrate scope.
- [Workspace onboarding](../../docs/onboarding.md)
- [Architecture overview](../../docs/architecture.md)
