# Cleanup-Pass milestone

**Status:** CLOSED 2026-06-17. 6 items shipped, 2 retired, 6 deferred to named successor milestones.

## Acceptance

Per the strategic plan, Cleanup-Pass closes when "all 14 items either shipped, explicitly retired, or moved to a named milestone." This document is the canonical close-out record.

## Scope (per the strategic plan)

The Cleanup-Pass slice held 14 named small-scope residuals across M11.5 / M13 / M14.11 / M15 / M16 / M6.6 / visualizer. A 7-cluster survey workflow ranked the 14 items by ROI, blockers, and milestone fit; user input directed the final scope cut.

## Disposition table

### Shipped in Cleanup-Pass (6)

| ID | Title | Anchor | Commit |
|---|---|---|---|
| `useLiveEvents-rest-polling` | `useLiveEvents` REST-polling fallback (`transport: 'poll'`) | [`graphreact-visualizer/src/lib/hooks/useLiveEvents.ts`](https://github.com/) and [docs/hooks/useLiveEvents.md](https://github.com/) (sibling repo) | `fc557b2` (sibling) |
| `m115-css-units` | CSS `cm` / `in` / `px` length units in the style desugarer | [`crates/graphreact-grx-parser/src/style/units.rs`](../../crates/graphreact-grx-parser/src/style/units.rs) | `7a7c0b8` |
| `m115-import-alias` | Transitive-import alias-chain propagation | [`crates/graphreact-grx-parser/src/multi_file.rs`](../../crates/graphreact-grx-parser/src/multi_file.rs) `MultiFileResolved::collect_root_subgraphs` | `bb1e4e5` |
| `m66-method-style-ctx` | Method-style `ctx.spawnChild/.compose/.unmount/.acceptEdge` across JS / Python / Go | [`docs/backend/m6.6-engine-parity.md`](m6.6-engine-parity.md) | `d662dbf` |
| `m115-tree-sitter-splice` | Tree-sitter Subsystem splice parity (route-a refactor) | [`crates/graphreact-grx-parser/src/treesitter.rs`](../../crates/graphreact-grx-parser/src/treesitter.rs) `build_grx_file_from_tree` | `26dfa03` |
| `m16-mount-path-consolidation` | Drop conditional `mount_python_definitions` vs `mount_with_scheduler` split in `commands.rs` | [`crates/graphreact-cli/src/commands.rs`](../../crates/graphreact-cli/src/commands.rs) build_scheduler_host Cpython arm | `22e6014` |

### Retired (2)

| ID | Title | Reason |
|---|---|---|
| `m13-ci-matrix-go` | Per-OS / per-Go-minor CI matrix | Already shipped via M17 phase 3h — see [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) `test-go` job (Linux/macOS × Go 1.24/1.25/1.26). No additional work. |
| `m14-11-remote-restart` | Cross-node remote restart wire protocol | Re-scoped into M-Prod sub-track #4 (lifecycle) per locked decision Q9. Named in [`ADR-0005`](../decisions/0005-third-runtime-is-go-not-jvm.md), [`architecture.md`](../../docs/architecture.md), and the M-Prod entry of the strategic plan. |

### Deferred to named milestones (6)

| ID | Title | Defer to | Reason |
|---|---|---|---|
| `m115-tool-state-query-selector` | CSS-selector grammar for `tool:state:query` inputs | **Phase 5-F** (rules / net-class substrate) | Per user direction: defer the CSS-selector grammar. Decision was "pending" in the m11.5 doc; locked as "Phase 5-F dependency" — selector grammar becomes load-bearing only when rules-substrate query patterns demand it. Current v1 colon-prefix surface (`component:<id>`, `net:<name>`, `role:<Role>`, `all`) is acceptable for present callers. |
| `m13-3i-locking` | Zig-side `state.*` mutation-site locking refactor (~30 sites in `cpython.zig`) | **M-Prod concurrency sub-track** (existing M13-phase3j tag) | ~30 mutation sites + deadlock-under-recursive-PyObject-re-entry risk + meaningful test signal requires `python3.14t` in CI (still pending free-threaded `actions/setup-python` support). Engine_lock() removal is its own coordinated effort. Source already tags this as "separable M13-phase3j refactor"; Cleanup-Pass routes it into M-Prod concurrency. |
| `m13-engine-split` | Per-engine binary split (`grx-jsc` / `grx-py` / `grx-go` via `grx` shim) | **M-Prod distribution sub-track** | ~600 LOC / 6 files; 4-way `grx-py` slice multiplier (3.12 / 3.13 / 3.14 / 3.14t); platform gating for `grx-jsc` (macOS-only without vendored WebKit per `m16-jsc-remote-inspector` blocker); shim-vs-feature-flag design call. Release-engineering shaping, not Cleanup-Pass refactor. |
| `m15-grx-reload-remote` | `grx reload <path>` against a remote daemon over management API | **M16-Backend management-API expansion** | Cluster fan-out side already shipped (`Cluster::reload_module` + `Packet::ReloadModule` + `dist_reload_bridge`). Missing pieces are a `POST /v1/reload` management-API route + the CLI HTTP client — both M16-Backend ingress concerns. |
| `m16-jsc-remote-inspector` | JSC RemoteInspectorServer cross-platform | **M12.3b-phase4 (vendored WebKit)** | Hard-blocked: JSC links macOS-only `JavaScriptCore.framework`. Linux / Windows attach is structurally impossible until vendored WebKit lands. The inspector ceiling is bounded by the engine ceiling. |
| `m16-debugpy-ci-overhead` | debugpy CI overhead test (<5% perf delta when listener idle) | **M-Prod debug-overhead gates** | Plan-locked at <1% (not 5%) per [m16-debug-cpython.md:148](m16-debug-cpython.md). Requires debugpy installed in CI runner + a criterion harness over 10K-message bench. Heavy CI commit; M-Prod overhead-gate territory. |

## Generalization principle

User direction surfaced during the milestone: "M11.5 came after work on PCBPnR needed it. Implementations should generalize." This was honored on all M11.5 items shipped (`m115-css-units`, `m115-import-alias`, `m115-tree-sitter-splice`) — implementations live in `graphreact-grx-parser` and serve any consumer of the `.grx` language, not the PCB domain.

## Cross-references

- [Strategic plan](../../../../.claude/plans/graphreact-rust-later-milestones.md) §Cleanup-Pass — original 14-item table.
- [m11.5-pcb-shape-substrate.md](m11.5-pcb-shape-substrate.md) — three M11.5 deferral rows strikethrough'd in the "What did NOT ship" section.
- [m6.6-engine-parity.md](m6.6-engine-parity.md) — method-style ctx row updated from ⚠️ partial to ✓ shipped.
- [m13-phases.md](m13-phases.md) — `M13-phase3j` locking refactor tag preserved; defer target documented above.
- [m16-debug-cpython.md](m16-debug-cpython.md) — mount-path consolidation Footgun row strikethrough'd; debugpy CI overhead defer reaffirmed.
- [ADR-0005](../decisions/0005-third-runtime-is-go-not-jvm.md) — M14.11 absorption into M-Prod #4 (already documented).
