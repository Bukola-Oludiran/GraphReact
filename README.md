# graphreactRust

Production Rust port of [GraphReact](../graphreact) — a graph-based reactive runtime where autonomous nodes communicate via message passing on a `MultiGraph`.

This is the **production-ready target**. The TypeScript project at [`../graphreact`](../graphreact) remains the **conceptual ideation space**.

## Workspace layout

| Crate | Purpose |
|-------|---------|
| `graphreact-linalg` | Pure-Rust linear algebra kernel (SVD, eigh, gemm, solve, normalize) built on `faer` |
| `graphreact-core` | Graph data structure, scheduler, hooks, supervision (OTP-inspired), reactive memos |
| `graphreact-algorithms` | NetworkX-parity graph algorithms (BFS, DFS, pathfinding, cycles, components, DAG) |
| `graphreact-graspologic` | Spectral embeddings, Leiden, clustering, hypothesis tests, graph matching |
| `graphreact-napi` | NAPI bindings — published as `@graphreact/native` for Node.js consumers |
| `graphreact-wasm` | WASM bindings for browser use |

## Design synthesis

| Layer | Pattern source |
|-------|----------------|
| Lifecycle & ownership | Erlang/OTP — monitor/link, restart strategies, pg-style groups, postpone-style selective receive |
| Reactive data flow | Leptos — Clean/Check/Dirty 3-state propagation, lazy `Memo<T>` |
| Ergonomic primitives | Dioxus — `generational-box` Copy signals, `slotmap` scheduler storage |
| Linear algebra | `faer` |
| Graph analytics | `graspologic` (Python reference) |

## Build

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Status

Pre-alpha. Initial milestone (M0 — workspace scaffolding) in progress. See [the plan](../../../.claude/plans/toasty-crafting-lantern.md) for the full milestone roadmap.
