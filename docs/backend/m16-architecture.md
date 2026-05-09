# M16-Backend — Architecture overview

**Status:** SHIPPED 2026-05-08 (P11 closeout doc).

One-page architecture for the observability substrate that turns a running `grx cluster daemon` into something an ops team can actually run. Server-native protocol surfaces only — NO bundled web UI, NO Chrome-DevTools-as-runtime-observer, NO supervisor-tree GUI ([decision 0002](../decisions/0002-graphreact-is-backend-only.md)).

## Layers

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Operator / Companion ecosystem               │
│                                                                      │
│   Prometheus / Grafana  │  Tempo / Jaeger  │  Loki / Elastic         │
│   Safari Web Inspector  │  VS Code Python  │  Custom React UI        │
│                         │  extension       │  (e.g. companion lib)   │
└─────────────────────────────────────────────────────────────────────┘
                  ▲ standard protocols              ▲ companion API
                  │                                 │
   ┌──────────────┴──────────────┐    ┌─────────────┴────────────────┐
   │  Stock observability        │    │  /v1/* management API +      │
   │  protocols (mostly stable)  │    │  WS / SSE event stream       │
   │  - Prometheus text format   │    │  (axum 0.7 + bearer auth)    │
   │  - OTLP/gRPC (OTel)         │    │                              │
   │  - JSON logs (Loki/Elastic) │    │                              │
   │  - debugpy / RemoteInspector│    │                              │
   └─────────────────────────────┘    └──────────────────────────────┘
                  ▲                                 ▲
                  │                                 │
   ┌──────────────┴─────────────────────────────────┴──────────────┐
   │                   graphreact-observer crate                    │
   │                                                                │
   │   PrometheusSink   JsonLogTelemetry   RingBufferTelemetry      │
   │   WebSocketSink    OtelSink (--features otel)                  │
   │           │                                                     │
   │           └──── MultiplexSink (fan-out) ──────────              │
   │                          │                                     │
   │                          ▼                                     │
   │            install_telemetry_sink (single slot)                │
   └────────────────────────────────────────────────────────────────┘
                              ▲
                              │ events fire from hot-path
                              │
   ┌──────────────────────────┴─────────────────────────────────────┐
   │                    graphreact-core (Scheduler)                  │
   │                                                                 │
   │   on_scheduler_tick_start/end   on_node_evaluate_start/end     │
   │   on_message_delivered          on_node_mounted/unmounted     │
   │   on_topology_event             on_supervisor_decision         │
   │   on_queue_depths                                               │
   │                                                                 │
   │   (12-event TelemetrySink trait)                               │
   └────────────────────────────────────────────────────────────────┘
                              ▲                       ▲
                              │ on_remote_message     │ on_hot_reload
                              │                       │
   ┌──────────────────────────┴───────┐  ┌────────────┴──────────────┐
   │   graphreact-dist                 │  │  graphreact-hotreload     │
   │   (M14 distribution)              │  │  (M15 hot reload)          │
   │                                    │  │                            │
   │   Cluster::install_telemetry_sink │  │  ReloadOrchestrator::      │
   │     (per-crate slot, NO scheduler │  │    install_telemetry_sink  │
   │     hop — option (a) per P1)      │  │                            │
   │                                    │  │                            │
   │   Cluster::install_wire_tracing_  │  │                            │
   │     hook (P10 substrate;          │  │                            │
   │     OtelSink installs both)       │  │                            │
   └────────────────────────────────────┘  └────────────────────────────┘
                              ▲
                              │ wire packets carry trace_context
                              │ (P10 + WIRE_VERSION = 3)
                              ▼
   ┌────────────────────────────────────────────────────────────────┐
   │                    Remote peers (M14 cluster)                   │
   └────────────────────────────────────────────────────────────────┘
```

## Surfaces shipped (P1-P11)

### Axis A — runtime telemetry (server-native; primary M16 deliverable)

GraphReact-runtime data about the running system: supervisor tree, message rates, queue depths, restart counters, scheduler tick latency, hot-reload outcomes.

| Surface | Phase | Module | Plug-into |
|---|---|---|---|
| **Prometheus** `/v1/metrics` | P2 | `graphreact_observer::PrometheusSink` + `prometheus.rs` text encoder | Stock Prometheus + Grafana ([m16-prometheus.md](m16-prometheus.md), [m16-grafana-dashboard.json](m16-grafana-dashboard.json)) |
| **Structured JSON logs** | P3 | `graphreact_observer::TracingJsonLayer` + `JsonLogTelemetry` | Loki / Elastic / fluent-bit |
| **Management API** `/v1/*` | P4 + P5 | `graphreact_observer::management_api` (axum 0.7) | Companion ecosystem ([m16-management-api.md](m16-management-api.md)) |
| **CLI introspection** `grx ps/tree/top/trace` | P5 | `graphreact_cli::observer_client` (hand-rolled HTTP/1.1) | Operators in shell scripts |
| **Terminal UI** `grx observer` | P6 | `graphreact_cli::observer_tui` (ratatui 0.28 + crossterm) | Operators over SSH ([m16-tui.md](m16-tui.md)) |
| **Live event stream** WS + SSE | P7 | `graphreact_observer::WebSocketSink` + axum upgrade handlers | TUI live-tail + companion subscribers |
| **OpenTelemetry traces** | P10 | `graphreact_observer::OtelSink` (`--features otel`) | Tempo / Jaeger / Honeycomb ([m16-otel.md](m16-otel.md)) |

### Axis B — per-body source debugging (engine-native; opt-in)

Each engine ships its native debug protocol consumed by its native IDE — never a browser. Captured as the durable rule [`feedback_axis_b_no_browser.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/feedback_axis_b_no_browser.md).

| Engine | Phase | Protocol | Native IDE | CLI flag |
|---|---|---|---|---|
| **CPython** | P8 | debugpy | VS Code Python extension / PyCharm | `--debug=cpython:5678` ([m16-debug-cpython.md](m16-debug-cpython.md)) |
| **JSC** | P9 | WebKit RemoteInspector (lockdownd) | Safari Web Inspector | `--debug=jsc:9229` ([m16-debug-jsc.md](m16-debug-jsc.md)) |
| **JVM** | M17 (future) | JDWP | IntelliJ / Eclipse | `--debug=jvm:5005` (planned) |

CDP-over-TCP attach (VS Code's JS debugger / Chrome DevTools / WebStorm) defers with V8 per [decision 0001](../decisions/0001-v8-engine-question.md); the vendored-WebKit `RemoteInspectorServer` + CDP shim is M-Prod scope.

## Default network exposure

Every listening surface binds `127.0.0.1` only. Remote requires explicit `--observer-bind=0.0.0.0:port` + `--observer-token-{env|file}` + `--observer-cert <path>` + `--observer-key <path>` flags; refused at config-load time without all three. Audit log of observer connections via `tracing::info!(target: "graphreact::observer::audit", peer=%addr, "observer connect")`.

Per-body debug flags (`--debug=cpython:<port>` / `--debug=jsc:<port>`) bind loopback only by design (debug traffic carries breakpoint state + variable inspection that's effectively process-internal). Cross-host attach uses SSH tunnel + loopback. M-Prod can add remote-bind variants with auth/TLS.

## Hot-path discipline (substrate constant)

Every TelemetrySink callback runs from inside the scheduler hot path. Discipline:

- Sink methods MUST NOT call back into the scheduler.
- Sink methods MUST NOT block (no synchronous `Mutex` waits, no I/O that could spill milliseconds).
- Defaults are `#[inline] fn ...(&self, _ev: &Event) {}` so a `NoopSink` compiles to a single `Option::is_some` branch on the scheduler hot path.

Bench gate `scheduler_overhead` (criterion):
- `case_a` — trait NOT attached (baseline).
- `case_b` — NoopSink attached. Plan-locked **≤1% gate** (case_b/case_a).
- `case_c` — JsonLogTelemetry. Plan-locked **≤5% gate** (case_c/case_b).
- `case_d` — PrometheusSink. Within case_c gate.
- `case_e` — PrometheusSink + tracing-subscriber. Within case_c gate.

P1-P10 trend on macOS dev machine: `case_b/case_a` oscillates between -0.57% and +2.32%. Clean-machine verification is M-Prod work.

## Cross-thread bridge (axum + !Send Scheduler)

axum runs on multi-threaded tokio; `Scheduler` is `!Send`. Handlers post `SnapshotRequest` enums onto a 256-slot bounded `mpsc::Sender`; the scheduler thread drains via `drain_snapshot_requests(rx, |req| ...)` after each `process_batch` round (called from `commands.rs::drive_handle_until`). Each request carries an embedded `oneshot::Sender<Response>`. Full channel returns `503 Service Unavailable` (substrate-not-MVP backpressure visible from day one).

Worst-case latency: one `process_batch` round (~50ms typical at idle, sub-ms under load).

## Multiplex fan-out

Scheduler holds a single `Option<Arc<dyn TelemetrySink>>` slot via `install_telemetry_sink`. Multiple consumers fan out through `MultiplexSink::builder().push(prom).push(ring).push(ws).push(otel).build()` — keeps the scheduler hot path single-branch (`Option::is_some`) regardless of consumer count.

`start_daemon` constructs the production multiplex chain: PrometheusSink + RingBufferTelemetry (always) + WebSocketSink (when `--observer-bind` set) + OtelSink (when `--observer-otel-endpoint` set under `--features otel`).

## Per-crate sink slots (option (a))

`graphreact-dist` and `graphreact-hotreload` each hold their own `TelemetrySinkSlot` rather than routing through the scheduler. Reasons:

1. Avoids cross-thread channel hop on every remote message (M14's 295k+ msg/sec floor would feel it).
2. Keeps the per-crate event-firing surface self-contained.
3. Matches the existing M15 Phase 10 `RemoteReloadSink` pattern.

`Cluster::install_telemetry_sink(sink)` + `ReloadOrchestrator::install_telemetry_sink(sink)` + `Scheduler::install_telemetry_sink(sink)` all take the SAME `Arc<dyn TelemetrySink>` (the production daemon's `MultiplexSink`) — single "register one consumer" experience.

## Cross-node trace propagation (P10)

The M14 wire format's `Message` struct gained an optional `trace_context: Option<[u8; 24]>` field at `WIRE_VERSION = 3` ([m16-otel.md](m16-otel.md)). When OtelSink is configured:

1. Outbound: `Cluster::install_wire_tracing_hook` slot (mirrors `install_telemetry_sink`) stores OtelSink as `dyn WireTracingHook`. `route_outbound` queries the slot before serializing wire `Message`; the OTel `Context::current().span()` SpanContext is encoded as 24 bytes (TraceID 16 + SpanID 8) and embedded in the field.
2. Inbound: `wire_to_local` propagates the field into local `Message.trace_context`. The scheduler's `deliver` path forwards into the `MessageDelivered` event. OtelSink decodes the bytes back into a `SpanContext` and attaches as a `Link` on the receiver-side `message.deliver` span.

Cross-cluster traces render as connected DAGs in Tempo/Jaeger/Honeycomb. Both peers must advertise `OBSERVER_OTEL_V1_CAPABILITY` in their handshake for full participation.

## What's NOT in M16-Backend

| Drop | Reason | Where it lands |
|---|---|---|
| Web UI / browser frontend | [Decision 0002](../decisions/0002-graphreact-is-backend-only.md) — backend-only. | Companion ecosystem (third-party React SDK consumes management API). |
| JVM JDWP debug protocol | M17 hasn't shipped JVM engine yet. | M17. |
| CDP-over-TCP attach for JSC | Requires vendored WebKit `RemoteInspectorServer` + CDP shim (~600+ LOC of WebKit-internal C++ glue, Bun-style). | M-Prod (alongside vendored WebKit work that lifts JSC's macOS-only ceiling). |
| `/readyz` Kubernetes-shaped readiness endpoint | M-Prod #8 deliverable. | M-Prod. |
| Backpressure policy on observer channels | M-Prod #5 audits ALL `mpsc::Sender`s uniformly. | M-Prod. |
| Rate-limiting on management API + Prometheus endpoint | M-Prod #6 cross-cuts all surfaces. | M-Prod. |
| TLS cert rotation procedures (SIGHUP-triggered reload) | M-Prod #7 cross-cuts all TLS surfaces. | M-Prod. |
| Continuous SLO commitments (published p99 numbers) | M-Prod #9. M16 ships histograms; commitment is M-Prod. | M-Prod. |
| Multi-token / RBAC / JWT claims | Single bearer token sufficient for M16 acceptance. | M-Prod. |
| Wire-version negotiation (v2 ↔ v3 rolling upgrade) | M-Prod owns SemVer commitments + rolling-upgrade story. | M-Prod. |

## Cross-references

- [`m16-backend-observer.md`](m16-backend-observer.md) — milestone-wide closeout narrative tying P1-P11 together.
- [`m16-prometheus.md`](m16-prometheus.md) — metric catalog.
- [`m16-management-api.md`](m16-management-api.md) — endpoint catalog.
- [`m16-tui.md`](m16-tui.md) — TUI keybinds + recipes.
- [`m16-otel.md`](m16-otel.md) — OTel + cross-node tracing.
- [`m16-debug-cpython.md`](m16-debug-cpython.md) — Axis B Python via debugpy + VS Code/PyCharm.
- [`m16-debug-jsc.md`](m16-debug-jsc.md) — Axis B JSC via Safari Web Inspector + lockdownd.
- [`m16-p4-framework-decision.md`](m16-p4-framework-decision.md) — why axum 0.7 (P4 evidence-based decision).
- [`m16-grafana-dashboard.json`](m16-grafana-dashboard.json) — provisioning-format Grafana dashboard.
- [decision 0002](../decisions/0002-graphreact-is-backend-only.md) — backend-only architectural commitment.
- [decision 0003](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md) — Backend / DB product split (M16-Backend is this product's first BEAM-class operability deliverable).
