# GraphReact published SLOs

**Status:** SHIPPED 2026-06-17 (M-Prod sub-track #5 residual). Living document — engine-attributed numbers land once M17/M22 per-engine cardinality lock settles (see [Blocked rows](#blocked-rows)).

This document captures the production-readiness SLOs that GraphReact operators can rely on TODAY. Each row carries:

- **Surface** — the user-facing endpoint or substrate event.
- **SLO target** — p99 (or other quantile if noted) with the test-fixture conditions.
- **Source of truth** — where the number is measured.
- **Verification** — how an operator confirms the daemon meets the target.
- **Notes** — caveats, environmental dependencies, blocked-on links.

A target marked `BLOCKED ON …` is a known gap; the row is preserved so operators see WHICH commitment is missing rather than discovering it on the day they need it.

## Management-API surfaces

| Surface | SLO target | Source of truth | Verification | Notes |
|---|---|---|---|---|
| `GET /v1/healthz` | p99 < 10 ms (loopback, idle daemon) | constant-time handler in [`management_api.rs::healthz`](../crates/graphreact-observer/src/management_api.rs); no scheduler thread touch | `hey -n 10000 -c 16 http://127.0.0.1:9090/v1/healthz` | Always returns 200 + body `ok`. Designed for k8s liveness probes. |
| `GET /v1/readyz` | p99 < 50 ms (loopback, idle daemon) | predicate over `(process_batch_count > 0, snapshot_tx.capacity() > 0, cluster_readyz())` — three Arc-shared atomic reads | `hey -n 10000 -c 16 http://127.0.0.1:9090/v1/readyz` after boot | Returns 503 + `Retry-After: 1` with one of `scheduler_first_batch_pending` / `bridge_saturated` / `cluster_no_peers_connected` during partial init. See [`m-prod-observability.md`](backend/m-prod-observability.md). |
| `GET /v1/metrics` | p99 < 500 ms (idle daemon, ~1k metric series) | `PrometheusSink::encode_text` walks `MetricsInner` in single pass | `hey -n 1000 -c 8 http://127.0.0.1:9090/v1/metrics` | Cardinality cap policy: `MetricsInner` rejects new labels past `MAX_LABEL_CARDINALITY` (currently un-pinned — see [`Blocked rows`](#blocked-rows)). |
| `POST /v1/nodes/:id/messages` round-trip | p99 < 100 ms (single-node, scheduler 50 ms tick) | inject_message_handler → `snapshot_tx.try_send` → scheduler drain → oneshot reply | `wrk -t4 -c64 -d30s -s scripts/inject_one.lua http://127.0.0.1:9090/v1/nodes/X/messages` | Saturated bridge surfaces as 503 + `Retry-After: 1` via [`bridge_saturated_response`](../crates/graphreact-observer/src/management_api.rs). Operators retry on 503; idempotency-key dedup keeps retries safe. |
| `POST /v1/nodes/:id/call` round-trip | p99 < 200 ms (single-node) | inject_call_handler → bridge → reducer → reply over `PendingCalls` oneshot | `wrk -t4 -c32 -d30s -s scripts/call_one.lua` | Same try_send→503 policy as the messages endpoint; correlation-id timeout configurable via `timeout_ms` body field. |
| `POST /v1/nodes/:id/messages/bulk` per-line | p99 < 1 ms (per-line dispatch budget) | bulk handler tries each line via `snapshot_tx.try_send`; backpressured lines surface as `{"line":N,"result":"backpressure_503"}` | `curl --data-binary @bulk.ndjson -X POST http://127.0.0.1:9090/v1/nodes/X/messages/bulk` | Per-line cap: `DEFAULT_BULK_LINE_CAP = 10_000`; per-line byte cap: `BULK_MAX_LINE_BYTES = 1 MiB`; total-body cap: `BULK_MAX_BODY_BYTES = 10 GiB` (pre-flight rejected at 413). |

## Live event stream

| Surface | SLO target | Source of truth | Verification | Notes |
|---|---|---|---|---|
| `GET /v1/observer/events` (WebSocket) — subscribe roundtrip | p99 < 100 ms (loopback, idle daemon) | WebSocket upgrade + first delta frame after subscription `filter` | manual: `wscat -c ws://127.0.0.1:9090/v1/observer/events?node=X` | `Lagged` re-sync via `Resync` event when slow subscriber falls behind; backpressure policy: drop-oldest on the broadcast channel. |
| `GET /v1/observer/events.sse` (SSE fallback) — first byte | p99 < 100 ms | same as WS path; uses axum's SSE response shape | manual: `curl -N http://127.0.0.1:9090/v1/observer/events.sse?node=X` | Heartbeat every 15 s; reconnect via `Last-Event-ID` header. |

## Cluster / dist surfaces

| Surface | SLO target | Source of truth | Verification | Notes |
|---|---|---|---|---|
| `Cluster::start` boot-to-ready | p99 < 5 s (3-node cluster, mTLS handshake) | end of dial loop in `start_daemon` (`commands.rs`) | `time cargo test --release -p graphreact-dist -- cluster_three_node_fixture` | Includes TLS handshake + cookie validation + first `PgSync` exchange. |
| Cluster reconvergence after partition heal | BLOCKED ON Jepsen-style harness (CI-infra track) | n/a | n/a | Bench shape locked: 3-node cluster, partition 30 s, heal, measure first-`PgDiscover` round trip. CI-infra track owns the harness. |

## Engine + scheduler surfaces

| Surface | SLO target | Source of truth | Verification | Notes |
|---|---|---|---|---|
| Scheduler `process_batch` tick cadence | 50 ms tick, p99 < 75 ms (idle daemon) | `drive_handle_until` in `commands.rs` | `grx_engine_eval_count` increment cadence on `/v1/metrics` | Per-tick eval count emission cadence fixed via M22 verify patch (`commands.rs:1828`). Heavy bodies extend the tick — operators monitor `process_batch_duration` histogram. |
| Per-engine `grx_engine_eval_count` emission | per-tick (50 ms cadence) | `record_engine_eval_metrics` called inside `drive_handle_until` tick body | scrape `/v1/metrics` 10 times in 1 s; expect ≥ 20 counter increments per active engine | Pre-M22-patch this was a one-shot snapshot. M22 verify finding `grx_engine_eval_count one-shot` (commit `7fe1d3a`) restored per-tick emission. |
| Body-eval latency under mixed runtime (per engine) | BLOCKED ON M17/M22 per-engine cardinality lock | n/a | n/a | Per-engine numbers (JSC vs CPython vs Go body eval p99) require the label cardinality contract to settle. Tracked under [`m22-mixed-runtime.md`](backend/m22-mixed-runtime.md). |

## Blocked rows

The following SLOs are deferred because they depend on a substrate that hasn't shipped yet:

1. **Cluster reconvergence after partition heal** — blocked on the Jepsen-style harness CI infrastructure (see the M-Prod remainder plan; CI-infra track).
2. **TLS rotation latency** — blocked on M-Prod sub-track #2 SIGHUP atomic-swap primitive.
3. **Drain window** — blocked on M-Prod sub-track #4 SIGTERM drain semantics. Once shipped, `/v1/readyz` flips to 503 at drain start so the LB de-routes before the daemon stops accepting calls.
4. **OTel exporter reachability** — blocked on CI-infra OTel-collector fixture; the `BatchSpanProcessor` doesn't expose a connect-failure handle that `/v1/readyz` could read today.
5. **Per-engine label cardinality lock** — blocked on M17 + M22 closure. Without the lock, per-engine SLOs (eval p99, GC pause, RSS per engine) would commit to a cardinality contract that may need to widen.
6. **24h fuzz cumulative corpus growth** — blocked on the M-Prod #9 24h CI fuzz cell + 7-day soak fixture (both CI-infra-bound).
7. **`POST /v1/nodes/:id/call` 1k-concurrent-call p99** — needs the M-Prod #6 rate-limit shape to settle (governor wraps the management API).

## Verification recipe (operator runbook)

To verify a deployment against this SLO set:

```sh
# 1. Liveness + readiness
hey -n 10000 -c 16 https://host:9090/v1/healthz   # expect p99 < 10 ms
hey -n 10000 -c 16 https://host:9090/v1/readyz    # expect p99 < 50 ms (after first batch)

# 2. Metrics scrape
hey -n 1000 -c 8  https://host:9090/v1/metrics   # expect p99 < 500 ms

# 3. Ingress round-trip (one-shot)
wrk -t4 -c64 -d30s -s scripts/inject_one.lua https://host:9090/v1/nodes/SUPERVISOR/messages   # expect p99 < 100 ms

# 4. Per-tick eval count emission cadence
for i in $(seq 1 10); do curl -s https://host:9090/v1/metrics | grep grx_engine_eval_count; sleep 0.1; done
# expect at least 20 distinct counter values across the 10 samples for each active engine

# 5. Backpressure under saturation
wrk -t8 -c512 -d60s -s scripts/inject_one.lua https://host:9090/v1/nodes/SUPERVISOR/messages
# expect a fraction of responses to be 503 + Retry-After: 1; ZERO timeouts; ZERO 5xx other than 503
```

If any row in the table fails its target on a deployment that matches the row's environmental notes, treat it as a sub-track #5 follow-on signal and file against the M-Prod milestone with the row name.

## Cross-references

- [`m-prod-observability.md`](backend/m-prod-observability.md) — observability sub-track narrative (where this SLO doc was first promised in the Open Work section).
- [`m-prod-secrets.md`](backend/m-prod-secrets.md) — neighbouring M-Prod sub-track.
- [`m-prod-supply-chain.md`](backend/m-prod-supply-chain.md) — neighbouring M-Prod sub-track.
- [`m16-management-api.md`](backend/m16-management-api.md) — the management-API substrate this SLO measures.
- [`m22-mixed-runtime.md`](backend/m22-mixed-runtime.md) — per-engine cardinality story (referenced from blocked rows).
