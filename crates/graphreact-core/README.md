# graphreact-core

*Core runtime — `CommunicationGraph` data structure, message-driven scheduler, React-style hooks, OTP-inspired supervision, Leptos-inspired reactive memos, telemetry sink trait.*

## Milestone

Substrate crate accreted across the foundational milestones **M2 (graph)**, **M4 (scheduler + hooks)**, **M5 (supervision)**, and **M7 (telemetry sink)** — all shipped before the per-milestone narrative tradition began with [M13](../../docs/backend/m13-phases.md) / [M14](../../docs/backend/m14-distribution.md). For the product context these milestones live inside, see the [backend product overview](../../docs/backend/README.md) and [decision 0002 — graphreact is backend-only](../../docs/decisions/0002-graphreact-is-backend-only.md).

## Role in the runtime

`graphreact-core` is the bottom of the dependency graph for the Rust workspace — it has **zero `graphreact-*` deps** and is what everything else (engines, distribution, observer, CLI, foreign-language bridges) hangs off. It owns the `CommunicationGraph` data structure, the single-threaded scheduler that drives `process_batch` rounds, the React-shaped hook surface (`use_state`, `use_effect`, `use_emit`, …), the OTP-inspired `Supervisor` / `RestartPolicy` machinery, and the `TelemetrySink` trait that downstream observability layers implement. Engines (`graphreact-jsc-engine`, `graphreact-cpython-engine`) plug component bodies into the scheduler via `SchedulerHost`; distribution (`graphreact-dist`) hooks cross-node delivery via `install_remote_sink`. The crate is **substrate, not engine** — it knows nothing about JS, Python, or the wire format.

## Key types

- **`CommunicationGraph`** ([`src/graph/graph.rs:57`](src/graph/graph.rs)) — the slotmap-backed node + edge store; everything downstream reads/writes through this.
- **`Scheduler` / `SchedulerHandle`** ([`src/runtime/scheduler.rs:80`](src/runtime/scheduler.rs), [`:3197`](src/runtime/scheduler.rs)) — owns the batch loop, the message queue, the cleanup queues, and the install-points for cross-node / supervisor / unmount hooks.
- **`NodeDefinition` / `NodeInstance`** ([`src/runtime/node_instance.rs:32`](src/runtime/node_instance.rs), [`:150`](src/runtime/node_instance.rs)) — the static blueprint vs the mounted instance with hook state.
- **`Supervisor` / `RestartPolicy`** ([`src/runtime/supervision/supervisor.rs:177`](src/runtime/supervision/supervisor.rs), [`:40`](src/runtime/supervision/supervisor.rs)) — OTP-shaped one-for-one / one-for-all / rest-for-one with backoff + max-restart-intensity.
- **`TelemetrySink`** ([`src/telemetry.rs:366`](src/telemetry.rs)) — the single seam through which Prometheus, OTel, the management API ring buffer, and the observer WebSocket all observe runtime events.

## Composes with

`graphreact-core` has no upstream `graphreact-*` deps. Downstream consumers in the workspace:

| Crate | Role downstream of core |
|---|---|
| [`graphreact-algorithms`](../graphreact-algorithms/) | Graph algorithms operating on `CommunicationGraph` views. |
| [`graphreact-graspologic`](../graphreact-graspologic/) | Graspologic-shaped graph stats over the same substrate. |
| [`graphreact-dist`](../graphreact-dist/) | Distribution layer; installs remote sinks on `Scheduler`. |
| [`graphreact-hotreload`](../graphreact-hotreload/) | M15 compatible-change reload preserving hook state + queues. |
| [`graphreact-observer`](../graphreact-observer/) | Management API + telemetry consumer (`TelemetrySink` impl). |
| [`graphreact-cli`](../graphreact-cli/) | The `grx` binary; drives `Scheduler::process_batch`. |
| [`graphreact-napi`](../graphreact-napi/) | Node-API host exposing core to Node.js. |
| [`graphreact-wasm`](../graphreact-wasm/) | WASM host exposing core to browser / Wasmtime. |

## Running the tests

```sh
cargo test -p graphreact-core --release
```

21 integration test files under `tests/` plus extensive in-module unit tests; contributes the bulk of the workspace's 699-test release-profile baseline reported in the [M14 closeout](../../docs/backend/m14-distribution.md#workspace).

## See also

- [Backend product overview](../../docs/backend/README.md) — where these milestones sit in the product roadmap.
- [M14 — Distribution layer](../../docs/backend/m14-distribution.md) — first downstream consumer that exercises the core install-hook surface end-to-end.
- [Decision 0002 — graphreact is backend-only](../../docs/decisions/0002-graphreact-is-backend-only.md) — frames the substrate scope.
- [Decision 0003 — graphreact bifurcates into backend and db](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md) — why core stays neutral of product wrapper.
- [Workspace onboarding](../../docs/onboarding.md) · [Architecture overview](../../docs/architecture.md)
