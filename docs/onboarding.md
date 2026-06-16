# Onboarding — GraphReact (Backend) Rust workspace

**Status:** living document — single entry point for new developers arriving at `graphreactRust/`. Bridges to existing milestone narratives, ADRs, per-crate code, and rustdoc; does not duplicate them.

This doc serves three audiences out of one file (per the Phase-4 decision to keep onboarding consolidated): **ops engineers** running daemons and reading metrics, **backend devs** writing `.grx` topologies and extending crates, and **contributors** changing the substrate itself. Pick the relevant Reading-order subsection and follow the trail.

---

## 30-second pitch

GraphReact (Backend) is a server-side, message-driven, BEAM-class runtime that executes `.grx` topologies — nodes (with bodies in JS / Python / future JVM) connected by edges, supervised OTP-style, scheduled reactively, observed over Prometheus + OTel. The runtime is **backend-only** by commitment ([decision 0002](decisions/0002-graphreact-is-backend-only.md)); a sister product **GraphReactDB** ships the database-shaped surface on the same substrate ([decision 0003](decisions/0003-graphreact-bifurcates-into-backend-and-db.md)). If you wanted to summarize it in one phrase: *Akka / Erlang-OTP / Orleans with a React-shaped DX and a typed actor wire format.*

---

## 5-minute quickstart

Prerequisites: Rust toolchain (stable), macOS or Linux, and:

