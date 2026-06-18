# M-Prod — Observability completeness (sub-track #5)

**Status:** STARTED 2026-06-17 with the safest slice. Tracks M-Prod sub-track #5 as defined in the master plan; closes the M16-Backend "deferred to M-Prod" observability rows.

This document lives alongside [`m16-backend-observer.md`](m16-backend-observer.md) (which covers the M16-Backend baseline) and extends it with the M-Prod hardening passes.

## Sub-track #5 scope recap

The M-Prod plan lists these observability deliverables:

1. **`/v1/readyz`** — Kubernetes-shaped readiness probe distinct from `/v1/healthz` (liveness). Default-routes traffic away from a daemon that's LIVE-but-NOT-READY.
2. **`OBSERVER_SHUTDOWN_DEADLINE`** — bounded teardown of the OTel `SdkTracerProvider` so an unreachable collector can't wedge `DaemonHandle::shutdown` past k8s `terminationGracePeriodSeconds`.
3. **`docs/SLO.md`** — published p99 commitments (ingress call, snapshot bridge propagation, WebSocket fan-out latency).
4. **mpsc::Sender backpressure audit** — uniform policy across every `mpsc::Sender` in the observer surface (today: mixed `try_send`→503 and `send().await`).
5. **OTel `case_g_batch` bench variant** — measure the BatchSpanProcessor cost under a real tokio runtime context (today's `case_g_otel_sink` uses SimpleSpanProcessor + overstates per-emit cost).

The **safest slice** lands #1 + #2 (this commit pair); items #3–#5 are addressed in follow-on slices inside the same sub-track and listed under [Open work in #5](#open-work-in-5).

## Shipped — `/v1/readyz`

`GET /v1/readyz` is a public (auth-bypassed) readiness probe alongside `/v1/healthz`.

**Predicate** (handler in [`management_api.rs::readyz`](../../crates/graphreact-observer/src/management_api.rs)):

| Term | Source | Why |
|---|---|---|
| Drain in progress (sub-track #4 Phase α, shipped 2026-06-17) | `state.draining.as_ref().is_some_and(|f| f.is_draining())` — relaxed `AtomicBool` load | First-branch predicate evaluated BEFORE the trio below. When set, return 503 + body `draining` ahead of any other not-ready reason. See [`m-prod-lifecycle.md`](m-prod-lifecycle.md) for the SIGTERM wiring + budget math. |
| Scheduler has completed ≥1 `process_batch` round | `MetricsInner::process_batch_duration.snapshot().count > 0` | The histogram is updated by `PrometheusSink::on_tick_end` exactly once per scheduler tick. Count > 0 means the first mount cascade drained, so the daemon's view of the `.grx` system is consistent. |
| Snapshot bridge has capacity | `state.snapshot_tx.capacity() > 0` | Capacity at zero means the scheduler thread is wedged behind a long `process_batch` round AND every `SnapshotRequest` slot is full. Inbound HTTP would 503 anyway; pre-emptively de-route via readiness rather than serve a stale snapshot. |
| Cluster membership reached (M-Prod residual, shipped 2026-06-17) | `cluster_readyz()` probe — returns `true` if `configured_peers == 0` OR `dialed_count >= 1` | Single-node deploys (no peers configured) pass trivially. Multi-node deploys flip to ready only once at least one peer dial completed, so load balancers don't route to a daemon whose `Cluster::start` hasn't yet handshook with anyone. Lock-free `AtomicUsize` read on the hot path. |

**Response shape:**

| State | Status | Body | Headers |
|---|---|---|---|
| Ready | 200 OK | `ready` | `Content-Type: text/plain; charset=utf-8` |
| **Draining (sub-track #4 Phase α)** | **503 Service Unavailable** | **`draining`** | **`Content-Type: text/plain; charset=utf-8`, `Retry-After: 1`** |
| Pre-tick | 503 Service Unavailable | `scheduler_first_batch_pending` | `Content-Type: text/plain; charset=utf-8`, `Retry-After: 1` |
| Bridge saturated | 503 Service Unavailable | `bridge_saturated` | `Content-Type: text/plain; charset=utf-8`, `Retry-After: 1` |
| Cluster peers configured but none dialed (M-Prod residual) | 503 Service Unavailable | `cluster_no_peers_connected` | `Content-Type: text/plain; charset=utf-8`, `Retry-After: 1` |

**Auth contract:** identical to `/v1/healthz` — the route is mounted on `public_routes` (no `auth_layer` middleware). Probes ride the k8s sidecar without access to the operator secret store; the readiness predicate is derived from `MetricsInner` + bridge capacity, so the response neither leaks secrets nor depends on per-probe authentication.

**k8s configuration** (paste into the helm chart when sub-track #8 lands):

```yaml
livenessProbe:
  httpGet:
    path: /v1/healthz
    port: observer
  initialDelaySeconds: 5
  periodSeconds: 10
readinessProbe:
  httpGet:
    path: /v1/readyz
    port: observer
  initialDelaySeconds: 1
  periodSeconds: 2
  failureThreshold: 3
```

**Tests:** `crates/graphreact-observer/tests/management_api.rs`:

- `readyz_503_before_first_batch_tick` — pre-tick path emits 503 + `Retry-After`, body names the reason.
- `readyz_200_after_first_batch_tick` — direct `process_batch_duration.observe(...)` flips the predicate to green.
- `readyz_bypasses_auth_when_token_required` — 503 (not 401) when the protected branch has a bearer token configured.

## Shipped — `OBSERVER_SHUTDOWN_DEADLINE`

Default: `5` seconds (constant `DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS` in `crates/graphreact-cli/src/commands.rs`).

Overridable via the `OBSERVER_SHUTDOWN_DEADLINE` env var (integer seconds). Empty / unparseable / zero values fall back to the default; the zero case in particular would race the OTel join to instant timeout and emit a guaranteed `otel_shutdown_timeout` audit-log line on every clean shutdown, which is noisier than just falling back.

**Wiring** (in `DaemonHandle::shutdown`):

```rust
let deadline = observer_shutdown_deadline();
let join = tokio::task::spawn_blocking(move || {
    let _ = provider.shutdown();
});
match tokio::time::timeout(deadline, join).await {
    Ok(_) => {}
    Err(_) => {
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "otel_shutdown_timeout",
            deadline_ms = deadline.as_millis() as u64,
            "OTel provider shutdown exceeded OBSERVER_SHUTDOWN_DEADLINE",
        );
    }
}
```

**Why this matters:** k8s sends SIGTERM, waits `terminationGracePeriodSeconds` (default 30s), then SIGKILLs. If the OTel collector has gone away (network partition, OOM, drained ahead of us), `SdkTracerProvider::shutdown()` can otherwise block for the full grace period — silent loss of any SIGTERM-arrival audit-log line that would otherwise let operators tell "graceful drain" apart from "SIGKILL'd by kubelet". The deadline forces the issue: bounded teardown + audit-log surfaces the cause.

**Audit-log event:** `event = "otel_shutdown_timeout"` on the existing `graphreact::observer::audit` target. Operators see this in the same JSON log stream as bind/accept/handshake-fail.

**Tests:** `commands::tests::observer_shutdown_deadline_env_var` — verifies the env-var contract (unset / empty / unparseable / zero all fall through; positive integer is accepted as seconds).

## Shipped — `case_g_batch` bench

**M-Prod sub-track #5 Phase B closeout, 2026-06-18.** A `case_g_batch` Criterion variant + a `case_g_batch_no_loss` integration test land alongside the existing `case_g_otel_sink` SimpleSpanProcessor cell. The new variant uses `opentelemetry_sdk::trace::BatchSpanProcessor` — the **sync** variant, NOT `span_processor_with_async_runtime::BatchSpanProcessor` — matching the production [`build_otel_sink`](../../crates/graphreact-observer/src/otel.rs) constructor's `.with_batch_exporter(...)` call.

**Bench location:** `crates/graphreact-observer/benches/scheduler_overhead.rs::bench_case_g_batch` (gated behind `otel-test` because `InMemorySpanExporter` lives under `opentelemetry_sdk/testing`).

**Lossless-export test:** `crates/graphreact-observer/tests/otel_sink.rs::case_g_batch_no_loss` — synthesizes a fixed event sequence (4 ticks × (1 + 3 node.evaluate + 2 message.deliver) = 24 spans), `force_flush`es through the `BatchSpanProcessor`, and asserts `get_finished_spans().len() == 24`. Pinning the lossless invariant against future processor or queue-depth tuning.

**Measured 2026-06-18 (macOS dev, release, `--measurement-time 5`):**

| Variant | Time | vs `case_a` baseline |
|---|---|---|
| `case_a_trait_not_attached` | 17.7 ms | — |
| `case_g_batch` (BatchSpanProcessor) | 19.9 ms | **+12%** |
| `case_g_otel_sink` (SimpleSpanProcessor) | 23.6 ms | +33% |

The batch processor saves ~21 percentage points of overhead vs the synchronous `SimpleSpanProcessor` at the fixture's ~9000 spans / iteration. Variance is wide (per-iteration flush + drop jitter) — the ratio band is the load-bearing number, not the absolute time.

**Substrate notes:**

- The sync `BatchSpanProcessor` uses a dedicated background **OS thread** for the drain (NOT a tokio task). The bench and test wrap iteration in a multi-thread tokio runtime so the `SdkTracerProvider` is BUILT inside a runtime context — parity with the production-side `opentelemetry_otlp::SpanExporter::with_tonic()` constraint that the provider be created inside a tokio runtime so the `grpc-tonic` channel can initialize. The processor itself does not need the runtime.
- `BatchSpanProcessor::shutdown()` and `force_flush()` are both **blocking** under the sync variant. The bench wraps shutdown in `tokio::task::spawn_blocking` per the upstream warning at `opentelemetry_sdk-0.31.0/src/trace/span_processor.rs:279-282` (calling from a current-thread tokio main thread deadlocks). The test uses the same wrap for `force_flush` so a multi-thread worker can park while the blocking call joins.
- The test reads `get_finished_spans()` AFTER `force_flush` but BEFORE `shutdown` because `InMemorySpanExporter::shutdown` by default RESETS its internal buffer (`should_reset_on_shutdown=true`). Calling shutdown first would clear the spans the assertion needs.

**Run locally:**

```bash
cargo test  -p graphreact-observer --features otel-test --test otel_sink case_g_batch_no_loss
cargo bench -p graphreact-observer --features otel-test --bench scheduler_overhead case_g_batch -- --warm-up-time 1 --measurement-time 5
```

**BatchSpanProcessor connect handle for readyz:** unchanged. The bench/test use `InMemorySpanExporter`, which has no connect handle. The "OTel exporter reachable" readyz extension at the bottom of [Open work in #5](#open-work-in-5) stays deferred — exposing connect-state from `opentelemetry_otlp::SpanExporter` is its own next slice.

## Open work in #5

- ~~**`docs/SLO.md`**: not yet authored~~ — **SHIPPED 2026-06-17.** See [`docs/SLO.md`](../../docs/SLO.md). Engine-attributed rows are explicitly marked `BLOCKED ON M17/M22 per-engine cardinality lock`.
- ~~**mpsc::Sender uniform backpressure audit**~~ — **SHIPPED 2026-06-17 (residual slice).** Ingress endpoints (`POST /v1/nodes/:id/{messages,call,messages/bulk}`) now use `try_send`→503 + `Retry-After: 1` via [`bridge_saturated_response`](../../crates/graphreact-observer/src/management_api.rs). Read-snapshot bridges (`nodes_list` / `node_detail` / `supervisor_tree`) keep `send().await` — those are internal observer→scheduler reads with bounded depth; blocking briefly is preferable to a 503 cascade on the snapshot surface. The bulk-response body `out_tx` (`mpsc::channel(64)`) also keeps `send().await` — that's slow-HTTP-client flow control, NOT scheduler backpressure; converting it to `try_send` would silently drop NDJSON result lines.
- ~~**Cluster-membership readyz extension**~~ — **SHIPPED 2026-06-17 (residual slice).** See predicate row above. Probe is a lock-free `Arc<dyn Fn() -> bool>` capturing `configured_peers + Arc<AtomicUsize>` so `/v1/readyz` doesn't pay any allocation per request.
- ~~**OTel `case_g_batch` bench variant**: the current `case_g_otel_sink` uses SimpleSpanProcessor and overstates per-emit cost. The production path uses BatchSpanProcessor; needs a parallel bench cell to measure it honestly.~~ — **SHIPPED 2026-06-18 (Phase B).** See [Shipped — `case_g_batch` bench](#shipped--case_g_batch-bench) above. Measured ratio drops from +33% (`case_g_otel_sink`) to +12% (`case_g_batch`) over the trait-not-attached baseline.
- ~~**Per-route 503-vs-429 ordering audit** (joint contract with sub-track #6 rate-limiter): when both fire under saturation, which precedence does the operator see? Need a single policy locked between #5 and #6.~~ — **SHIPPED 2026-06-18 (#6 Phase A).** Precedence ladder `drain > rate_limit > auth > bridge_saturated` is documented in [`m-prod-rate-limit.md`](m-prod-rate-limit.md). 429 fires before 503 when both could apply: "going too fast" is actionable client-side, "scheduler slow" is operator-side; surfacing 429 first means the client backs off without waking the operator. Canonical response envelope (same JSON shape across `rate_limited` / `bridge_saturated` / `draining` / `scheduler_first_batch_pending` / `cluster_no_peers_connected`) is unit-test-pinned via `rate_limited_response_shape_matches_bridge_saturated_envelope`.
- **Readiness predicate extensions** (deferred inside #5 until sub-track dependencies land):
  - TLS cert loaded / not-in-rotation — needs sub-track #2 SIGHUP reload primitive.
  - OTel exporter reachable — needs BatchSpanProcessor connect handle (currently the SDK absorbs the failure mode).
  - ~~Drain-in-progress flag~~ — **SHIPPED 2026-06-17 (sub-track #4 Phase α).** See [`m-prod-lifecycle.md`](m-prod-lifecycle.md). `/v1/readyz` now flips to 503 `draining` on SIGTERM-driven drain start, ahead of all other not-ready predicates.

## Cross-track contracts locked here

- **Sub-track #1 (RBAC / hash-chained audit log):** the `otel_shutdown_timeout` event uses the same `target: "graphreact::observer::audit"` convention so when #1 lands its append-only sink the event flows through automatically — no callsite change.
- **Sub-track #4 (SIGTERM drain + rolling-version):** the `OBSERVER_SHUTDOWN_DEADLINE` env var is the **OTel teardown bound**, not the **SIGTERM drain bound**. Sub-track #4 introduces its own `SHUTDOWN_DEADLINE` (or similarly-named) bound for the scheduler-drain phase. The two compose: the OTel teardown runs first inside the drain budget, so #4's bound must be ≥ #5's bound.
- **Sub-track #8 (packaging / helm chart):** the helm chart's `readinessProbe` points at `/v1/readyz` — see the YAML snippet above.
