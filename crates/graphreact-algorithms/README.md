# graphreact-algorithms

*NetworkX-parity graph algorithms (components, cycles, DAG, edges, pathfinding, traversal, validation) over `CommunicationGraph` — pure functions, no scheduler dependency.*

## Milestone

Ships as part of **M3 — SHIPPED**. M3 doesn't have a dedicated milestone narrative; the algorithm surface is summarised in the engine-parity matrix at [../../docs/backend/m6.6-engine-parity.md](../../docs/backend/m6.6-engine-parity.md) (rows tagged `M3` cover pathfinding, topology, traversal, validation), and the substrate-bifurcation decision pins this crate as shared substrate consumed by both products in [../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md).

## Role in the runtime

`graphreact-algorithms` sits at the substrate tier alongside `graphreact-core`, one tier below the engine bridges and the scheduler-hosted runtime. It owns the read-only graph-shape queries the runtime, the CLI, and the engine-side `__graphQueryProxy` helpers all reach for: "what's connected to what", "is there a path", "what's the topological order", "does this graph validate". Every entry point is a pure function over `&CommunicationGraph` returning owned `Vec` / `Result<T>` — no scheduler handle, no async, no mutation, no engine dependency. That deliberate purity is what lets the algorithm surface be **engine-agnostic** per the M6.6 audit: the same crate serves JSC + CPython + NAPI bridges and both the Backend and DB products without a per-engine fork.

## Key types

- [`connected_components`](src/components.rs#L9) — undirected connected-component partitioning over `CommunicationGraph` (plus `is_connected` / `number_of_connected_components`).
- [`topological_sort`](src/dag.rs#L62) — Kahn's-order over a `DirectedAdj` built from edge pairs; returns `DagError::Cycle` on cycle. Companion: `topological_generations`.
- [`shortest_path`](src/pathfinding.rs#L97) / [`astar_path`](src/pathfinding.rs#L231) — BFS shortest-path and A\* with optional `PathHeuristic` + `EdgeFilter`; degenerates to BFS under uniform edge cost when no heuristic is supplied.
- [`bfs_edges`](src/traversal.rs#L23) / `dfs_edges` — edge-yielding traversals (plus node-yielding `bfs_nodes` / `dfs_nodes`); both accept an optional depth limit.
- [`validate_graph`](src/validation.rs#L43) — structural validation returning `Vec<ValidationWarning>` with `WarningLevel` (info / warn / error) tiers.

## Composes with

| Crate | Direction | Role |
|---|---|---|
| [`graphreact-core`](../graphreact-core) | depends on | `CommunicationGraph`, `GraphError`, edge/node id types — the data shape every algorithm here reads. |
| [`graphreact-cli`](../graphreact-cli) | depends on this | `grx` subcommands that introspect `.grx` topology (components, cycles, validation) call into this crate directly. |
| [`graphreact-napi`](../graphreact-napi) | depends on this | NAPI bridge exposes the algorithm surface to JS bodies via `__graphQueryProxy` (see M6.6 follow-on-B). |

## Running the tests

```sh
cargo test -p graphreact-algorithms --release
```

Six per-module integration test files (`components`, `cycles`, `dag`, `edges`, `pathfinding`, `traversal`) under `tests/` plus inline unit tests. Pure-function shape means each module is independently testable without a scheduler fixture.

## See also

- [../../docs/backend/m6.6-engine-parity.md](../../docs/backend/m6.6-engine-parity.md) — engine-parity matrix; rows tagged `M3` track this crate's surface across JSC / CPython / NAPI.
- [../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md) — shared-substrate decision; `graphreact-algorithms` is named as substrate consumed by both products.
- [../../docs/decisions/0002-graphreact-is-backend-only.md](../../docs/decisions/0002-graphreact-is-backend-only.md) — backend-tier framing; algorithms are reachable from CLI/protocol surfaces, never a frontend.
- [../../docs/onboarding.md](../../docs/onboarding.md) — workspace onboarding (crate tour + dev loop).
- [../../docs/architecture.md](../../docs/architecture.md) — runtime architecture diagram (substrate / engine / distribution / observer / cli / foreign tiers).
