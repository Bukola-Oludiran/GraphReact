# graphreact-observer

*Observer + telemetry substrate — JSON logs, Prometheus exporter, OTel tracing, axum-based management API (incl. M-Ingress endpoints), SSE/WS event stream, ratatui TUI client.*

## Milestone

Ships across **M16-Backend** (Phases 1–10) and the **M-Ingress** wire-protocol layer. The phase narratives are split across several docs in [`docs/backend/`](../../docs/backend/):

- [m16-backend-observer.md](../../docs/backend/m16-backend-observer.md) — overall phase log (JSON logs → Prom → tracing → mgmt API → WS → TUI → OTel)
- [m16-management-api.md](../../docs/backend/m16-management-api.md) — axum surface, auth, bind defaults
- [m16-prometheus.md](../../docs/backend/m16-prometheus.md) — exporter + scrape format
- [m16-otel.md](../../docs/backend/m16-otel.md) — distributed tracing + wire context
- [m16-tui.md](../../docs/backend/m16-tui.md) — ratatui client
- [m16.5-ingress.md](../../docs/backend/m16.5-ingress.md) + [m16.5-ingress-protocol.md](../../docs/backend/m16.5-ingress-protocol.md) — M-Ingress endpoints (`POST /v1/nodes/:id/call`, message injection, snapshots) that PCBPnR and other agentic clients drive

## Role in the runtime

Sits in the **observer tier** of the workspace — peer to `graphreact-dist` and downstream of `graphreact-core`'s `TelemetrySink` hook. The scheduler holds a single `Arc<dyn TelemetrySink>`; this crate provides the rich consumer impls (Prometheus counters, JSON-log writer, fan-out to WS subscribers, OTel span emission) behind a `MultiplexSink` so installation stays one-shot. It also hosts the **management API** — the axum HTTP/WS surface the CLI, TUI, and external agents (PCBPnR's `pcb-cli daemon`, the upcoming `pi-coding-agent`) hit for snapshots, metrics, event streams, and M-Ingress `correlation_id`-style tool calls. Hot-path discipline is load-bearing: every `TelemetrySink` method runs while the scheduler holds internal `RefCell` borrows, so sinks here MUST NOT call back into the scheduler, MUST NOT block, and keep allocations bounded.

## Key types

- **`MultiplexSink`** ([`src/multiplex.rs`](src/multiplex.rs)) — fans `TelemetryEvent`s to N consumers behind one `Arc<dyn TelemetrySink>`. The integration point for every Phase-2+ sink stack.
- **`JsonLogTelemetry`** ([`src/json_logs.rs`](src/json_logs.rs)) — Phase-1 single-line JSON record per event to stderr or `GRX_OBSERVER_LOG` path.
- **`PrometheusSink`** ([`src/prometheus_sink.rs`](src/prometheus_sink.rs)) — atomic-counter exporter; renders the `/metrics` scrape body via [`prometheus.rs`](src/prometheus.rs).
- **`spawn_management_api` / `ApiState`** ([`src/management_api.rs`](src/management_api.rs)) — axum server hosting snapshots, metrics, SSE/WS event stream, and M-Ingress `POST /v1/nodes/:id/call` with `pending_calls` correlation.
- **`BindConfig` / `BearerToken`** ([`src/bind.rs`](src/bind.rs), [`src/auth.rs`](src/auth.rs)) — loopback-default bind policy + bearer-token auth gate for non-loopback binds.
- **`TelemetryEvent`** ([`src/events.rs`](src/events.rs)) — the event taxonomy the scheduler emits and every sink consumes.

## Composes with

| Crate | Role |
|---|---|
| [`graphreact-core`](../graphreact-core/) | Owns the `TelemetrySink` trait + scheduler hook this crate's sinks plug into. |
| [`graphreact-dist`](../graphreact-dist/) | OTel feature only — `WireTracingHook` trait lives in dist; `OtelSink` here impls it for cross-node span propagation. |

**Depended on by:**

- [`graphreact-cli`](../graphreact-cli/) — `grx daemon` + `grx observer tui` wire `MultiplexSink`, the management API, and the ratatui client through this crate. External daemons (PCBPnR's `pcb-cli daemon`) use the same M-Ingress endpoints.

## Running the tests

```sh
cargo test -p graphreact-observer --release
```

13 integration-test files under [`tests/`](tests/) cover auth, bind defaults, bulk + call + inject M-Ingress flows, JSON logs, management API, observer WebSocket, OTel sink, per-node detail snapshots, Prometheus scrape, telemetry zero-cost guarantees, and the tracing layer.

## See also

- [docs/backend/m16-backend-observer.md](../../docs/backend/m16-backend-observer.md) — parent milestone narrative
- [docs/backend/m16.5-ingress.md](../../docs/backend/m16.5-ingress.md) — M-Ingress design (the surface PCBPnR + agentic clients drive)
- [docs/decisions/0004-ingress-as-substrate-primitive.md](../../docs/decisions/0004-ingress-as-substrate-primitive.md) — why M-Ingress is a substrate primitive, not a per-product overlay
- [docs/onboarding.md](../../docs/onboarding.md) — workspace tour
- [docs/architecture.md](../../docs/architecture.md) — tier map (substrate / engine / distribution / observer / cli / foreign)
