# graphreact-napi

*NAPI bindings (published as `@graphreact/native`) — exposes the core runtime, algorithms, and graspologic analytics to Node.js consumers via napi-rs.*

## Milestone

Ships as part of **M6 / M6.6 — SHIPPED**. The Node bridge surface itself landed in M6; per-row feature parity is signed off in the engine-parity matrix at [../../docs/backend/m6.6-engine-parity.md](../../docs/backend/m6.6-engine-parity.md), which tracks every JS-facing hook + ctx surface against its NAPI column. The product framing for Backend-tier embeddings (Node hosts as a polyglot consumer of the same runtime) is pinned by [../../docs/decisions/0002-graphreact-is-backend-only.md](../../docs/decisions/0002-graphreact-is-backend-only.md).

## Role in the runtime

`graphreact-napi` sits at the foreign-tier — the engine-equivalent bridge for Node.js, peer to the in-process engine crates (`graphreact-engine-jsc`, `graphreact-engine-cpython`, `graphreact-engine-zig`) but distributing the runtime out-of-process to a JavaScript host instead of embedding one inside `grx`. It wraps `graphreact-core`'s `Scheduler` + `CommunicationGraph`, the pure-function algorithm surface from `graphreact-algorithms`, and the graspologic analytics from `graphreact-graspologic`, exposing them as napi-rs `#[napi]` classes / functions that compile to a `cdylib` consumed by Node as `@graphreact/native`. The `defineNode` bridge re-enters JS for every component-body invocation; the hooks surface (`useState` / `useEffect` / `useMessage` / `useEmit` / `useGraphQuery` / …) is wired through host-callback closures back into Rust. This is the original polyglot path the M6 audit signed off against — every other engine bridge (JSC, CPython) is parity-tracked relative to what NAPI already exposes.

## Key types

- `JsCommunicationGraph` ([src/graph.rs:11](src/graph.rs#L11)) — JS-visible wrapper over `graphreact_core::CommunicationGraph`; backs the mutator + query surface (addNode/addEdge/setNodeData/…) exported to Node.
- `JsScheduler` ([src/scheduler.rs:234](src/scheduler.rs#L234)) — owns the `Scheduler` instance, drives `start` / `process_batch` / `send` / `stop`, hosts the async-round + source-wakeup + async-cleanup glue, and exposes runtime mutation entry points (`add_node_at_runtime`, `replace_component`).
- `registerNode` / `defineNode` bridge ([src/scheduler.rs:590](src/scheduler.rs#L590)) — converts a JS component definition (`{ render, accepts, emits, height, props }`) into a `NodeDefinition`; the JS function reference is held via `napi::Ref<()>` for the registered closure's lifetime.
- Algorithm wrappers ([src/algorithms.rs](src/algorithms.rs)) — `#[napi]` thin shells over `graphreact-algorithms` (`bfs_nodes`, `bfs_edges`, `dfs_nodes`, `dfs_edges`, `connected_components`, `is_connected`, …) returning owned `Vec<String>` / `bool` to JS.
- Source descriptors ([src/sources.rs](src/sources.rs)) — `timerSource` / `delaySource` / `httpSource` / `watchSource` factories, the canonical NAPI implementation that JSC + CPython parity-tracks against.

## Composes with

| Crate | Direction | Role |
|---|---|---|
| [`graphreact-core`](../graphreact-core) | depends on | `Scheduler`, `CommunicationGraph`, `NodeDefinition`, `Message` — the runtime substrate every JS-visible class wraps. |
| [`graphreact-algorithms`](../graphreact-algorithms) | depends on | pure-function graph queries (components, traversal, pathfinding, validation) re-exported as `#[napi]` functions. |
| [`graphreact-graspologic`](../graphreact-graspologic) | depends on | spectral + community-detection analytics (Leiden, embeddings) re-exported to JS callers. |

No workspace crate depends back on `graphreact-napi` — it terminates at the `cdylib` boundary that Node loads as `@graphreact/native`.

## Running the tests

```sh
cargo test -p graphreact-napi --release
```

The Rust-side test surface is unit-test light (the crate is mostly a bridge layer); TypeScript-side coverage lives under [`test-ts/tests/`](test-ts/tests/) with ten suites (smoke, scheduler, hooks, sources, async, examples, graph-query, ctx-mutations, accept-edge, dynamic-and-validation) driven via vitest against the compiled `index.node`.

## See also

- [../../docs/backend/m6.6-engine-parity.md](../../docs/backend/m6.6-engine-parity.md) — engine-parity matrix; the NAPI column is the reference all other engine bridges are signed off against.
- [../../docs/decisions/0002-graphreact-is-backend-only.md](../../docs/decisions/0002-graphreact-is-backend-only.md) — backend-only framing; NAPI is the canonical Node embedding of the Backend product.
- [../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md) — substrate-bifurcation decision; `graphreact-napi` is a Backend-product distribution surface, not consumed by the DB product.
- [../../docs/onboarding.md](../../docs/onboarding.md) — workspace onboarding (crate tour + dev loop).
- [../../docs/architecture.md](../../docs/architecture.md) — runtime architecture diagram (substrate / engine / distribution / observer / cli / foreign tiers).
