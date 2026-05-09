# M16-Backend — Prometheus metric catalog

**Status:** SHIPPED 2026-05-08 (P11 closeout doc; metrics shipped P2 + P5 + P7).

`graphreact-observer`'s Prometheus exporter ships 14 metric families covering scheduler, supervisor, distribution (M14), hot-reload (M15), and observer-internal surfaces. Hand-rolled atomic counters / gauges / sharded histograms (no `prometheus` crate dep). Exposed at `GET /v1/metrics` on the daemon's `--observer-bind <addr>` port; auth-checked via the same Bearer-token middleware as the rest of the management API.

## Hot-path discipline

Every counter increment + gauge set + histogram observe runs from inside the scheduler hot path. Storage is lock-free per metric family:
- **Counters** — `AtomicU64` with `Relaxed` ordering. ~3ns per increment.
- **Gauges** — `AtomicU64` with `Relaxed`. ~3ns per set.
- **Histograms** — 16-shard `[AtomicU64; 14]` with thread-id-hashed shard selection. ~5ns per observe.
- **Labeled counters** — `DashMap<u64 hash, Arc<LabeledCounter>>` keyed by hashed label values; ~30ns on cache hit (no allocation), ~120ns on cache miss (label-string allocation). Hash collisions extremely improbable at our cardinality (<10K distinct label sets).

Bench gate `case_d_prometheus_sink` (P2 baseline) measured PrometheusSink at +4.55% over NoopSink — well within the plan-locked 5% gate.

## Metric families

### Scheduler counters

| Metric | Type | Labels | Source | Notes |
|---|---|---|---|---|
| `grx_messages_delivered_total` | counter | `from`, `to`, `type` | `on_message_delivered` (any disposition) | Increments regardless of `accepted`. Cardinality bounded by application's distinct {from,to,type} triples. **High-cardinality** — production deployments with thousands of nodes should add a recording rule that drops the `from`/`to` labels for headline dashboards. |
| `grx_messages_dropped_total` | counter | `reason` | Derived (see below) | `reason=policy_rejected` increments on `on_message_delivered{accepted=false}`. `reason=max_depth` increments on `on_scheduler_tick_end{terminated_by_depth_cap=true}` (one increment per saturated tick, NOT per pending message). |
| `grx_node_renders_total` | counter | `node_id` | `on_node_evaluate_end` | Increments per body call. Cardinality bounded by node count. |
| `grx_node_mount_total` | counter | (none) | `on_node_mounted` | Increments per `Scheduler::add_node_at_runtime`. Initial-mount nodes (registered via `register_node`) do NOT increment — they go through the M14/M15 substrate path which is already counted by the M14 cluster gauges. |
| `grx_node_unmount_total` | counter | `reason` | `on_node_unmounted` | `reason` is one of `"normal"`, `"shutdown"`, `"crashed"`. |

### Supervisor counters

| Metric | Type | Labels | Source | Notes |
|---|---|---|---|---|
| `grx_supervisor_restarts_total` | counter | `action` | `on_supervisor_decision` | `action` is one of `"proceeded"`, `"deferred"`, `"escalated"`. **Reframing:** plan-authored design used `{policy}` but the underlying `SupervisorDecisionMade` event carries `action`, not policy; renamed in P2 with `# HELP` documenting the deferral. |

### Queue depth gauges (sampled per cascade round)

| Metric | Type | Source | Notes |
|---|---|---|---|
| `grx_queue_depth_messages` | gauge | `on_queue_depths.snapshot.messages` | Pending in scheduler message queue. |
| `grx_queue_depth_dirty` | gauge | `on_queue_depths.snapshot.dirty` | Nodes flagged dirty for re-render. |
| `grx_queue_depth_pending_topology` | gauge | `on_queue_depths.snapshot.pending_topology` | Topology events buffered (NodeAdded / EdgeAdded / etc.). |
| `grx_queue_depth_external_inbox` | gauge | `on_queue_depths.snapshot.external_inbox` | Messages from external sources (P5 sources hooks: timer/delay/http/watch). |
| `grx_queue_depth_pending_mutations` | gauge | `on_queue_depths.snapshot.pending_mutations` | AcceptEdge / topology mutations queued. |

Sampled once per `process_batch` cascade round (NOT per message) — see `graphreact_core::telemetry::QueueDepths` doc.

### Latency histogram

| Metric | Type | Buckets | Source | Notes |
|---|---|---|---|---|
| `grx_process_batch_duration_seconds` | histogram | 14 buckets, powers-of-2 from 4µs to 65ms | `on_scheduler_tick_start` → `on_scheduler_tick_end` | Bucket boundaries `[4e-6, 8e-6, 16e-6, 32e-6, 64e-6, 128e-6, 256e-6, 512e-6, 1.024e-3, 2.048e-3, 4.096e-3, 8.192e-3, 16.384e-3, 32.768e-3, +Inf]`. Bimodal in production: light ticks (no work) at the lowest bucket, cascade rounds spread across the higher buckets. p99 is the operator-relevant SLO target (M-Prod owns the published number). |

