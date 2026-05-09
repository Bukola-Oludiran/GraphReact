# M15 — Hot reload

**Status:** shipped 2026-05-07. Second BEAM-class property.

[`crates/graphreact-hotreload/`](../../crates/graphreact-hotreload/) is the GraphReact
hot-reload pipeline: file watcher → diff classifier → safe-point detector →
apply orchestrator → rollback guard → engine-bridge integration → CLI watch
mode → cluster-wide propagation. This is the second BEAM-class property
(after M14 distribution); the first runtime feature that makes "edit a `.grx`
and watch the cluster pick it up without restart" routine for application-class
prototypes.

This document is the milestone narrative: what M15 ships, what each sub-phase
added, the acceptance numbers from the closeout, and the known deferrals.
For the per-phase implementation plan see
[`~/.claude/plans/plan-implementation-for-closing-cached-pascal.md`](../../../../.claude/plans/plan-implementation-for-closing-cached-pascal.md).

## Why M15 lights up the application space

Every application class needs live code update without restart:

- **Thymus** self-healing — policy updates roll out across the cluster mid-load,
  rebalancing supervisor decisions without a cold restart.
- **ComplyFlow** regulatory rules — auditors push compliance-rule updates;
  the cluster picks them up across nodes within 500ms.
- **NewsCaster** + generative worlds + adaptive education — game / world /
  curriculum logic edited live without disconnecting players.
- **Eight named classes** (per [project_graphreact_application_space.md](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_application_space.md))
  all gain "edit-without-restart" as a baseline ergonomic.

Without M15, every application-class prototype faced "restart on every edit"
or "restart on every cluster member edit" — both unacceptable for the
prototype-velocity argument the runtime makes.

## Architectural shape

```
┌────────────────────────────────────────────────────────────────────┐
│  graphreact-cli (`grx run --watch`)                                │
│   ├─ FileWatcher (notify::RecommendedWatcher)                      │
│   ├─ ParseCache (mtime-keyed; M15 Phase 1B: tree-sitter Tree too) │
│   ├─ resolve_multi_file_cached → (parser-backend) → GrxIr         │
│   ├─ classify(prev_ir, new_ir, hook_sig_cache) → ReloadDelta      │
│   ├─ ReloadOrchestrator::apply(scheduler, delta)                  │
│   ├─ RollbackGuard (catch_unwind + Err)                           │
│   └─ JscReloadAdapter / PyReloadAdapter (per-isolate state)       │
└────────────────────────────────────────────────────────────────────┘
              │ ships pre-resolved IR                       ▲ peer apply
              ▼ for cluster fan-out                          │ via reload sink
     ┌────────────────────────────────────────────────────────────────┐
     │  graphreact-dist (M15 Phase 10)                                │
     │   Cluster::reload_module(ir_payload, file_path, version)      │
     │   ├─ SHA-256 the payload                                      │
     │   ├─ filter peers by RELOAD_V1_CAPABILITY + PeerStatus::Healthy│
     │   ├─ ship Packet::ReloadModule (WIRE_VERSION = 2)             │
     │   └─ return ClusterReloadOutcome { shipped, skipped_*, ...}   │
     │                                                                │
     │   dispatch_inbound: Packet::ReloadModule → reload_sink.dispatch│
     │   install_remote_reload_sink(callback) — higher-layer apply    │
     └────────────────────────────────────────────────────────────────┘
              │                                       │
              ▼ Send + Sync ↔ !Send bridge            │
     ┌────────────────────────────────────────────────────────────────┐
     │  graphreact-cli::dist_reload_bridge (M15 Phase 10)             │
     │   make_reload_bridge() → (callback, ReloadCommandReceiver)     │
     │   Mpsc channel hop; runtime thread polls + decodes + applies   │
     └────────────────────────────────────────────────────────────────┘
```

## Sub-staging

The plan-implementation file split M15 into 11 phases (with tactical reordering
during execution; the final shipping order is shown below):

