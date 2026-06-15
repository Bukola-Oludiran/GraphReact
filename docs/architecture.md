# graphreactRust — architecture

**Status:** living document; last update 2026-06-15. Reflects the M14 → M15 → M16-Backend → M-Ingress arc shipped through 2026-05-09.

The architectural map of the `graphreactRust` workspace: crate tiers, runtime data flow, and the load-bearing invariants every milestone has had to respect. Read this before opening individual crate READMEs; it tells you which tier each crate sits in and what edges across tiers are legal. Companion to [`onboarding.md`](onboarding.md).

---

## Thesis

`graphreactRust` is a React-shaped message-driven scheduler over a `CommunicationGraph` data structure, with engine-pluggable hosts (JSC / CPython / null / echo / future JVM) sitting behind a single Rust `Engine` trait, and an Erlang-cluster-equivalent distribution layer underneath. The substrate composes three properties the BEAM popularized — supervised reactive runtime (M4), distribution (M14), hot reload (M15) — plus observer + telemetry (M16-Backend) and an HTTP ingress primitive (M-Ingress) shipped through 2026-05-09. None of those properties are bolted on: each landed as a substrate-shaped milestone with the full primitive set, not as an MVP slice. The result is a backend runtime where the same `.grx` topology runs single-process for prototyping, scales to multi-host clusters with location-transparent addressing, hot-reloads node bodies without dropping in-flight messages, exposes a Prometheus + OTel + WebSocket observer surface, and accepts external HTTP injection through default-deny manifest gates — without the application author touching any of those layers directly.

---

## Crate tiers

Sixteen crates across seven tiers. Edges across tiers go strictly downward (engine bridges depend on the engine trait, never the other way). The CLI is the single binary entry point and depends on the union of every other crate except the foreign-binding tier.

```
┌──────────────────────────────────────────────────────────────────────────┐
│  BINARY ENTRY POINT                                                      │
│   graphreact-cli                       (grx CLI + M-Ingress daemon)      │
└──────────────────────────────────────────────────────────────────────────┘
            │                                                  │
            ▼                                                  ▼
┌──────────────────────────────┐               ┌───────────────────────────┐
│  OBSERVABILITY               │               │  HOT RELOAD               │
│   graphreact-observer        │               │   graphreact-hotreload    │
│   (M16-Backend / M-Ingress)  │               │   (M15)                   │
└──────────────────────────────┘               └───────────────────────────┘
            │                                                  │
            ▼                                                  ▼
┌──────────────────────────────┐               ┌───────────────────────────┐
│  DISTRIBUTION                │               │  ENGINE BRIDGES           │
│   graphreact-dist            │               │   graphreact-engine-jsc   │
│   (M14)                      │               │   graphreact-engine-cpython│
│                              │               │   graphreact-engine-echo  │
│                              │               │   graphreact-engine-null  │
│                              │               │   graphreact-engine-zig   │
│                              │               │   (trait surface)         │
└──────────────────────────────┘               └───────────────────────────┘
            │                                                  │
            └───────────────────┬──────────────────────────────┘
                                ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  SUBSTRATE                                                               │
│   graphreact-core       (scheduler, hooks, supervision, telemetry sink)  │
│   graphreact-grx-parser (.grx → IR; recursive descent + tree-sitter)     │
│   graphreact-algorithms (NetworkX-parity graph algorithms)               │
│   graphreact-graspologic(Leiden, ASE/LSE/OMNI/MASE/CASE, GMM, tests)     │
│   graphreact-linalg     (faer-backed SVD / eigh / gemm / solve)          │
└──────────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────────┐
│  FOREIGN BINDINGS (sibling consumers, not on the runtime path)           │
│   graphreact-napi   (@graphreact/native — Node.js via napi-rs)           │
│   graphreact-wasm   (M18 stub — server-WASM entry point)                 │
└──────────────────────────────────────────────────────────────────────────┘
```

### Substrate (5)

The pure-Rust core: no engine, no scheduler customization, no network. Everything above depends on this tier; nothing in this tier depends on anything above.

