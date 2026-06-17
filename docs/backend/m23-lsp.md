# M23 — LSP server (outer-shell MVP)

**Status: SHIPPED 2026-06-17.** First Shared-substrate milestone after
M-Ingress; surfaces the existing `.grx` parser pipeline (lexer +
hand-rolled / tree-sitter backend + IR builder + multi-file resolver
+ CSS-style desugarer) through the Language Server Protocol so any
editor that speaks LSP (VS Code, Neovim, Emacs, Helix, Zed, …) gets
diagnostics, hover, goto-definition, and completion on `.grx` files
without per-editor plumbing.

The crate is [`crates/graphreact-lsp/`](../../crates/graphreact-lsp/);
the binary is `graphreact-lsp` and reads JSON-RPC frames off stdin.

## Why M23

The `.grx` markup language is the only authoring surface for both
Backend and DB products (see [ADR-0003](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md)).
PCBPnR Phase 5-D §3 added a 24-property CSS desugarer + 6 clip-path
shapes + multi-file `<import as=…>` aliasing + `<Subsystem ref=…>`
splice resolution; even the canonical fixtures now run to multiple
hundred lines and three files. Authoring `.grx` without diagnostics
plus hover plus goto reduces the surface to a notepad-grade
experience and blocks every author who isn't the original spec
writer. M23 closes that gap by piggybacking on LSP — the industry's
settled editor-protocol substrate.

## Architectural shape

```
┌──────────────────────────────────────────────────────────────────┐
│  graphreact-lsp (bin: stdio JSON-RPC)                            │
│   ├─ backend.rs         — LanguageServer trait impl              │
│   ├─ documents.rs       — RwLock<HashMap<Uri, DocumentEntry>>    │
│   ├─ position.rs        — SourceLoc <-> Position/Range adapter   │
│   ├─ catalog.rs         — tag / attr / style catalogs            │
│   ├─ diagnostics.rs     — GrxParseError -> Diagnostic            │
│   ├─ hover.rs           — tag/attr/style/refprop -> Markdown     │
│   ├─ completion.rs      — 4-lane classifier                      │
│   └─ resolve.rs         — Subsystem ref + import alias resolver  │
└──────────────────────────────────────────────────────────────────┘
            │ depends on (rlib)
            ▼
       graphreact-grx-parser
```

The crate is deliberately shell-only: every doc string, range
computation, and resolution routine routes back to existing parser
types. When the parser's surface grows, the LSP grows with it for
free.

## Dependency story

- `tower-lsp-server 0.23` — the community-maintained fork of
  `tower-lsp`. Tracks modern `tower 0.5` (matches the workspace pin)
  and `ls-types ≥ 0.97` which exposes `PositionEncodingKind::UTF8`,
  so we skip the UTF-16 conversion tax.
- `tokio` + `tokio-util` — workspace pins; the LSP flips on
  `io-std` for the stdio bin.
- `tracing` — logs to stderr to keep stdout clean.
- `graphreact-grx-parser` — every diagnostic, hover blurb, and
  goto-definition target routes through here. No new parser logic
  was written for M23.

## Phase decomposition (5 shipped)

| Phase | What it ships |
|---|---|
| 1 | Crate scaffold; `LanguageServer::initialize` + `shutdown` + `did_open` + `did_change` + `did_close`; `DocumentStore`; UTF-8 position encoding advertised. |
| 2 | `from_parse_error()` mapping every `GrxParseError` variant to an LSP `Diagnostic` with stable `code` strings + collision-variant `relatedInformation`. |
| 3 | Hover handler with Markdown for structural tags, PCB tags, attributes per tag, every `style=` property (sourced from `recognized_style_properties()`), and `clip-path` shape functions. |
| 4 | `goto_definition` resolves `<Subsystem ref="…">` to its `<Subgraph name="…">` declaration (in-file or cross-file via aliased `<import as="…">`). |
| 5 | `completion` offers four context-sensitive lanes: tag names from the live IR + built-ins, per-tag attribute names, style property names, and clip-path shape function names. |

Each phase landed as its own commit on the `m23-lsp-outer-shell`
branch.

## Acceptance

- 34 tests green (19 unit + 15 integration across 5 test files).
- `cargo test --workspace --release` passes.
- `cargo clippy -p graphreact-lsp --all-targets -- -D warnings` clean.
- New parser-side accessors added: `recognized_style_properties()`
  and `clip_path_shape_names()` in
  `crates/graphreact-grx-parser/src/style/mod.rs`.

## Cross-refs

- Parser surface: [`crates/graphreact-grx-parser/`](../../crates/graphreact-grx-parser/).
- Tree-sitter incremental reparse: [`crates/graphreact-grx-parser/src/treesitter.rs`](../../crates/graphreact-grx-parser/src/treesitter.rs).
- Style desugarer: [`crates/graphreact-grx-parser/src/style/`](../../crates/graphreact-grx-parser/src/style/).
- Multi-file resolver: [`crates/graphreact-grx-parser/src/multi_file.rs`](../../crates/graphreact-grx-parser/src/multi_file.rs).
- M15 hot-reload narrative: [`m15-hot-reload.md`](m15-hot-reload.md).

## Deferred to M23.next

- Semantic tokens (`textDocument/semanticTokens/full`).
- VS Code extension publishing.
- In-body completions/diagnostics for JS/Python/Go bodies.
- Inlay hints, formatter, find-references.
- Incremental sync via `IncrementalParser::reparse`.
- `relatedInformation` on cross-file errors (needs parser-side loc
  on `CircularImport` / `ImportNotFound` / `Io`).
- LSP-aware document parse cache that invalidates on `did_change`
  rather than disk mtime.
