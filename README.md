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

Pre-alpha. M-Ingress (M16.5-Backend) shipped 2026-05-09; backend product is at M16-Backend SHIPPED + M-Ingress + P11.1, with later-milestone roadmap M17–M25+ replanned 2026-06-15 (M17 Go-runtime + M22 mixed-runtime + M23 LSP + Cleanup-Pass + M-Prod + the DB-product track M19/M20/M21/M24 + per-language SDKs M25/M25.a/M25.b). See [the later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) for the full roadmap.

## Agentic Cloud

GraphReact can serve as the execution and data plane of an Agentic Cloud. The
current capability boundary, missing cloud layers, and recommended delivery
order are documented in [GraphReact Rust as an Agentic Cloud
substrate](docs/agentic-cloud.md).