| Crate | One-liner | Milestone | README |
|---|---|---|---|
| `graphreact-core` | Core runtime — `CommunicationGraph` data structure, message-driven scheduler, React-style hooks, OTP-inspired supervision, Leptos-inspired reactive memos, telemetry sink trait. | M2/M4/M5/M7 | [`../crates/graphreact-core/README.md`](../crates/graphreact-core/README.md) |
| `graphreact-grx-parser` | Parser + IR builder for the `.grx` markup language — hand-rolled recursive descent plus a tree-sitter incremental backend; PCB-shape CSS substrate and multi-file resolver included. | M11 / M11.5 / M15 | [`../crates/graphreact-grx-parser/README.md`](../crates/graphreact-grx-parser/README.md) |
| `graphreact-algorithms` | NetworkX-parity graph algorithms (components, cycles, DAG, edges, pathfinding, traversal, validation) over `CommunicationGraph` — pure functions, no scheduler dependency. | M3 | [`../crates/graphreact-algorithms/README.md`](../crates/graphreact-algorithms/README.md) |
| `graphreact-linalg` | Linear algebra kernel — pure-Rust SVD, symmetric eigendecomposition, gemm, solve, row L2 normalization built on faer. | M8 substrate | [`../crates/graphreact-linalg/README.md`](../crates/graphreact-linalg/README.md) |
| `graphreact-graspologic` | Graspologic-equivalent statistical graph analysis — Leiden partitioning, spectral embeddings (ASE/LSE/OMNI/MASE/CASE), GMM clustering, hypothesis tests, graph matching, vertex nomination. | M8/M9/M10 | [`../crates/graphreact-graspologic/README.md`](../crates/graphreact-graspologic/README.md) |

### Engine bridges (5)

One trait, four concrete impls, plus a reference echo bridge that validates the FFI surface end-to-end without a real engine linked. The trait surface (`graphreact-engine-zig`) is engine-agnostic; concrete impls each wrap a Zig FFI shim against the actual interpreter.

| Crate | One-liner | Milestone | README |
|---|---|---|---|
| `graphreact-engine-zig` | Engine-agnostic Rust trait surface for embedding host languages (JSC / CPython / V8 / JVM) — concrete impls live in sister `graphreact-engine-*` crates. | M12 substrate | [`../crates/graphreact-engine-zig/README.md`](../crates/graphreact-engine-zig/README.md) |
| `graphreact-engine-null` | Null engine — implements the `Engine` trait without executing guest code; default for `grx run --runtime=null` topology dry-runs and CI smoke coverage of the parser→runtime path. | M12 substrate | [`../crates/graphreact-engine-null/README.md`](../crates/graphreact-engine-null/README.md) |
| `graphreact-engine-echo` | Reference echo engine — validates the Rust↔native FFI surface end-to-end via a C/Zig bridge without requiring a real JS engine linked. | M12 substrate | [`../crates/graphreact-engine-echo/README.md`](../crates/graphreact-engine-echo/README.md) |
| `graphreact-engine-jsc` | JSC (JavaScriptCore) engine bridge — wraps `zig/bridge/jsc.zig`, links the macOS system JavaScriptCore framework, ships real bidirectional JS↔Rust callbacks and hook semantics. | M12.3a / M12.3b | [`../crates/graphreact-engine-jsc/README.md`](../crates/graphreact-engine-jsc/README.md) |
| `graphreact-engine-cpython` | CPython engine bridge — wraps `zig/bridge/cpython.zig` and links the system Python framework; ships real eval/resolve/call + Python `useState`/`useEffect`/`useEmit`/`useMessage` hook prelude. | M13 | [`../crates/graphreact-engine-cpython/README.md`](../crates/graphreact-engine-cpython/README.md) |

### Distribution (1)

| Crate | One-liner | Milestone | README |
|---|---|---|---|
| `graphreact-dist` | Distribution layer — Erlang-cluster-equivalent multi-host clustering: location-transparent addressing, distributed monitors/links/pg groups, net-tick partition detection over length-prefixed postcard on TLS. | M14 | [`../crates/graphreact-dist/README.md`](../crates/graphreact-dist/README.md) |

### Hot reload (1)

| Crate | One-liner | Milestone | README |
|---|---|---|---|
| `graphreact-hotreload` | Hot reload (second BEAM-class property) — file watcher, IR diff classifier, apply orchestrator, rollback on first-tick failure; per-language hook-AST diffing via swc + rustpython. | M15 | [`../crates/graphreact-hotreload/README.md`](../crates/graphreact-hotreload/README.md) |

