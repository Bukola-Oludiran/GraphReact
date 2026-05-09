# M16-Backend — Observer + telemetry (closeout narrative)

**Status:** SHIPPED 2026-05-08. Third BEAM-class property after M14 distribution + M15 hot reload. 11 phases delivered across 2 days (P1-P6 on 2026-05-07; P7-P11 on 2026-05-08). Substrate-not-MVP discipline maintained throughout — every consumer set + every wire surface ships complete from day one.

## What this milestone delivered

The observability substrate that turns a running `grx cluster daemon` into something an ops team can actually run. **Server-native protocol surfaces** that an ops team plugs into existing tooling (Prometheus + Grafana + Tempo/Jaeger + Loki/Elastic) plus per-body debug protocols (debugpy + Safari Web Inspector) the body author attaches their existing IDE to. **No bundled web UI; no Chrome-DevTools-as-runtime-observer; no supervisor-tree GUI** per [decision 0002](../decisions/0002-graphreact-is-backend-only.md).

When a deployment runs `grx cluster daemon --observer-bind <addr> --observer-otel-endpoint <url>`, the operator gains:

| Capability | Surface | Standard tooling |
|---|---|---|
| Live runtime telemetry over SSH | `grx observer` ratatui TUI | Built into `grx` CLI |
| Stock metrics scrape | `GET /v1/metrics` | Prometheus + Grafana |
| Distributed tracing across cluster | OTLP/gRPC with cross-node `Link` attributes | Tempo / Jaeger / Honeycomb |
| Structured logs | Single-line JSON to stderr | Loki / Elastic / fluent-bit |
| Programmatic introspection | `/v1/nodes/...` + `/v1/supervisor/tree` | Companion ecosystem (e.g. third-party React UI) |
| Live event stream | `/v1/observer/events` (WS + SSE) | TUI live-tail + custom subscribers |
| Per-body source debugging (Python) | `--debug=cpython:<port>` | VS Code Python extension / PyCharm |
| Per-body source debugging (JSC) | `--debug=jsc:<port>` | Safari Web Inspector via macOS lockdownd |
| Crash-detection | `process_start_id` on `/v1/identity` | Ops scripts polling for restart |
| Backpressure visibility | `503` on full mgmt-API channel + `Resync` close-frame for slow WS subscribers + `subscribers_kicked_total` counter | Existing alerting pipelines |

It also gates the **Backend product M-Prod track** opening per [decision 0003](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md).

## Phase-by-phase delivery