### M14 distribution counter

| Metric | Type | Labels | Source | Notes |
|---|---|---|---|---|
| `grx_remote_messages_routed_total` | counter | `peer`, `direction` | `on_remote_message` (graphreact-dist) | `direction` is `"outbound"` or `"inbound"`. Asymmetry between in/out for a peer indicates one-direction packet drop or NAT misconfiguration; the `m16-grafana-dashboard.json` flags this with the per-peer/per-direction time series panel. **Reframing:** `direction` label was NOT in the P2 plan — added because the underlying `RemoteMessage` event carries it and emitting cost nothing extra. |

### M15 hot-reload counter

| Metric | Type | Labels | Source | Notes |
|---|---|---|---|---|
| `grx_hot_reload_total` | counter | `outcome` | `on_hot_reload` (graphreact-hotreload) | `outcome` is `"success"` or `"error"`. Spike in `error` after a deploy is a strong signal to roll back. |

### P7 WebSocket observer surfaces

| Metric | Type | Labels | Source | Notes |
|---|---|---|---|---|
| `grx_observer_subscribers_active` | gauge | (none) | `ActiveSubscriberGuard::Drop` | Live count of WebSocket subscribers on `/v1/observer/events`. |
| `grx_observer_subscribers_kicked_total` | counter | (none) | `RecvError::Lagged` handler | Increments on slow-subscriber kick. Server emits `Resync { missed }` close-frame then drops the connection. Sustained kicks indicate the broadcast capacity (1024) is too small for current subscriber count + event rate; bump in M-Prod's flow-control work. |

## Omitted from M16 (deliberate)

| Metric | Why omitted | Where it lands |
|---|---|---|
| `grx_memo_recomputes_total` / `grx_memo_skips_total` | No `on_memo_*` event in the 12-event TelemetrySink + "no new event types in P2" was the constraint. | Future event-set extension (M-Prod or post-M16). |
| Per-policy supervisor breakdown | Underlying event carries action not policy. | Future event-set extension when supervisor wires per-policy telemetry. |
| Cardinality-cap fallback for high-cardinality `grx_messages_delivered_total{from,to,type}` | Bench-verified parity with JSON sink at P2 scale; no need today. | M-Prod #6 (rate limiting + cardinality caps). |

## Discovery

The exporter is exposed under the same management-API auth + bind config as the rest of the `/v1/*` surfaces (P2 + P4 substrate). `GET /v1/metrics`:
- Default loopback: `http://127.0.0.1:9090/v1/metrics` (typical deployment).
- Bearer auth via `Authorization: Bearer <token>` header. Token resolved from `--observer-token-env <VAR>` or `--observer-token-file <path>` (mode-0600 enforced on Unix).
- TLS via `--observer-cert <path>` + `--observer-key <path>`. Required for non-loopback `--observer-bind`.
- `/v1/healthz` bypasses auth (liveness-probe-friendly); `/v1/metrics` is auth-checked because high-cardinality labels can leak topology info.

Stock Prometheus scrape config:

```yaml
scrape_configs:
  - job_name: graphreact
    scrape_interval: 5s
    metrics_path: /v1/metrics
    bearer_token_file: /etc/prometheus/grx-observer-token
    scheme: https
    tls_config:
      ca_file: /etc/prometheus/grx-ca.pem
    static_configs:
      - targets: ["grx-1.example.com:9090", "grx-2.example.com:9090"]
        labels: { cluster: prod }
```

## Recording rules (recommended)

For high-cardinality metrics, define recording rules to pre-aggregate for dashboards:

```yaml
groups:
  - name: graphreact_recording_rules
    interval: 15s
    rules:
      # Drop high-cardinality from/to labels for headline rates.
      - record: grx_messages_delivered_rate_by_type
        expr: sum by (type) (rate(grx_messages_delivered_total[1m]))

      # Per-node render rate (drop from/to from messages).
      - record: grx_node_renders_rate
        expr: sum by (node_id) (rate(grx_node_renders_total[1m]))

      # Cluster-wide remote message rate.
      - record: grx_remote_messages_rate
        expr: sum by (peer, direction) (rate(grx_remote_messages_routed_total[1m]))
```

## Cross-references

- [`m16-grafana-dashboard.json`](m16-grafana-dashboard.json) — provisioning-format Grafana dashboard using these metrics.
- [`m16-management-api.md`](m16-management-api.md) — full /v1/* endpoint catalog (this metric exporter is one of them).
- [`m16-otel.md`](m16-otel.md) — OTel traces for cross-node distributed tracing (complementary signal to these metrics).
- [`m16-architecture.md`](m16-architecture.md) — data-flow overview from scheduler events → TelemetrySink → consumers.
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase2.md` — P2 closeout (Prometheus exporter substrate landed).
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase5.md` — P5 closeout (per-node ring buffer extends the metric set with `/v1/nodes/:id/messages/recent`).
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase7.md` — P7 closeout (added `grx_observer_subscribers_*`).