### Observability (1)

| Crate | One-liner | Milestone | README |
|---|---|---|---|
| `graphreact-observer` | Observer + telemetry substrate — JSON logs, Prometheus exporter, OTel tracing, axum-based management API (incl. M-Ingress endpoints), SSE/WS event stream, ratatui TUI client. | M16-Backend / M-Ingress | [`../crates/graphreact-observer/README.md`](../crates/graphreact-observer/README.md) |

The observer surface (Prometheus / SSE/WS / management API) has a typed React consumer at [`../../graphreact-visualizer/`](../../graphreact-visualizer/) — see decision [0002](decisions/0002-graphreact-is-backend-only.md) for the no-bundled-UI commitment and why the consumer lives downstream.

### CLI (1)

| Crate | One-liner | Milestone | README |
|---|---|---|---|
| `graphreact-cli` | Standalone `grx` CLI — parses and runs `.grx` files; engine selection (JSC / CPython / null) lives behind the engine-zig trait; hosts the M-Ingress daemon + observer TUI. | M12.1 / M16-Backend | [`../crates/graphreact-cli/README.md`](../crates/graphreact-cli/README.md) |

### Foreign bindings (2)

Sibling consumers of the substrate, not on the runtime data path. Both embed the core runtime; neither participates in distribution or hot reload.

| Crate | One-liner | Milestone | README |
|---|---|---|---|
| `graphreact-napi` | NAPI bindings (published as `@graphreact/native`) — exposes core runtime, algorithms, and graspologic analytics to Node.js consumers via napi-rs. | M6/M6.6 | [`../crates/graphreact-napi/README.md`](../crates/graphreact-napi/README.md) |
| `graphreact-wasm` | WebAssembly bindings (M18 stub) — server-WASM entry point for edge compute, embedded host runtimes, sandboxed plugins (NOT a browser-frontend runtime). | M18 | [`../crates/graphreact-wasm/README.md`](../crates/graphreact-wasm/README.md) |

---

## Runtime data flow

A `.grx` file enters at the CLI and ends as either telemetry events out the observer surface or a wire packet out the distribution layer. The scheduler is the only place all paths converge.

```
                        ┌──────────────────┐
                        │  source.grx file │
                        └────────┬─────────┘
                                 │
                                 ▼
            ┌─────────────────────────────────────────┐
            │  graphreact-grx-parser                  │
            │   Lexer → Parser → GrxIr                │
            │   resolve_multi_file + ParseCache       │
            └────────────────────┬────────────────────┘
                                 │ GrxIr
                                 ▼
            ┌─────────────────────────────────────────┐
            │  graphreact-cli                         │
            │   mount IR nodes onto Scheduler         │
            │   pick engine via --runtime=...         │
            └────────────────────┬────────────────────┘
                                 │ NodeDefinition (with body src)
                                 ▼
            ┌─────────────────────────────────────────┐
            │  graphreact-engine-{jsc,cpython,null,…} │
            │   evaluate body via Engine trait        │
            │   register hook callbacks back to core  │
            └────────────────────┬────────────────────┘
                                 │ HookValue / HostValue
                                 ▼
       ┌─────────────────────────────────────────────────┐
       │  graphreact-core::Scheduler                     │
       │   deliver_messages → eval_dirty → emit_outbound │
       │   loop until quiescent; supervision on panic    │
       └──┬──────────────────────────────────────────┬───┘
          │ TelemetryEvent                           │ outbound Message
          ▼                                          ▼
 ┌──────────────────────────┐         ┌───────────────────────────────┐
 │ graphreact-observer       │         │ graphreact-dist               │
 │  MultiplexSink ─►          │         │  DistRouter resolves local   │
 │    JsonLogTelemetry        │         │  vs remote NodeId; postcard  │
 │    PrometheusSink          │         │  encode + length-prefix +    │
 │    OtelSink                │         │  TLS framed write to peer    │
 │    WebSocket broadcast     │         └────────────────┬──────────────┘
 │    SSE event stream        │                          │
 │    M-Ingress management    │                          ▼
 │     API (axum)             │                ┌──────────────────────┐
 └──────────┬─────────────────┘                │  remote graphreact-  │
            │                                  │  cli daemon          │
            ▼                                  └──────────────────────┘
   ┌────────────────────┐
   │ operator surfaces  │
   │  • Prometheus pull │
   │  • ratatui TUI     │
   │  • WS subscribers  │
   │  • external HTTP   │
   │    POST → ingress  │
   └────────────────────┘
```