| Phase | Slice | Status | What it added |
|---|---|---|---|
| **2** | crate skeleton + file watcher | shipped | `notify::RecommendedWatcher`-backed `FileWatcher` with atomic-save coalescing (50ms window) |
| **4** | diff classifier (later upgraded via Phase 3) | shipped | `ReloadDelta { Compatible / Incompatible / TopologyOnly / Mixed }`; `classify(before, after, cache) → ReloadDelta` |
| **5** | apply orchestrator | shipped | `ReloadOrchestrator::apply(scheduler, delta)`; `replace_definition` for compat; drain+remount for incompat; edge mutations via `pending_topology` |
| **6** | safe-point detection | shipped | `wait_for_global_quiescence(scheduler, timeout)`; mirrors `process_batch`'s exit condition |
| **7** | rollback (Err + Rust panic) | shipped | `RollbackGuard` + `catch_unwind` envelope; `panic_payload_message` helper; restoring snapshots is itself wrapped defensively |
| **8** | engine-bridge integration | shipped | `JscReloadAdapter` / `PyReloadAdapter` — share `Rc<RefCell<Engine>>` + `IsolateHandle` with the runtime so per-isolate state (token registries, MEMO_REGISTRY, NODE_TOKENS) survives swap |
| **9** | CLI watch mode + Command::Reload stub | shipped | `grx run --watch <path>`; integrates `FileWatcher` so saves trigger the pipeline; SIGINT exit is clean |
| **3** | per-language hook-AST diffing (the precision win) | shipped | `swc_ecma_parser` for JS bodies, `rustpython-parser` for Python; `HookSignature` produces `(hook_kind, dep_arity)` tuples; conservative-fallback on parse failure |
| **1A** | ParseCache + multi-file caching | shipped | `FxHashMap<PathBuf, (mtime, GrxFile)>` mtime-keyed; cache-aware sibling of `resolve_multi_file` with hit/miss stats |
| **1B** | tree-sitter grammar swap (substrate-not-MVP: parallel backend) | shipped | `tree-sitter-grx/` workspace crate (grammar.js + scanner.c + Rust binding); `parse_to_ir_via_treesitter` + `IncrementalParser::reparse`; `ParseCache` widened to also cache `tree_sitter::Tree` |
| **10** | cluster propagation (M14 integration) | shipped | `PacketType::ReloadModule = 0x40` + `ReloadModulePacket` + `WIRE_VERSION` 1→2; `reload-v1` capability filter; `Cluster::reload_module(ir_payload, file_path, version)`; `ChallengeAck.server_capabilities` echoes peer caps to client |
| **11** | reload SLO under load | shipped | `reload_under_sustained_load_converges_within_500ms`; 3-node cluster sustaining ≥1k msg/sec, reload mid-load, peers converge within 500ms, supervisor + group state intact |

### Tactical reordering rationale

Phase 1B was originally flagged as "deferred multi-session foundation work"
because tree-sitter grammar authoring + C external scanner is meaningful
build complexity for a markup language with state-aware lexing. Per the
substrate-not-MVP principle, when 1B finally landed it shipped as a **parallel**
backend (not a swap) — the hand-rolled parser stays the default for `parse_to_ir`,
and the tree-sitter path is opt-in via `parse_to_ir_via_treesitter` and
`IncrementalParser`. This sidestepped the "all 50 existing parser tests must
stay green through a swap" gate while still delivering the perf win
(22× faster reparse on a 200-line synthetic `.grx`) and the LSP-ready
substrate.

Phase 3 (per-language hook-AST diffing) shipped LATE in the order because it
upgrades Phase 4's classifier (string-literal-only changes preserve `useState`
instead of forcing drain+remount). Earlier phases used a conservative
body-hash heuristic; Phase 3 made the classifier precision-correct.

## Acceptance numbers (closeout 2026-05-07)

- **804 release tests pass** workspace-wide; 0 failed; 4 ignored.
  (Baseline before M15 close: 714 from M14.10. Net add over M15: +90.)
