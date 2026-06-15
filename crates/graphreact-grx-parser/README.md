# graphreact-grx-parser

*Parser + IR builder for the `.grx` markup language — hand-rolled recursive descent plus a tree-sitter incremental backend; PCB-shape CSS substrate and multi-file resolver included.*

## Milestone

Ships across three milestones — all SHIPPED. The crate landed in **M11** (initial recursive-descent port from the TypeScript reference), grew a CSS-shape substrate + multi-file splice + `<Subgraph>`/`<import as>` aliasing in **M11.5** ([narrative](../../docs/backend/m11.5-pcb-shape-substrate.md)), and gained the tree-sitter incremental backend wired into `ParseCache` during **M15** ([narrative](../../docs/backend/m15-hot-reload.md)). No frontmatter milestone post-dates M15 for this crate.

## Role in the runtime

Sits at the substrate tier of the workspace — engine-agnostic, runtime-agnostic, depends on no other `graphreact-*` crate. Every consumer that needs to turn `.grx` source into a mountable graph goes through this crate: the CLI for `grx run` / `grx watch`, the hot-reload pipeline for diff classification, the PCB tooling for `tool:state:render:grx` round-trips. The pipeline is fixed: source → `Lexer` → tokens → `Parser` → `GrxFile` AST → `MultiFileResolver` (cycle detection + import aliasing + `<Subsystem>` splice) → `GrxIr`. The tree-sitter backend at `treesitter.rs` is a swap-in for incremental reparse; both backends produce the same `GrxIr` for downstream consumers.

## Key types

- `GrxFile` / `GrxNode` — AST shape produced by the recursive-descent `Parser`. Matches the TypeScript reference's `Record<string, unknown>` insertion order via `serde_json` `preserve_order`. See [`src/ast.rs`](src/ast.rs).
- `Lexer` / `Token` / `Parser` — hand-rolled lexer + recursive-descent parser. State-aware modes for raw-content blocks and brace-balanced expressions (the reason tree-sitter was a swap-in, not the default). See [`src/lexer.rs`](src/lexer.rs), [`src/parser.rs`](src/parser.rs).
- `GrxIr` / `IrNode` / `IrEdge` — post-resolution intermediate representation that downstream mounting consumes. Enforces the §2.4 first-declares-attrs / rest-id-only collision rule that preserves GraphReact's multi-parent pattern.
- `resolve_multi_file` / `resolve_multi_file_cached` / `ParseCache` — multi-file entry point + mtime-keyed cache used by M15 hot-reload. See [`src/multi_file.rs`](src/multi_file.rs).
- `GrxParseError` — `thiserror`-derived error taxonomy with `SourceLoc` spans, including the §2.3 splice / §2.4 collision / `<import as>` alias variants.

## Composes with

This crate has zero `graphreact-*` dependencies (substrate-tier). Crates that depend on `graphreact-grx-parser`:

| Crate | Role of the dependency |
|---|---|
| [`graphreact-cli`](../graphreact-cli/) | `grx run` / `grx watch` / `grx cluster daemon` parse `.grx` entry points via `resolve_multi_file` before handing the `GrxIr` to the scheduler. |
| [`graphreact-hotreload`](../graphreact-hotreload/) | Diff classifier consumes `GrxIr` snapshots; `ParseCache` + the tree-sitter backend feed the M15 watch loop. |

## Running the tests

```sh
cargo test -p graphreact-grx-parser --release
```

Test count after M11.5 follow-on: **160 lib + 13 integration = 173 tests** (per the M11.5 narrative). Add the tree-sitter Phase 1B suite from M15 on top.

## See also

- [M11.5 narrative — PCB-shape substrate + multi-file splice](../../docs/backend/m11.5-pcb-shape-substrate.md)
- [M15 narrative — hot reload + tree-sitter incremental backend](../../docs/backend/m15-hot-reload.md)
- [Decision 0002 — GraphReact is a backend runtime](../../docs/decisions/0002-graphreact-is-backend-only.md)
- [Decision 0003 — GraphReact bifurcates into Backend and DB](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md)
- [Workspace onboarding](../../docs/onboarding.md)
- [Workspace architecture](../../docs/architecture.md)