External HTTP ingress flows the other way through the same observer-hosted management API: `POST /v1/nodes/:id/inject` and `POST /v1/nodes/:id/call` land in axum handlers, are gated by the target node's `accepts_external` / `accepts_external_call` manifest, ride a `correlation_id` on `Message` for the call shape, and resolve via `PendingCalls` registered as `oneshot::Sender<Value>`. See [`backend/m16.5-ingress.md`](backend/m16.5-ingress.md) for the milestone narrative and [`decisions/0004-ingress-as-substrate-primitive.md`](decisions/0004-ingress-as-substrate-primitive.md) for the architectural justification.

---

## Load-bearing invariants

### Substrate-not-MVP

Every milestone ships the complete primitive set the application space needs, not a phase-1 slice biased toward the named near-term projects. M-Ingress is the canonical case: single-shot inject + request/response call + bulk NDJSON all shipped together in one milestone arc (Phases 1+2+3 are implementation sequencing, not separately-deployable scope cuts), because a phase-1-only ship would have left half the application space speculative until phase 2 lands. The rule prevents the slow accumulation of half-built substrate features that each impose a migration tax on future milestones. See [`decisions/0004-ingress-as-substrate-primitive.md`](decisions/0004-ingress-as-substrate-primitive.md) for the worked example.

### Backend-only

The runtime ships protocols, not UIs. The observer surface exposes Prometheus, OTel, JSON-over-WebSocket, and SSE — every one of those is a protocol an external consumer (Grafana, Jaeger, a custom React dashboard) hooks into. The `grx observer` TUI is a thin ratatui client over the same WebSocket protocol any other consumer would use; it is not a privileged UI. There is no bundled browser frontend, no React renderer for `.grx` files into HTML, no built-in admin dashboard. The bifurcation into Backend + GraphReactDB (decision 0003) is the load-bearing reason this scope stays narrow. See [`decisions/0002-graphreact-is-backend-only.md`](decisions/0002-graphreact-is-backend-only.md).

### Default-deny external ingress

Pre-M-Ingress applications had no external HTTP path into actor inboxes. Post-M-Ingress, they continue to refuse every external message type because their `.grx` manifests have empty `accepts_external` / `accepts_external_call` lists — operators upgrade the runtime with zero attack-surface change. The component author opts in per message type, next to the handler code where it is reviewable. `accepts_external` is intentionally separate from `accepts` so internal node-to-node delivery can never be inadvertently mirrored as an HTTP-reachable surface. See [`decisions/0004-ingress-as-substrate-primitive.md`](decisions/0004-ingress-as-substrate-primitive.md).

### Hard wire-version floor

`MIN_SUPPORTED_VERSION == WIRE_VERSION` in the distribution layer: a v4 daemon refuses to handshake with a v3 daemon and vice versa. Rolling cluster upgrades that span a wire bump require running both versions side-by-side at the orchestrator layer, not at the runtime layer. This is a deliberate substrate-level choice — defer rolling-upgrade support to M-Prod where it can be designed alongside auth rotation and capability negotiation. The capability-flag pattern (`OBSERVER_OTEL_V1_CAPABILITY`, `INGRESS_CALL_V1_CAPABILITY`) handles graceful field stripping within a single wire version; cross-version mixing stays out of scope.

### Per-engine FFI is engine-native

Every engine bridge talks to its host interpreter through that interpreter's own C/native embedding API, surfaced through a thin Zig FFI shim. JSC uses the `JavaScriptCore.framework` C API on macOS; CPython uses the stable Python embedding ABI; future JVM would use JNI. There is no shared CDP-over-TCP layer, no generic protocol abstraction across engines — the `Engine` trait in `graphreact-engine-zig` is the only shared surface, and it exists at the Rust level, not the wire level. V8 is soft-demoted (see [`decisions/0001-v8-engine-question.md`](decisions/0001-v8-engine-question.md)) because the JSC + CPython axis already covers the JavaScript + Python surface area the application space needs without inheriting V8's vendoring + ABI-churn cost.

