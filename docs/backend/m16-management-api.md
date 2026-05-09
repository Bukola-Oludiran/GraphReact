# M16-Backend — Management API endpoint catalog

**Status:** SHIPPED 2026-05-08 (P11 closeout doc; endpoints shipped P2 + P4 + P5 + P7).

`graphreact-observer`'s management API exposes runtime introspection over HTTP under `/v1/*`. axum 0.7 framework (decision artifact: [`m16-p4-framework-decision.md`](m16-p4-framework-decision.md)). All endpoints share the same Bearer-token auth middleware + bind config + TLS as the metrics exporter.

## `/v1/` versioning commitment

Endpoints under `/v1/*` are **stable + documented + versioned** from day one. Companion ecosystem (e.g. a third-party React visualizer per [decision 0002](../decisions/0002-graphreact-is-backend-only.md)) pins against this version. Breaking changes get `/v2/`; `/v1/` remains addressable until M-Prod's deprecation policy says otherwise.

## Auth + bind

| Endpoint | Auth | Notes |
|---|---|---|
| `GET /v1/healthz` | **Bypassed** | Liveness probe; intentionally auth-free so k8s readiness probes don't need the bearer token. |
| `GET /v1/identity` | Required | Returns process_start_id; ops tooling uses this for crash-detection (changes on daemon restart). |
| All other `/v1/*` | Required | Bearer token via `Authorization: Bearer <token>` header. |

Auth failure → `401 Unauthorized` + `WWW-Authenticate: Bearer realm="grx-observer"` header.

Default bind: `127.0.0.1:9090`. Non-loopback bind requires all of `--observer-token-{env|file}` + `--observer-cert` + `--observer-key`; refused at config-load time otherwise.

## Endpoint reference

### `GET /v1/healthz`

Liveness probe. Always returns `200 OK` with empty body.

```
$ curl http://127.0.0.1:9090/v1/healthz
$ echo $?
0
```

### `GET /v1/identity`

Daemon identity for crash-detection.

**Response** (200 OK):
```json
{
  "process_start_id": 14842763098273647128,
  "version": "0.1.0",
  "api_version": 1
}
```

`process_start_id` is a random `u64` minted at daemon start (via `rand::thread_rng().next_u64()`); changes on every restart. Ops tooling watching for daemon restarts polls this every N seconds and triggers reconnect/state-clear on a change.

### `GET /v1/metrics`

Prometheus exposition (text format). See [`m16-prometheus.md`](m16-prometheus.md) for the full metric catalog.

**Response** (200 OK, content-type `text/plain; version=0.0.4`):
```
# HELP grx_messages_delivered_total Cumulative count of messages delivered through the scheduler...
# TYPE grx_messages_delivered_total counter
grx_messages_delivered_total{from="alice",to="bob",type="ping"} 4823
...
```

### `GET /v1/nodes`

Snapshot of every registered node + its supervisor mapping.

**Response** (200 OK):
```json
[
  {
    "id": "alice",
    "role": "Greeter",
    "depth": 0,
    "path": [],
    "supervisor": "sup:1"
  },
  {
    "id": "bob",
    "role": "Listener",
    "depth": 1,
    "path": ["alice"]
  }
]
```

`supervisor` field is `null` (omitted from JSON) for top-level nodes. `path` is the path from root to the node (excluding the node itself). `depth` is the path length.

Drained from the scheduler via the cross-thread bridge (P4 substrate); worst-case latency one `process_batch` round (~50ms typical).

### `GET /v1/nodes/:id`

Single-node lookup. Returns the same shape as one element of `/v1/nodes`.

**Response** (200 OK):
```json
{
  "id": "alice",
  "role": "Greeter",
  "depth": 0,
  "path": [],
  "supervisor": "sup:1"
}
```

**404 Not Found** if `id` doesn't resolve to a registered node.

### `GET /v1/nodes/:id/messages/recent?limit=N`

Bounded ring buffer of the most recent messages delivered to `id`. P5 substrate.

**Query params:**
- `limit` (optional, default `64`, max `256`) — how many messages to return.

**Response** (200 OK):
```json
[
  {
    "from": "alice",
    "to": "bob",
    "message_type": "ping",
    "ttl": 4,
    "direct": false,
    "accepted": true,
    "ts_ms": 1715184523456
  }
]
```

**Newest first.** `ts_ms` is wall-clock millis at delivery time (sender side; not Lamport).

**PII safe-by-default**: `payload` is **NEVER** included. Operators wanting payload visibility use the P3 JSON-log stream (which IS payload-aware AND PII-aware via the manifest annotation system).

**404 Not Found** if `id` has no ring buffer (node never delivered any messages OR was never registered).

### `GET /v1/supervisor/tree`

Snapshot of every supervisor + its current-membership children.