- **Python 3.12+ headers on PATH** (M13 closeout floored CPython at 3.12 for [PEP 684](https://peps.python.org/pep-0684/)'s per-interpreter GIL; pre-3.12 Pythons hit C-extension state-pollution issues). Bypass entirely by running with `--runtime=null` for engine-free fixtures.

WebKit for JSC on Linux is gated on [M12.3b](backend/m12.3b-webkit-vendoring.md) and not required for the quickstart — on macOS the system JavaScriptCore framework is linked automatically.

```bash
# 1. Build the whole workspace (release; debug works but the engines are slow without optimization)
cargo build --workspace --release

# 2. Run a known-good fixture through the CLI
cargo run --release -p graphreact-cli -- run \
  crates/graphreact-cli/tests/fixtures/python-counter.grx

# 3. Run the full test suite
cargo test --workspace --release
```

What you should see: structured JSON logs on stdout reporting parser → IR → mount → first tick, then per-tick scheduler events as the counter fixture emits messages. Stdout is the canonical telemetry sink unless you pass `--telemetry=json-file:/path` or wire the Prometheus exporter; the format is documented in [backend/m16-architecture.md](backend/m16-architecture.md).

If the Python engine fails to link (no system Python framework, or headers missing), fall back to the null engine for a topology dry-run:

```bash
cargo run --release -p graphreact-cli -- run \
  --runtime=null \
  crates/graphreact-cli/tests/fixtures/python-counter.grx
```

To poke the observer surface while a topology is live, run `grx observer` in a second terminal — the [M16-Backend TUI](backend/m16-tui.md) lists keybinds; the management API listens on `127.0.0.1:9410` by default ([endpoint catalog](backend/m16-management-api.md), [Prometheus metric catalog](backend/m16-prometheus.md)).

Other fixtures worth knowing under `crates/graphreact-cli/tests/fixtures/`:

| Fixture | Exercises |
|---|---|
| `python-counter.grx` | scheduler + `useState` + `useEmit` round-trip |
| `python-pingpong.grx` | two-actor message loop, supervisor restart paths |
| `python-effect.grx` / `python-effect-empty-deps.grx` | `useEffect` dependency-array semantics |
| `python-external-body.grx` + `python-external-body.py` | external-body resolution via `src=` |
| `js-external-body.grx` + `js-external-body.js` | JSC engine + external JS body |
| `python-setter.grx` | setter-side state propagation |
| `python-smoke.grx` / `python-smoke-mixed-rejected.grx` | parser + engine-rejection smoke |
| `cluster/` | M14 distribution wire-protocol fixtures |

---

## Reading order by audience

Three lanes — pick the one that matches what you are about to do. Each entry is one line of rationale.

### Ops engineer (running daemons, scraping metrics, debugging stuck nodes)

1. [this doc](onboarding.md) — orient.
2. [backend/m16-architecture.md](backend/m16-architecture.md) — the observability layer's wire shape (axum API, telemetry sinks, SSE/WS).
3. [backend/m16-management-api.md](backend/m16-management-api.md) — endpoint catalog (`/v1/nodes`, `/v1/messages`, `/v1/health`, etc.).
4. [backend/m16-prometheus.md](backend/m16-prometheus.md) — metric catalog + Grafana dashboard JSON (`m16-grafana-dashboard.json` next to it).
5. [backend/m16-otel.md](backend/m16-otel.md) — OTel span shape + `trace_context` propagation across the cluster.
6. [backend/m16-tui.md](backend/m16-tui.md) — `grx observer` keybinds, recipes for "which node is stuck?", "show me the inbox depth histogram".
7. [backend/m16.5-ingress.md](backend/m16.5-ingress.md) — external HTTP injection model, default-deny manifest gating.
8. [backend/m14-distribution.md](backend/m14-distribution.md) — cluster topology, partition detection, net-tick semantics for ops paging.

### Backend dev (writing `.grx`, understanding the scheduler, adding a feature to a crate)

1. [this doc](onboarding.md) — orient.
2. [backend/README.md](backend/README.md) — product overview + living-doc index.
3. [backend/m13-phases.md](backend/m13-phases.md) — CPython engine bridge, hook prelude (`useState` / `useEffect` / `useEmit` / `useMessage`).
4. [backend/m14-distribution.md](backend/m14-distribution.md) — clustering, location-transparent addressing, the `WIRE_VERSION` floor.
5. [backend/m15-hot-reload.md](backend/m15-hot-reload.md) — IR diff classifier, file watcher, rollback semantics; how `.grx` reloads in-place.
6. [backend/m16.5-ingress-protocol.md](backend/m16.5-ingress-protocol.md) — wire spec for external message injection if you are surfacing a node to outside callers.
7. [backend/m16-debug-cpython.md](backend/m16-debug-cpython.md) / [m16-debug-jsc.md](backend/m16-debug-jsc.md) — per-body source debugging via debugpy / CDP when a hook misbehaves.

### Contributor (adding a new crate / engine / wire-format change)

1. [this doc](onboarding.md) — orient.
2. [backend/README.md](backend/README.md) — the layered crate inventory at the bottom.
3. [decisions/0001-v8-engine-question.md](decisions/0001-v8-engine-question.md) → [0002](decisions/0002-graphreact-is-backend-only.md) → [0003](decisions/0003-graphreact-bifurcates-into-backend-and-db.md) → [0004](decisions/0004-ingress-as-substrate-primitive.md) — the four load-bearing ADRs, in order; read these *before* proposing a structural change.
4. [backend/m11.5-pcb-shape-substrate.md](backend/m11.5-pcb-shape-substrate.md) — the CSS-shape substrate inside `graphreact-grx-parser`; mandatory reading before touching the parser.
5. [backend/m6.6-engine-parity.md](backend/m6.6-engine-parity.md) — TS-feature parity matrix per engine; mandatory before adding a new engine impl behind `graphreact-engine-zig::Engine`.
6. The relevant `crates/<name>/Cargo.toml` + module-level rustdoc — run `cargo doc --workspace --no-deps --open` and read the local code-level surface.

---

## Milestone glossary

Every milestone tag you will see in commits, doc cross-links, or memory entries:

| Tag | Name | Status | Narrative doc |
|---|---|---|---|
| M2 | CommunicationGraph data structure (foundational substrate) | shipped (pre-M14) | no separate narrative; see `graphreact-core` README |
| M3 | NetworkX-parity graph algorithms (components, cycles, DAG, pathfinding, traversal, validation) | shipped (pre-M14) | no separate narrative; rows tagged M3 in [backend/m6.6-engine-parity.md](backend/m6.6-engine-parity.md) |
| M4 | Reactive runtime / scheduler | shipped (pre-M14) | [backend/README.md](backend/README.md) |
| M5 | OTP-inspired supervision | shipped (pre-M14) | no separate narrative; see `graphreact-core` README |
| M6 | NAPI bindings — Node.js polyglot embedding (`@graphreact/native`) | shipped (pre-M14) | no separate narrative; see [backend/m6.6-engine-parity.md](backend/m6.6-engine-parity.md) + ADR 0002 |
| M6.6 | TS feature parity audit, per-engine sign-off | UNSHIPPED (living document; last update 2026-05-02) | [backend/m6.6-engine-parity.md](backend/m6.6-engine-parity.md) |
| M7 | Telemetry sink trait (`TelemetrySink`) — substrate hot-path | shipped (pre-M14) | no separate narrative; load-bearing invariant in [backend/m16-architecture.md](backend/m16-architecture.md) |
| M8 | Linear algebra kernel + partition (Leiden, modularity) | shipped (pre-M14) | no separate narrative; see `graphreact-linalg` + `graphreact-graspologic` READMEs |
| M9 | Spectral embeddings (ASE/LSE/OMNI/MASE/CASE + Procrustes + dimension selection) | shipped (pre-M14) | no separate narrative; see `graphreact-graspologic` rustdoc |
| M10 | Cluster / inference / match / nominate analytics surface (GMM, latent-position, Hungarian+FAQ, spectral VN) | shipped (pre-M14) | no separate narrative; see `graphreact-graspologic` rustdoc |
| M11 | `.grx` parser + IR builder | shipped (pre-M14) | [backend/README.md](backend/README.md) |
| M11.5 | PCBPnR Phase 5-D shape substrate for the parser | SHIPPED 2026-05-26 | [backend/m11.5-pcb-shape-substrate.md](backend/m11.5-pcb-shape-substrate.md) |
| M12 | Engine-substrate trio — engine-agnostic Rust trait + null + echo bridges | shipped (pre-M14) | no separate narrative; see [backend/m6.6-engine-parity.md](backend/m6.6-engine-parity.md) |
| M12.1 | Standalone `grx` CLI skeleton (parse / check / run / fmt / engine selection) | shipped (pre-M14) | no separate narrative; see `graphreact-cli` README |
| M12.3a | JSC (JavaScriptCore) engine bridge (macOS system framework) | shipped (M12.3a phase-track) | [backend/m6.6-engine-parity.md](backend/m6.6-engine-parity.md) |
| M12.3b | WebKit vendoring for JSC on Linux/Windows | UNSHIPPED (living document; last update 2026-05-02) | [backend/m12.3b-webkit-vendoring.md](backend/m12.3b-webkit-vendoring.md) |
| M12.4 | Node-API compatibility shim for JSC bodies — first-class backend NPM ecosystem access | UNSHIPPED (roadmap) | no separate narrative; see [backend/README.md](backend/README.md) milestone table |
| M13 | CPython engine bridge | shipped 2026-05-06 (M13 closeout) | [backend/m13-phases.md](backend/m13-phases.md) |
| M14 | Distribution layer (first BEAM-class property) | shipped 2026-05-05 | [backend/m14-distribution.md](backend/m14-distribution.md) |
| M15 | Hot reload (second BEAM-class property) | shipped 2026-05-07 | [backend/m15-hot-reload.md](backend/m15-hot-reload.md) |
| M16-Backend | Observer + telemetry (third BEAM-class property) | SHIPPED 2026-05-08 | [backend/m16-backend-observer.md](backend/m16-backend-observer.md) |
| M-Ingress | External HTTP ingress substrate (M16.5-Backend) | SHIPPED 2026-05-09 | [backend/m16.5-ingress.md](backend/m16.5-ingress.md) |
| **M17** | **Go runtime engine bridge (third in-process language host; replaces JVM per Java→Go pivot 2026-06-15)** | PLANNED — replanned 2026-06-15 | [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |
| M18 | WebAssembly server-WASM bindings | UNSHIPPED (stub; orthogonal) | [backend/README.md](backend/README.md) + [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |
| **M22** | **Mixed-runtime systems — JS + Python + Go in same `.grx`** | PLANNED — replanned 2026-06-15 | [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |
| **M23** | **LSP server for `.grx` (outer-shell MVP)** | PLANNED — replanned 2026-06-15 | [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |
| **Cleanup-Pass** | **Pre-M-Prod deferred-cleanup slice (13 named residuals)** | PLANNED — replanned 2026-06-15 | [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |
| **M-Prod** | **Production-readiness milestone — 9 sub-tracks (auth / TLS / secrets / lifecycle (incl. M14.11) / observability / rate limiting / supply-chain / packaging / chaos+fuzz)** | PLANNED — replanned 2026-06-15 | [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |
| **M19** | **`graphreact-persistence` — own segment log + snapshot + replay** | PLANNED — replanned 2026-06-15 (DB-product track) | [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |
| **M20** | **`graphreact-db-distributed` — sharded `CommunicationGraph`, Raft per shard** | PLANNED — replanned 2026-06-15 (DB-product track) | [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |
| **M21** | **`graphreact-query` — SpacetimeDB-style reducer-with-subscription (supersedes "Cypher-shaped" wording in ADR-0003 + db/README)** | PLANNED — replanned 2026-06-15 (DB-product track) | [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |
| **M24** | **DB client SDK protocol — WS + binary BSATN; canonical React SDK; typed-binding codegen** | PLANNED — replanned 2026-06-15 (DB-product track) | [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |
| **M25 / M25.a / M25.b** | **Additional client SDKs — Rust → Swift → C#/Unity (per locked Q11)** | PLANNED — replanned 2026-06-15 (DB-product track) | [later-milestone plan](../../../.claude/plans/graphreact-rust-later-milestones.md) |

---

## Crate map (quick navigation)

The workspace is organized as one crate per concern; engine impls are sister crates behind the shared `Engine` trait in `graphreact-engine-zig`.

| Crate | Role | Milestone tag |
|---|---|---|
| `graphreact-core` | CommunicationGraph + scheduler + hooks + supervision + telemetry trait | M2 / M4 / M5 / M7 |
| `graphreact-algorithms` | NetworkX-parity algorithms (components, cycles, DAG, pathfinding, traversal, validation) | M3 |
| `graphreact-grx-parser` | `.grx` lexer + parser + IR + multi-file resolver + PCB-shape CSS substrate | M11 / M11.5 / M15 |
| `graphreact-hotreload` | File watcher + IR diff classifier + apply orchestrator + rollback guard | M15 |
| `graphreact-dist` | Distribution layer: NodeId/Creation, DistRouter, Cluster, Discovery, PartitionDetector | M14 |
| `graphreact-observer` | JSON logs + Prometheus + OTel + axum management API + SSE/WS + ratatui TUI | M16-Backend / M-Ingress |
| `graphreact-cli` | Standalone `grx` binary; engine selection; daemon + observer entry points | M12.1 / M16-Backend |
| `graphreact-engine-zig` | Engine-agnostic Rust trait surface (concrete impls in sister crates) | M12 substrate |
| `graphreact-engine-cpython` | CPython engine — system Python framework, hook prelude | M13 |
| `graphreact-engine-jsc` | JSC engine — macOS JavaScriptCore framework on macOS, WebKit-vendored on Linux | M12.3a / M12.3b |
| `graphreact-engine-echo` | Reference echo engine — validates the FFI surface without a real JS runtime | M12 substrate |
| `graphreact-engine-null` | No-op engine for topology dry-runs and parser→runtime CI smoke | M12 substrate |
| `graphreact-graspologic` | Statistical graph analysis (Leiden, ASE/LSE/OMNI/MASE/CASE, GMM, hypothesis tests) | M8 / M9 / M10 |
| `graphreact-linalg` | Pure-Rust SVD / eigh / gemm / solve on faer | M8 substrate |
| `graphreact-napi` | NAPI bindings published as `@graphreact/native` | M6 / M6.6 |
| `graphreact-wasm` | Server-WASM bindings (stub) — edge compute, embedded hosts, sandboxed plugins | M18 |

The dependency direction is strict: `core` has no internal deps; `algorithms` / `dist` / `observer` / `hotreload` build on `core`; `cli` and `napi` ride on top of everything. Engine impls depend only on `graphreact-engine-zig`.

Where things actually live at runtime (mental model for new devs):

- A **`.grx` file** declares nodes, edges, supervision policies, message channels, and per-node manifests (`accepts_external`, `accepts_external_call`).
- The **parser** (`graphreact-grx-parser`) turns the file into a `GrxIr`.
- The **CLI** (`graphreact-cli`) hands the IR to the runtime, mounts node bodies into the chosen engine, registers them with the **scheduler** (`graphreact-core::Scheduler`), and (if requested) spawns the **management API** (`graphreact-observer::spawn_management_api`).
- Each tick, the scheduler drains node inboxes, dispatches messages, runs hook bodies through the engine, and writes telemetry events to the configured `TelemetrySink` (typically `MultiplexSink` over JSON logs + Prometheus + ring buffer + WS).
- External callers reach nodes via the M-Ingress endpoints; the manifest gate decides whether the message enters the inbox or is 403'd at the API edge.

---

## Doc layer map

Five layers exist; consult the lowest one that answers your question.

| Layer | Path shape | Use when |
|---|---|---|
| Strategic plan | `~/.claude/plans/graphreact-rust-later-milestones.md` (workspace-parent) | You need the M17–M25+ roadmap framing, milestone definitions, or the rationale for what comes next. (Replaces the prior `toasty-crafting-lantern.md`, which is lost on disk; the new plan is a substrate-faithful reconstruction.) |
| Decision docs (ADRs) | `docs/decisions/000N-*.md` | You are asking *why* a structural choice was made (backend-only, bifurcation, ingress shape, V8 omission). Four ADRs total; all four are short. |
| Milestone narratives | `docs/backend/m*.md` | You want to know *what* a milestone shipped, the acceptance criteria it closed against, and the deferrals it consciously left open. |
| Per-crate READMEs | `crates/*/README.md` (where present), otherwise `Cargo.toml` + module-level rustdoc | You are touching one crate and need the local entry points; module-level `//!` docs are the canonical surface where a README is absent. |
| Rustdoc | `cargo doc --workspace --no-deps --open` | You need code-level detail: trait bounds, type signatures, error variants, public-item inventory. |

Rule of thumb: a question of the form *why* hits the ADR layer, *what* hits the milestone narrative, *how* hits rustdoc or the crate-local code.

---

## Sibling projects

**`@graphreact/client-react`** lives at `../../graphreact-visualizer/` — the canonical React client library for the protocols this runtime ships. Hooks for the management API, SSE/WS live-event stream, and the M-Ingress single-shot / call / bulk endpoints. Pinned to runtime commit and shares the `/v1/` wire-format contract verbatim (see [`../../graphreact-visualizer/docs/contract-mirror.md`](../../graphreact-visualizer/docs/contract-mirror.md)).

Per [decision 0002](decisions/0002-graphreact-is-backend-only.md) the runtime ships server-native protocols only; the visualizer is the **downstream** companion that provides the typed React client. Ops engineers writing dashboards around `grx cluster daemon` should start there; see [`../../graphreact-visualizer/docs/onboarding.md`](../../graphreact-visualizer/docs/onboarding.md).

---

## Design philosophy quick-card

Four rules every contributor should keep in their head from day one. Each is load-bearing on real design decisions in the existing tree — violating any of these will produce a PR that gets rewritten on review.

- **Substrate-not-MVP.** Substrate work ships the *complete* primitive set the application space needs across all eight named application classes (Thymus, ComplyFlow, NewsCaster, Generative Worlds, Adaptive Education at scale, Reputation networks, Knowledge graphs that reason, plus the Backend-product baseline). It does NOT ship a phase-1 MVP biased toward the nearest-term project. The cleanest worked example is [M-Ingress decision 0004](decisions/0004-ingress-as-substrate-primitive.md), which rejected a single-shot-only MVP in favor of shipping single-shot + request/response call + bulk NDJSON in one milestone. (Memory backstop: `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/feedback_complete_substrate_not_mvp.md`.)

- **Backend-only.** The runtime exposes server-native protocols (HTTP JSON API, Prometheus, OTel, JSON logs, SSE/WS event stream) and ships zero bundled UI. Dashboards are a downstream consumer concern. See [decision 0002](decisions/0002-graphreact-is-backend-only.md). If you find yourself adding a frontend asset to the runtime tree, you are in the wrong product.

- **Default-deny external ingress.** A node accepts external HTTP messages *only* for types listed in its `accepts_external` (single-shot / bulk) or pairs listed in its `accepts_external_call` (request / response). Pre-M-Ingress, no node accepted anything; post-M-Ingress, existing applications gain zero attack surface from the upgrade. See [decision 0004](decisions/0004-ingress-as-substrate-primitive.md).

- **Hard wire-version floor.** Cluster nodes refuse to interoperate across `WIRE_VERSION` mismatches; mixed-version cluster operation is deferred to M-Prod, *not* shipped opportunistically. The `WIRE_VERSION` constant lives in `crates/graphreact-dist/` and bumped from 3→4 for the M-Ingress `correlation_id` change (mirroring the M16-Backend Phase 10 `trace_context` precedent). See [backend/m14-distribution.md](backend/m14-distribution.md).

---

## Common gotchas (the ones that bite first-week)

A short list of things that have caused real wasted hours; calibrate your defaults to these.

- **Debug builds are slow enough to look broken.** The engines (CPython, JSC) take meaningful time to spin up under `cargo build` without `--release`. If a fixture appears to hang at startup, you forgot `--release`. The test suite is also dramatically faster under `cargo test --workspace --release`.
- **Stdout JSON logs are the canonical sink, not a debug aid.** They are structured, machine-readable, and used by the integration tests. Don't reach for `println!` to add a print — wire a telemetry event instead; the format and conventions are in [m16-architecture.md](backend/m16-architecture.md).
- **Hot reload is opt-in per topology, not per file edit.** `graphreact-hotreload` watches a `.grx` only when the CLI is invoked with the watch surface; rolling back on first-tick failure is automatic and silent. See [m15-hot-reload.md](backend/m15-hot-reload.md) before debugging "why did my edit not take effect."
- **`WIRE_VERSION` mismatches refuse the handshake outright.** Two nodes at different `WIRE_VERSION`s do not interoperate at all (no degraded mode). If a `grx cluster connect` fails immediately with a version error, rebuild both ends from the same commit.
- **External HTTP injection is default-deny.** Calling `POST /v1/nodes/<id>/messages` against a node whose manifest does not list the message type in `accepts_external` returns 403, not "queued". This is intentional ([decision 0004](decisions/0004-ingress-as-substrate-primitive.md)) — surface the type in the manifest first.

---

## Where to go next

- [backend/README.md](backend/README.md) — milestone narrative index + crate inventory.
- [decisions/0001-v8-engine-question.md](decisions/0001-v8-engine-question.md), [0002](decisions/0002-graphreact-is-backend-only.md), [0003](decisions/0003-graphreact-bifurcates-into-backend-and-db.md), [0004](decisions/0004-ingress-as-substrate-primitive.md) — the four ADRs.
- `crates/*/Cargo.toml` + module-level rustdoc — local code-level surfaces; run `cargo doc --workspace --no-deps --open` to browse.
- Strategic plan in the parent workspace (`~/.claude/plans/`) — roadmap framing beyond the shipped milestones.

If a doc cross-link in this tree 404s for you, it is in flight — open an issue (or a PR) rather than working around it. The doc tree is co-versioned with the substrate.