### Telemetry hot path is zero-cost when no sink attached

The scheduler's emit path is `if let Some(sink) = telemetry.load_full() { sink.record(event) }` — one `ArcSwap` load + one `Option::is_some` branch when no observer is attached, no allocation, no event construction. Sinks are installed via `install_telemetry_sink` after scheduler start; the hot path stays identical whether the operator is running `--runtime=null` for a CI smoke test or full Prometheus + OTel + WebSocket multiplexing in production. The `MultiplexSink` fan-out to per-sink workers happens off the scheduler critical path on a dedicated tokio task.

---

## Decision docs index

The architectural decisions that shape current scope. Each one is a single page; read them in order if you are new to the project.

| # | Title | Affects |
|---|---|---|
| 0001 | [Do we actually need V8?](decisions/0001-v8-engine-question.md) | Engine bridges — V8 soft-demoted in favor of JSC + CPython axis. |
| 0002 | [GraphReact is a backend runtime; no bundled frontend](decisions/0002-graphreact-is-backend-only.md) | Scope boundary — observer ships protocols, not UIs. |
| 0003 | [GraphReact bifurcates into Backend + GraphReactDB](decisions/0003-graphreact-bifurcates-into-backend-and-db.md) | Product split — keeps actor-runtime scope distinct from database-shape scope. |
| 0004 | [M-Ingress is its own milestone; correlation_id rides on Message](decisions/0004-ingress-as-substrate-primitive.md) | M-Ingress — substrate-not-MVP, default-deny manifest gating, `correlation_id` on `Message`. |

---

## Milestone status

Every milestone tag in the workspace glossary, the crate(s) it owns, and the narrative doc. Stable shipped milestones are the load-bearing surface; UNSHIPPED entries are living documents that track in-flight scope.