**Response** (200 OK):
```json
[
  {
    "key": "sup:0",
    "strategy": "OneForOne",
    "max_restarts": 3,
    "period_secs": 5,
    "restart_count": 0,
    "children": ["alice", "bob"]
  },
  {
    "key": "sup:1",
    "strategy": "RestForOne",
    "max_restarts": 5,
    "period_secs": 60,
    "restart_count": 2,
    "children": ["worker-1", "worker-2", "worker-3"]
  }
]
```

`key` is the stable string form of the supervisor's `SupervisorKey` slotmap index — `"sup:0"`, `"sup:1"`, etc. Stable across the lifetime of one daemon process; resets on restart.

`children` reflects RUNTIME state from the `node_to_supervisor` reverse index (NOT the registered child specs which may include children awaiting restart).

### `GET /v1/observer/events` (WebSocket)

Live event stream over WebSocket. P7 substrate.

**Upgrade request:**
```
GET /v1/observer/events HTTP/1.1
Host: 127.0.0.1:9090
Authorization: Bearer <token>
Upgrade: websocket
Connection: Upgrade
Sec-WebSocket-Version: 13
Sec-WebSocket-Key: ...
```

**Query params (optional filters, AND-semantics):**
- `?node=<id>` — only events touching this node (`from` or `to` matches).
- `?type=<kind>` — case-insensitive match on the event discriminant (e.g. `message_delivered`, `node_evaluate_start`).
- `?since=<ts_ms>` — strict greater-than filter on `ts_ms`. Useful as a re-subscribe cursor on reconnect.

**Frame format:** JSON text frames, one event per frame. Wire envelope:
```json
{
  "kind": "message_delivered",
  "ts_ms": 1715184523456,
  "from": "alice",
  "to": "bob",
  "type": "ping",
  "ttl": 4,
  "direct": false,
  "accepted": true,
  "trace_context": null
}
```

`kind` discriminates the variant (snake_case): `tick_start`, `tick_end`, `queue_depths`, `node_evaluate_start`, `node_evaluate_end`, `message_delivered`, `node_mounted`, `node_unmounted`, `topology_event`, `supervisor_decision`, `remote_message`, `hot_reload`, `resync`. Unknown kinds (forward-compat) ignored client-side.

**Slow-subscriber handling**: when a subscriber falls behind by `>1024` events (broadcast capacity), the server emits:
```json
{ "kind": "resync", "ts_ms": ..., "missed": 4321 }
```
…then sends a close-frame and drops the connection. `grx_observer_subscribers_kicked_total` counter increments. Client should refetch state via REST + reconnect.

### `GET /v1/observer/events.sse` (Server-Sent Events)

Same event stream, SSE transport for non-WebSocket clients.

**Response** (200 OK, content-type `text/event-stream`):
```
data: {"kind":"tick_start","ts_ms":1715184523456}

data: {"kind":"message_delivered","ts_ms":1715184523457,"from":"alice","to":"bob",...}

```

Same query-param filters + same JSON envelope as WebSocket.

## Cross-thread bridge architecture

axum runs on multi-threaded tokio; `Scheduler` is `!Send` (single-threaded by design). Handlers post `SnapshotRequest` enums onto a 256-slot bounded mpsc channel; the scheduler thread drains via `drain_snapshot_requests(rx, |req| ...)` after each `process_batch` round (called from `commands.rs::drive_handle_until`). Each request carries an embedded `oneshot::Sender<Response>`. Full channel returns `503 Service Unavailable` (substrate-not-MVP backpressure visible from day one).

Worst-case latency: one `process_batch` round (~50ms typical at idle, sub-ms under load).

## OpenAPI / schema

Not yet machine-readable. P11 ships a hand-written catalog (this doc); M-Prod can layer in `utoipa` to derive an OpenAPI 3.0 spec from the axum router + handler types if companion-ecosystem demand justifies the dep cost.

## Cross-references

- [`m16-architecture.md`](m16-architecture.md) — overall data flow + how this API fits in.
- [`m16-prometheus.md`](m16-prometheus.md) — full metric catalog (one of the endpoints above).
- [`m16-tui.md`](m16-tui.md) — `grx observer` ratatui TUI consumes this API.
- [`m16-otel.md`](m16-otel.md) — OTel cross-node tracing (orthogonal signal to these endpoints).
- [`m16-p4-framework-decision.md`](m16-p4-framework-decision.md) — why axum 0.7 (vs hyper / actix-web / warp / poem).
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase4.md` — P4 closeout (mgmt API foundation + cross-thread bridge).
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase5.md` — P5 closeout (added per-node ring + `grx ps/tree/top/trace` CLI).
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase7.md` — P7 closeout (added WebSocket + SSE).