| Phase | Slice | LOC | Date | Per-phase memory |
|---|---|---|---|---|
| **P1** | TelemetrySink trait + observer crate skeleton + JsonLogTelemetry first consumer + `scheduler_overhead` bench harness | ~450 | 2026-05-07 | [`project_graphreact_m16_backend_phase1.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase1.md) |
| **P2** | Prometheus exporter + bind-defaults + auth substrate (BindConfig + BearerToken + ServerTlsConfig) | ~400 | 2026-05-07 | [`project_graphreact_m16_backend_phase2.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase2.md) |
| **P3** | `tracing-subscriber` → unified JSON logs (Loki/Elastic shape) | ~280 | 2026-05-07 | [`project_graphreact_m16_backend_phase3.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase3.md) |
| **P4** | Management API foundation (axum 0.7 picked on evidence) + GET endpoints + cross-thread bridge | ~450 | 2026-05-07 | [`project_graphreact_m16_backend_phase4.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase4.md) |
| **P5** | Per-node ring buffer + `grx ps/tree/top/trace` introspection commands | ~350 | 2026-05-07 | [`project_graphreact_m16_backend_phase5.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase5.md) |
| **P6** | `grx observer` ratatui terminal UI + first cross-host TLS scrape (rustls-native-certs) | ~600 | 2026-05-07 | [`project_graphreact_m16_backend_phase6.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase6.md) |
| **P7** | WebSocket + SSE live event stream | ~280 | 2026-05-08 | [`project_graphreact_m16_backend_phase7.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase7.md) |
| **P8** | Axis B Part 1: CPython debugpy via `invoke_global` | ~200 | 2026-05-08 | [`project_graphreact_m16_backend_phase8.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase8.md) |
| **P9** | Axis B Part 2: JSC RemoteInspector via Zig bridge (Safari Web Inspector via lockdownd) | ~250 | 2026-05-08 | [`project_graphreact_m16_backend_phase9.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase9.md) |
| **P10** | OpenTelemetry traces (feature-gated) + cross-node span linkage via WIRE_VERSION 2→3 bump | ~1100 | 2026-05-08 | [`project_graphreact_m16_backend_phase10.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase10.md) |
| **P11** | E2E integration + closeout artifacts (Grafana dashboard + 6 docs + plan-file SHIPPED) | ~150 LOC of code + ~2.5K LOC of markdown | 2026-05-08 | [`project_graphreact_m16_backend_phase11.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase11.md) |

**Total milestone LOC**: ~4,500 across observer + dist + core + cli (excluding M16-Backend's contributions to the per-phase test counts and excluding the ~5K LOC of markdown documentation).

## Test counts (final)

| Suite | Count | Notes |
|---|---|---|
| Workspace lib + integration tests (default features) | **996 release tests pass, 0 failures** | Full `cargo test --release --workspace` |
| OtelSink integration tests under `--features otel-test` | **8/8 pass** | InMemorySpanExporter-backed; includes the cross-node Link acceptance gate |
| Workspace clippy `--all-targets -- -D warnings` | **Clean (0 warnings, 0 errors)** | Both default + `--features otel` |
| Bench `scheduler_overhead/case_b/case_a` | **+1.14% (P10) / +0.25% (P9) / etc.** | Within macOS dev-machine noise band consistent with P5-P10 trend |

## Key architectural decisions locked in across the milestone

1. **M16-Backend only, M16-DB stays sketched.** DB-side activates when M19+M20+M21+M24 mature ([decision 0003](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md)).
2. **Server-native protocol surfaces only.** No bundled web UI / GUI / browser-rendered surface. Frontend tier is the user's job (companion ecosystem). [Decision 0002 addendum 2026-05-03](../decisions/0002-graphreact-is-backend-only.md).
3. **Telemetry trait location**: `graphreact-core::telemetry` (so `Scheduler` can call without depending on `graphreact-observer`); rich impls live in `graphreact-observer`. Mirrors how `RemoteSink` lives in core but `RemoteSupervision` lives in dist.
4. **Sized wrapper for arc-swap**: `TelemetrySinkRef(Arc<dyn TelemetrySink>)` + `Deref`. Required because `arc_swap::ArcSwapOption<T>` needs `T: Sized`.
5. **Option (a) per-crate sink slots, NO scheduler hop**: `on_remote_message` fires from `graphreact-dist`'s slot; `on_hot_reload` fires from `ReloadOrchestrator`'s slot. Avoids cross-thread channel hop on every remote message (M14's 295k+ msg/sec floor would feel it).
6. **All hot-path callsites wired in P1**: substrate-not-MVP at the consumer level. Avoids "P2 adds the X event" asymmetry.
7. **MultiplexSink for multi-consumer fan-out**: scheduler always sees ONE sink; `MultiplexSink::builder().push(prom).push(ring).push(ws).push(otel).build()` keeps the hot path single-branch.
8. **Per-tick `QueueDepthsSnapshot` sample** fired once per cascade round, NOT per-message: ~1 sample/ms cadence under load without dominating the hot path.
9. **Bench gates as plan-authored (1% / 5%)** — required adding `arc-swap` + `itoa` deps. Both well-trodden. Met after P1's A+B1 optimisation; held across P2-P10.
10. **HTTP framework decided in P4 on evidence, not P1**: P2 ships hyper 1.x low-level for the single `/metrics` route; P4 picks axum on evidence (multi-route + auth-state-sharing requirements visible by then). [`m16-p4-framework-decision.md`](m16-p4-framework-decision.md).
11. **Default `127.0.0.1` bind on every listening surface**: remote requires explicit `--observer-bind=0.0.0.0:port` + `--observer-token=<hex>` + `--observer-cert=<path>`. Refuses to bind remotely without all three.
12. **Audit-log convention**: `tracing::info!(target: "graphreact::observer::audit", peer=%addr, "...")` mirrors M15 Phase 10's `target: "graphreact_dist::reload"` pattern.
13. **TLS in `observer_client.rs` lands in P6** (not deferred to P7). Same TLS path serves all 5 introspection commands.
14. **`--insecure-no-tls` is on `grx observer` only**. Never on the daemon path. **MUST NOT appear in production deploys.**
15. **Symmetric Axis B framing**: engine-native debug protocol consumed by the engine-native IDE — Python → debugpy + VS Code/PyCharm; JSC → RemoteInspector + Safari; JVM → JDWP + IntelliJ. **No browsers in the protocol path.** Captured as durable rule [`feedback_axis_b_no_browser.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/feedback_axis_b_no_browser.md). Mid-implementation correction caught Chrome-DevTools framing inherited from V8/Node and fixed it ([decision 0001](../decisions/0001-v8-engine-question.md) — V8 soft-demoted; CDP-shaped clients defer with V8).
16. **macOS-only JSC + CPython** (current engine ceiling). Linux + Windows return errors at install time. M12.3b-phase4 will lift via vendored WebKit; M13-followon will lift CPython via vendored libpython.
17. **WIRE_VERSION 2→3 bump for cross-node OTel** (P10). Hard floor: v3 peers cannot interop with v2. M-Prod owns rolling-version negotiation. Adds `Option<[u8; 24]>` (TraceID 16 + SpanID 8) `trace_context` field to wire `Message`. Bytewidth reframed from plan-locked `[u8; 16]` (TraceID-only) so OTel `Link` attributes work properly.
18. **OTel deps feature-gated** behind `otel` Cargo feature on `graphreact-observer` + `graphreact-cli`. Default builds don't pull the ~30 transitive crate dep tree.
19. **`/v1/` versioning** from day one for companion ecosystem stability.
20. **PII safe-by-default**: payload deliberately omitted from `MessageSnapshot` (P5) + from OTel `message.deliver` spans (P10). Operators wanting payload visibility use the P3 JSON-log stream which IS payload-aware.