| Tag | Name | Status | Crate(s) | Narrative |
|---|---|---|---|---|
| M2 | `CommunicationGraph` data structure (foundational substrate) | shipped (pre-M14) | `graphreact-core` | [`../crates/graphreact-core/README.md`](../crates/graphreact-core/README.md) |
| M3 | NetworkX-parity graph algorithms (components, cycles, DAG, pathfinding, traversal, validation) | shipped (pre-M14) | `graphreact-algorithms` | [`backend/m6.6-engine-parity.md`](backend/m6.6-engine-parity.md) |
| M4 | Reactive runtime / scheduler | shipped (pre-M14) | `graphreact-core` | [`backend/README.md`](backend/README.md) |
| M5 | OTP-inspired supervision | shipped (pre-M14) | `graphreact-core` | [`../crates/graphreact-core/README.md`](../crates/graphreact-core/README.md) |
| M6.6 | TS feature parity audit, per-engine sign-off | UNSHIPPED (living, 2026-05-02) | `graphreact-napi`, `graphreact-engine-jsc`, `graphreact-engine-cpython` | [`backend/m6.6-engine-parity.md`](backend/m6.6-engine-parity.md) |
| M7 | Telemetry sink trait (`TelemetrySink`) — substrate hot path | shipped (pre-M14) | `graphreact-core` | [`../crates/graphreact-core/README.md`](../crates/graphreact-core/README.md) |
| M8 | Linear algebra kernel + partition (Leiden, modularity) | shipped (pre-M14) | `graphreact-linalg`, `graphreact-graspologic` | [`../crates/graphreact-linalg/README.md`](../crates/graphreact-linalg/README.md) |
| M9 | Spectral embeddings (ASE/LSE/OMNI/MASE/CASE + Procrustes + dimension selection) | shipped (pre-M14) | `graphreact-graspologic` | [`../crates/graphreact-graspologic/README.md`](../crates/graphreact-graspologic/README.md) |
| M10 | Cluster / inference / match / nominate analytics (GMM, latent-position, Hungarian+FAQ, spectral VN) | shipped (pre-M14) | `graphreact-graspologic` | [`../crates/graphreact-graspologic/README.md`](../crates/graphreact-graspologic/README.md) |
| M11 | `.grx` parser + IR builder | shipped (pre-M14) | `graphreact-grx-parser` | [`backend/README.md`](backend/README.md) |
| M11.5 | PCBPnR Phase 5-D shape substrate for the parser | SHIPPED 2026-05-26 | `graphreact-grx-parser` | [`backend/m11.5-pcb-shape-substrate.md`](backend/m11.5-pcb-shape-substrate.md) |
| M12 | Engine-substrate trio — engine-agnostic Rust trait + null + echo bridges | shipped (pre-M14) | `graphreact-engine-zig`, `graphreact-engine-null`, `graphreact-engine-echo` | [`backend/m6.6-engine-parity.md`](backend/m6.6-engine-parity.md) |
| M12.1 | Standalone `grx` CLI skeleton (parse / check / run / fmt / engine selection) | shipped (pre-M14) | `graphreact-cli` | [`../crates/graphreact-cli/README.md`](../crates/graphreact-cli/README.md) |
| M12.3a | JSC engine bridge (macOS system framework) | shipped | `graphreact-engine-jsc` | [`backend/m6.6-engine-parity.md`](backend/m6.6-engine-parity.md) |
| M12.3b | WebKit vendoring for JSC on Linux/Windows | UNSHIPPED (living, 2026-05-02) | `graphreact-engine-jsc` | [`backend/m12.3b-webkit-vendoring.md`](backend/m12.3b-webkit-vendoring.md) |
| M13 | CPython engine bridge | shipped 2026-05-06 | `graphreact-engine-cpython` | [`backend/m13-phases.md`](backend/m13-phases.md) |
| M14 | Distribution layer (first BEAM-class property) | shipped 2026-05-05 | `graphreact-dist` | [`backend/m14-distribution.md`](backend/m14-distribution.md) |
| M15 | Hot reload (second BEAM-class property) | shipped 2026-05-07 | `graphreact-grx-parser`, `graphreact-hotreload` | [`backend/m15-hot-reload.md`](backend/m15-hot-reload.md) |
| M16-Backend | Observer + telemetry (third BEAM-class property) | SHIPPED 2026-05-08 | `graphreact-observer`, `graphreact-cli` | [`backend/m16-backend-observer.md`](backend/m16-backend-observer.md) |
| M-Ingress | External HTTP ingress substrate (M16.5-Backend) | SHIPPED 2026-05-09 | `graphreact-observer`, `graphreact-cli` | [`backend/m16.5-ingress.md`](backend/m16.5-ingress.md) |
| M18 | WebAssembly server-WASM bindings | UNSHIPPED (stub) | `graphreact-wasm` | [`backend/README.md`](backend/README.md) |

---

## Where to go next

- **Canonical React client**: [`../../graphreact-visualizer/docs/onboarding.md`](../../graphreact-visualizer/docs/onboarding.md) and [`../../graphreact-visualizer/docs/architecture.md`](../../graphreact-visualizer/docs/architecture.md) — the downstream library that consumes the M16-Backend management API + SSE/WS event stream + M-Ingress endpoints.
- **Per-crate detail**: each crate ships its own README at [`../crates/<name>/README.md`](../crates/) — see the tier tables above for direct links. Start with `graphreact-core` if you want to read the scheduler; start with `graphreact-cli` if you want to read the binary entry point.
- **Onboarding**: [`onboarding.md`](onboarding.md) walks a fresh contributor through cloning, building, running `grx run --runtime=null` against a sample `.grx` file, then progressively wiring in JSC / CPython / distribution / observer.
- **Plan file**: the active workspace plan lives at [`~/.claude/plans/`](../../.claude/plans/) — search for the most recent `phoenix` / `waffle` / milestone-tagged plan.
- **Deep dives by milestone tag**: every shipped milestone has a narrative doc under [`backend/`](backend/) — see the milestone-status table above. The decision docs under [`decisions/`](decisions/) explain *why* the shape is what it is; the milestone narratives explain *what* shipped and *what's deferred*.
- **Wire-format specs**: distribution wire format at [`../crates/graphreact-dist/docs/wire-protocol.md`](../crates/graphreact-dist/docs/wire-protocol.md); M-Ingress wire format at [`backend/m16.5-ingress-protocol.md`](backend/m16.5-ingress-protocol.md); Prometheus metric catalog at [`backend/m16-prometheus.md`](backend/m16-prometheus.md); management API endpoint catalog at [`backend/m16-management-api.md`](backend/m16-management-api.md).