- **Clippy clean release + dev** under workspace `-D warnings`.
- **M14 cross-node throughput** bench: 295–326k msg/sec (≥285k floor maintained).
- **Reload SLO under load**: bob convergence ≈ 89µs, carol ≈ 86µs (5000× headroom
  under the 500ms gate); 110k msg/sec sustained aggregate (well above the
  ≥1k floor); supervisor + group state intact post-reload.
- **Phase 1B perf win**: 22× faster reparse (full=32ms vs incremental=1.4ms)
  on a 200-line synthetic `.grx` (plan-targeted ≥10×).
- **UTF-8 parity-win**: tree-sitter backend correctly preserves multi-byte
  UTF-8 sequences in definition bodies (the hand-rolled lexer has a
  pre-existing `ch as char` byte-cast bug that mangles `→` etc; tree-sitter
  is strictly more correct).

## Wire-protocol highlights (M15-side)

- **`PacketType::ReloadModule = 0x40`** + `ReloadModulePacket { origin, file_path,
  ir_hash, ir_payload, version }`. `origin` is the bare node name (no `@host`
  suffix); `ir_hash` is SHA-256 over `ir_payload`; `ir_payload` is JSON-encoded
  `GrxIr` (mirrors `Message.payload`'s outer-postcard / inner-JSON split —
  postcard can't serialise `serde_json::Value` used by `AttrMap`).
- **`WIRE_VERSION` 1 → 2 + `MIN_SUPPORTED_VERSION` 1 → 2** — hard floor; v1↔v2
  mixing rejected at handshake. M-Prod's SemVer policy commits to back-compat
  once the substrate stabilises.
- **`ChallengeAck.server_capabilities: Vec<String>`** — closes the M14.3
  substrate gap where client-side `client_handshake` returned empty
  `peer_capabilities`. Both sides now learn peer caps regardless of who dialled.
- **`reload-v1` capability**: peers without it are skipped cleanly on cluster
  reload (operator warning logged; connection stays up — graceful mixed-version
  operation within v2).

## Known deferrals (M-Prod track)

Documented in the plan; M15 ships feature-complete + correctness-tested,
M-Prod adds production-shaped hardening:

| Item | Reason |
|---|---|
| Adversarial fuzzing of the diff classifier | M-Prod owns fuzzing across all surfaces uniformly |
| Signed `ReloadModulePacket` + replay defence beyond per-origin counter | M-Prod's third-party security audit |
| Peer-side re-parse + hash-verify on cluster reload | M-Prod hardening; v1 is origin-authoritative |
| `grx reload <path>` against a remote daemon over the management API | unblocks at M16-Backend ship |
| Reload-under-load 7-day soak | M-Prod's chaos-injection track; Phase 11's harness lifts cleanly |
| Reload SLO surfaced via Prometheus / OTel | M16-Backend (downstream consumption) |

## Cross-references

- **Phase 1A memory**: [project_graphreact_m15_phase1_multifile_caching.md](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m15_phase1_multifile_caching.md)
- **Phase 1B memory**: [project_graphreact_m15_phase1b.md](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m15_phase1b.md)
- **Phase 10 memory**: [project_graphreact_m15_phase10.md](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m15_phase10.md)
- **Phase 11 memory**: [project_graphreact_m15_phase11.md](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m15_phase11.md)
- **Aggregate M15 memory**: [project_graphreact_m15.md](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m15.md)
- **Implementation plan**: [`~/.claude/plans/plan-implementation-for-closing-cached-pascal.md`](../../../../.claude/plans/plan-implementation-for-closing-cached-pascal.md)
- **Canonical strategic plan**: [`~/.claude/plans/toasty-crafting-lantern.md`](../../../../.claude/plans/toasty-crafting-lantern.md)
- **M14 distribution narrative**: [m14-distribution.md](m14-distribution.md) — substrate Phase 10 builds on
- **M-Prod policy** in plan file (~L1245)