## Substrate-not-MVP wins

The user's "build the unified substrate, not the MVP" rule shaped every phase. Specific applications:

- **P1 wired all 12 events at once**, not "P2 adds the X event later." Avoided the asymmetry that would have left some consumers blind.
- **P2 shipped the COMPLETE primitive set from day one**: PrometheusSink + bind-defaults + auth substrate + TLS + audit log. No "P2-α metrics, P2-β auth" split.
- **P9 shipped Safari attach + future-compat port plumbing in one phase** (~250 LOC after the user's mid-implementation correction caught the Chrome-framing wrong-shape).
- **P10 shipped local OtelSink + full cross-node propagation in one phase** (~1100 LOC) — including the WIRE_VERSION bump that has real future cost. Skipping the bump would have created an asymmetry where OTel-aware applications could see local spans but not cross-cluster ones.
- **P11 shipped operator-facing artifacts** (Grafana dashboard + endpoint catalog + metric catalog + TUI keybinds + architecture overview + per-engine debug docs + closeout narrative) in one phase. No "P11-α dashboard, P11-β docs" split.

## Reframings noted across the milestone

| Phase | Plan-locked | Reframed | Why |
|---|---|---|---|
| P2 | `grx_supervisor_restarts_total{policy}` | `{action}` (Proceeded/Deferred/Escalated) | Underlying `SupervisorDecisionMade` event carries action, not policy; supervisor doesn't fire per-policy telemetry today |
| P2 | `grx_memo_recomputes_total` / `grx_memo_skips_total` | OMITTED | No `on_memo_*` event in the 12-event TelemetrySink; "no new event types in P2" was the constraint |
| P2 | `grx_messages_dropped_total{reason}` (dedicated event) | Derived from `MessageDelivered{accepted=false}` + `SchedulerTickEnd{terminated_by_depth_cap=true}` | No dedicated event; documented in HELP |
| P2 | (no `direction` label on `grx_remote_messages_routed_total`) | Added `direction` label | RemoteMessage event carried it; emitting cost nothing |
| P3 | (one unified output schema) | Two schemas: `JsonLogTelemetry`'s `{event, payload}` + `TracingJsonLayer`'s `{ts, level, target, name, fields, span}` | "Unified" means single output stream, not single record schema; both can co-exist on the same writer; operators distinguish by presence of `event` vs `ts` |
| P9 | "Chrome DevTools attaches to `--debug=jsc:9229`" | "Safari Web Inspector attaches via lockdownd; CDP-over-TCP defers with V8 (decision 0001)" | Mid-implementation user correction caught Chrome-DevTools wrong-shape framing; symmetric Axis B design is engine-native protocol + native IDE; Bun-style CDP-relay is M-Prod scope |
| P10 | `Option<[u8; 16]>` (TraceID-only) | `Option<[u8; 24]>` (TraceID + SpanID) | OTel `Link` attribute requires full SpanContext for proper span linkage; 16 bytes only enables trace-grouping |

## What did NOT ship in M16-Backend (deliberate)

| Drop | Reason | Where it lands |
|---|---|---|
| Web UI / browser frontend | [Decision 0002](../decisions/0002-graphreact-is-backend-only.md) — backend-only. | Companion ecosystem (third-party React SDK consumes management API). |
| JVM JDWP debug protocol | M17 hasn't shipped JVM engine yet. | M17. |
| CDP-over-TCP attach for JSC | Requires vendored WebKit `RemoteInspectorServer` + CDP shim (~600+ LOC of WebKit-internal C++ glue, Bun-style). | M-Prod (alongside vendored WebKit work that lifts JSC's macOS-only ceiling). |
| `/readyz` Kubernetes-shaped readiness endpoint | M-Prod #8 deliverable. | M-Prod. |
| Backpressure policy on observer channels | M-Prod #5 audits ALL `mpsc::Sender`s uniformly. | M-Prod. |
| Rate-limiting on management API + Prometheus endpoint | M-Prod #6 cross-cuts all surfaces. | M-Prod. |
| TLS cert rotation (SIGHUP-triggered reload) | M-Prod #7 cross-cuts all TLS surfaces. | M-Prod. |
| Continuous SLO commitments (published p99 numbers) | M-Prod #9. M16 ships histograms; commitment is M-Prod. | M-Prod. |
| Multi-token / RBAC / JWT claims | Single bearer token sufficient for M16 acceptance. | M-Prod. |
| Wire-version negotiation (v2 ↔ v3 rolling upgrade) | M-Prod owns SemVer commitments. | M-Prod. |
| Continuous fuzzing of management-API JSON schemas | M-Prod #1. M16 ships valid-input tests; corpus + 24h CI run is M-Prod. | M-Prod. |
| Stock-tooling integration tests in CI (Loki / promtool / OTel collector docker fixtures) | Heavy CI commits requiring docker fixtures. P11.1 added the wire-shape validation tier (prometheus-parse round-trip on /v1/metrics + JSON-log shape validation on stderr) that catches the encoder regressions docker fixtures would also catch. | M-Prod CI infrastructure track. |
| `grx observer` pass-2 polish (column widths, color highlighting, fuzzy-finder) | P6 2-iteration budget bounded the initial ship to data-shows-correctly + reconnect-on-restart. | TUI ergonomics; ship as user feedback comes in. |
| `case_g_batch` bench variant (BatchSpanProcessor + tokio runtime context) | P11.1 shipped `case_g_otel_sink` with SimpleSpanProcessor (measured at +39% over case_a — over-estimates production cost since SimpleSpanProcessor is synchronous; production BatchSpanProcessor with async tokio drain would be lower). The batch-processor variant requires bench infrastructure that hosts a tokio runtime context. | M-Prod observability hardening. |

## Operator runbook quick-reference

### Standard production deployment

```sh
grx cluster daemon \
  --node-id alice@10.0.0.5:5050 \
  --listen 0.0.0.0:5050 \
  --cookie-file /etc/grx/cookie \
  --cert /etc/grx/cluster.pem --key /etc/grx/cluster.key --ca /etc/grx/ca.pem \
  --peers bob@10.0.0.6:5050,carol@10.0.0.7:5050 \
  \
  --observer-bind 0.0.0.0:9090 \
  --observer-token-file /etc/grx/observer-token \
  --observer-cert /etc/grx/observer.pem --observer-key /etc/grx/observer.key \
  --observer-json-logs \
  --observer-otel-endpoint https://collector.observability.example.com:4317 \
  --observer-otel-service-name grx-alice \
  \
  --runtime cpython \
  --path /etc/grx/app.grx
```

### Stock observability stack

- **Prometheus** scrapes `https://10.0.0.5:9090/v1/metrics` per the recipe in [`m16-prometheus.md`](m16-prometheus.md).
- **Grafana** imports [`m16-grafana-dashboard.json`](m16-grafana-dashboard.json) provisioning JSON.
- **Tempo** receives OTLP/gRPC traces on `4317`; dashboards via Grafana Tempo data source.
- **Loki** ingests the JSON log stream piped from `--observer-json-logs` (via fluent-bit / promtail).
- **Operators** SSH in and run `grx observer --target https://10.0.0.5:9090 --token-env GRX_TOKEN` for live introspection.

### Per-body debugging (dev)

- Python: `grx run --runtime=cpython --debug=cpython:5678 fixture.grx` + VS Code Python extension `Remote Attach` to `localhost:5678` ([`m16-debug-cpython.md`](m16-debug-cpython.md)).
- JSC: `grx run --runtime=jsc --debug=jsc:9229 fixture.grx` + Safari `Develop → [Hostname] → JSContext` ([`m16-debug-jsc.md`](m16-debug-jsc.md)).

## Cross-references

### Phase docs
- [`m16-architecture.md`](m16-architecture.md) — overall data flow + layer diagram.
- [`m16-prometheus.md`](m16-prometheus.md) — metric catalog (14 families).
- [`m16-management-api.md`](m16-management-api.md) — endpoint catalog (9 routes).
- [`m16-tui.md`](m16-tui.md) — `grx observer` keybinds + recipes.
- [`m16-otel.md`](m16-otel.md) — OTel + cross-node tracing.
- [`m16-debug-cpython.md`](m16-debug-cpython.md) — Axis B Python via debugpy + VS Code/PyCharm.
- [`m16-debug-jsc.md`](m16-debug-jsc.md) — Axis B JSC via Safari Web Inspector + lockdownd.
- [`m16-p4-framework-decision.md`](m16-p4-framework-decision.md) — why axum 0.7.
- [`m16-grafana-dashboard.json`](m16-grafana-dashboard.json) — provisioning-format dashboard.

### Decisions
- [decision 0001](../decisions/0001-v8-engine-question.md) — V8 soft-demote; explains why CDP-over-TCP defers.
- [decision 0002](../decisions/0002-graphreact-is-backend-only.md) — backend-only architectural commitment.
- [decision 0003](../decisions/0003-graphreact-bifurcates-into-backend-and-db.md) — Backend / DB product split.

### Related milestones
- [`m14-distribution.md`](m14-distribution.md) — M14 distribution (the cluster substrate that M16-Backend observes).
- [`m15-hot-reload.md`](m15-hot-reload.md) — M15 hot reload (the third event source for `on_hot_reload`).

### Memories
- [`project_graphreact_m16_backend.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend.md) — milestone aggregate index.
- Per-phase memories: `project_graphreact_m16_backend_phase{1..11}.md`.
- [`feedback_complete_substrate_not_mvp.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/feedback_complete_substrate_not_mvp.md) — substrate-not-MVP rule that shaped every phase.
- [`feedback_axis_b_no_browser.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/feedback_axis_b_no_browser.md) — Axis B engine-native protocol + native IDE rule (no browsers).
