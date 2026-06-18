# M-Prod — Rate limiting (sub-track #6)

**Status:** SHIPPED Phase A 2026-06-18 — per-IP token-bucket on protected `/v1/*` routes; per-token and distributed back-ends deferred to Phase B + C.

This document covers sub-track #6 of the M-Prod milestone (production-hardening): per-source-IP request rate limiting on the management API + ingress. Sibling tracks (#1 auth + audit chain, #5 observability, #4 lifecycle / drain) reference this substrate; see [Cross-track contracts](#cross-track-contracts).

## Phase A scope

Phase A ships the **substrate** — a per-IP token-bucket layer mounted on the protected branch of the management-API router. Public health-probe routes (`/v1/healthz`, `/v1/readyz`) explicitly bypass the limiter so k8s liveness + readiness probes cannot trip a 429 on the cluster. Phase A explicitly does NOT ship per-token / per-account discrimination — corporate NAT'd clients share a bucket; see [Known limitations](#known-limitations).

| Deliverable | Status | Location |
|---|---|---|
| `tower_governor 0.5` dependency on graphreact-observer | SHIPPED | [`crates/graphreact-observer/Cargo.toml`](../../crates/graphreact-observer/Cargo.toml) |
| `GovernorLayer` mounted on `protected_routes` branch | SHIPPED | [`crates/graphreact-observer/src/management_api.rs`](../../crates/graphreact-observer/src/management_api.rs) `build_router_with_rate_limit` |
| `PeerIpKeyExtractor` keyed by `ConnectInfo<SocketAddr>` | SHIPPED | injected per accept in `accept_loop` |
| Canonical 429 response shape (`{error: "rate_limited", retry_after_secs}`) | SHIPPED | `rate_limited_response_from_error` |
| `Retry-After` header on every 429 | SHIPPED | `rate_limited_response_from_error` |
| `rate_limit_observer` middleware (audit + metric fan) | SHIPPED | `management_api.rs::rate_limit_observer` |
| `grx_rate_limit_blocked_total{route}` Prometheus counter | SHIPPED | `metrics.rs::MetricsInner::rate_limit_blocked` |
| Audit event `rate_limit_blocked` | SHIPPED | inlined in `rate_limit_observer`; rides the standard `target = "graphreact::observer::audit"` |
| `RateLimitConfig` knob struct + `SpawnOptions::rate_limit` | SHIPPED | `management_api.rs::RateLimitConfig` |
| Env-var operator overrides (`GRX_RATE_LIMIT_PER_IP_PER_SEC`, `GRX_RATE_LIMIT_PER_IP_BURST`) | SHIPPED | `RateLimitConfig::from_env` |
| Unit + integration tests | SHIPPED | `management_api::rate_limit_tests` + [`tests/rate_limit.rs`](../../crates/graphreact-observer/tests/rate_limit.rs) |
| Per-token (bearer-token-keyed) discrimination | DEFERRED → Phase B | n/a |
| Distributed / Redis-class shared limiter store | DEFERRED → Phase C | n/a |
| Timing-attack-safe auth-failure rate-limit coordination with #1 | DEFERRED → Phase B | n/a |
| `X-Forwarded-For` / `SmartIpKeyExtractor` reverse-proxy support | DEFERRED → Phase B | n/a |

## Defaults

```text
sustained:   50 requests / second / source IP   (DEFAULT_RATE_LIMIT_PER_IP_PER_SEC)
burst:      100 tokens                          (DEFAULT_RATE_LIMIT_PER_IP_BURST)
```

50 req/s sustained comfortably absorbs a single human-driven `curl` + a modest batch script while still pinning an unauth flood. Burst = 2× sustained matches the classic token-bucket convention: a page-load fanning ~30 parallel requests is well under cap, but a sustained 60-req/s curl loop trips the bucket within ~2 seconds.

## Layer ordering (the load-bearing invariant)

```text
request →
  rate_limit_observer  (outermost — captures peer + classified route, fires audit + metric on 429)
    → GovernorLayer    (per-IP token bucket; rejects over-cap with 429 BEFORE auth runs)
      → auth_layer     (BearerToken::matches; only runs if governor passed the request through)
        → handler      (registered route function)
```

Putting the GovernorLayer **outside** auth_layer is the operator contract: an unauthenticated flood over the IP cap is rejected by the governor before `BearerToken::matches` runs its constant-time `subtle::ConstantTimeEq` check. Sub-track #1's auth-side CPU budget is therefore safe from cap-bound floods.

Putting `rate_limit_observer` **outside** the GovernorLayer is also load-bearing: the governor's `error_handler` closure does NOT receive the request, so it can't read the peer or matched path. The observer middleware sits on the response path, snapshots `(peer, classified_route)` on entry, and fires the audit event + metric increment if the response status is `429 Too Many Requests`. This keeps the audit/metric contract close to the rate-limit decision without requiring a custom KeyExtractor.

`rate_limit_observer` runs OUTSIDE the GovernorLayer; an alternative is per-route audit logging inside a custom KeyExtractor. We picked the observer pattern because:

1. The default `PeerIpKeyExtractor` is the right primitive for Phase A. A custom KeyExtractor with audit-emission baked in would require us to maintain a fork of the extractor logic.
2. `tower_governor`'s `tracing` feature emits its events under target `tower_governor` (NOT `graphreact::observer::audit`), so enabling it would bypass the M-Prod #1 audit-sink fan. The observer middleware keeps the audit surface canonical.

## 429 ↔ 503 precedence (joint contract with sub-track #5)

When a client both **exceeds the per-IP cap** AND **the scheduler bridge is saturated**, the outer-runs-first invariant means the 429 fires before the 503. This is the correct operator semantic:

- `429 rate_limited`: actionable client-side — back off, retry per `Retry-After`.
- `503 bridge_saturated`: actionable operator-side — scheduler thread is slow.

Surfacing the 429 first means the client backs off without waking the operator. The full precedence ladder across all backpressure-class responses is:

```text
drain (#4)  >  rate_limit (#6)  >  auth (#1)  >  bridge_saturated (#5)
```

Read this top-to-bottom: a draining daemon responds 503 `draining` to every request regardless of IP cap. A non-draining daemon over the IP cap responds 429 regardless of auth state. A non-draining, under-cap, authenticated daemon over scheduler capacity responds 503 `bridge_saturated`.

## Canonical response envelope

Every back-pressure-class response in the management-API surface ships the SAME JSON envelope + `Retry-After` header semantics; only the `error` discriminator changes:

```json
{ "error": "rate_limited",              "retry_after_secs": 7 }   // 429
{ "error": "bridge_saturated",          "retry_after_secs": 1 }   // 503
{ "error": "draining" }                                            // 503 (no body retry; drain orchestrator stops)
{ "error": "scheduler_first_batch_pending" }                       // 503 (until first tick)
{ "error": "cluster_no_peers_connected" }                          // 503 (until ≥1 peer dialed)
```

Operator scripts greping `{rate_limited, bridge_saturated, scheduler_first_batch_pending, cluster_no_peers_connected, draining}` find every backpressure-class response in the same envelope. Drift here would silently break operator triage; the unit test `rate_limited_response_shape_matches_bridge_saturated_envelope` pins the contract at build time.

`Retry-After` is a positive-integer count of seconds (RFC 7231 §7.1.3 delta-seconds). For 429s, the value is `tower_governor`'s `wait_time_from_too_many_requests` clamped to a 1-second minimum (a 0-value would tell the client to retry immediately, spinning).

## Observability

### Prometheus

```
# HELP grx_rate_limit_blocked_total Per-route count of HTTP requests rejected by the per-IP rate limiter with 429. ...
# TYPE grx_rate_limit_blocked_total counter
grx_rate_limit_blocked_total{route="/v1/nodes/:id/messages"} 12
grx_rate_limit_blocked_total{route="/v1/identity"}            3
```

The `route` label is **bounded** to the 11 registered protected-route patterns plus an `"unknown"` sentinel — total cardinality ≤ 12. The rate-limit observer middleware feeds the bounded label by passing the raw URI path through `classify_protected_path`, which collapses concrete instantiations like `/v1/nodes/abc123/messages` back to the pattern `/v1/nodes/:id/messages`. Without this collapse, labelling by the raw path would emit one series per distinct node id ever rate-limited — a cardinality explosion.

The metric is **suppressed entirely** until the first 429 fires (Prometheus best practice — don't emit empty families). A steady-state daemon that never rate-limits never publishes the `grx_rate_limit_blocked_total` family.

### Audit log

```json
{"seq":42,"ts_ms":1718630400000,"kind":"rate_limit_blocked","fields":{"peer":"203.0.113.5:51234","route":"/v1/nodes/:id/messages"},"prev_hash":"…","this_hash":"…"}
```

Same `target = "graphreact::observer::audit"` target convention as `auth_fail` / `bind` / `tls_*`. When a `FileAuditSink` is wired via `SpawnOptions::audit_sink` (`graphreact-cli --observer-audit-log`), the record lands in the same NDJSON file alongside the bind + auth-fail records, hash-chained and tamper-evident.

Fields are intentionally minimal: peer + bounded route. Bearer tokens, request payloads, `Authorization` header bytes are **NEVER** included — the integration test `rate_limit_blocked_audit_event_fires` asserts no `token` / `payload` / `authorization` field keys appear in the record.

## Configuration

### Production tuning

```bash
GRX_RATE_LIMIT_PER_IP_PER_SEC=200 \
GRX_RATE_LIMIT_PER_IP_BURST=400 \
grx daemon ...
```

Both env vars accept positive integers; empty / unparseable / zero values fall through to the in-code defaults. This matches the `OBSERVER_SHUTDOWN_DEADLINE_SECS` parsing contract from sub-track #5.

Suggested re-tuning surface for operators behind a corporate NAT (many internal users behind one egress IP): bump `GRX_RATE_LIMIT_PER_IP_PER_SEC` to 500–1000 and re-tune from observed traffic. Until Phase B's per-token-key extractor lands, a NAT is the load-bearing axis for the cap.

### In-process override (tests + embedding)

```rust
use graphreact_observer::{spawn_management_api_with_options, RateLimitConfig, SpawnOptions};

let opts = SpawnOptions {
    // ...other fields...
    rate_limit: Some(RateLimitConfig::for_tests(1, 2)),  // 1 req/s, burst 2
};
let handle = spawn_management_api_with_options(opts).await?;
```

Tests use this knob to pin a deterministic cap without polluting the process-global env vars across parallel `cargo test` runners.

## Known limitations

These are deliberate Phase A trade-offs, NOT bugs:

- **Corporate NAT collapses many users into one bucket.** Phase A keys on source IP, so a corporate gateway with 500 users sharing one egress IP shares one bucket. Operators must tune `GRX_RATE_LIMIT_PER_IP_PER_SEC` upward for NAT-fronted deployments. Phase B's per-token-key extractor lifts this limitation by partitioning the bucket by bearer token.
- **Concurrent connections from one IP share the bucket.** `tower_governor`'s key is global to the IP — 50 in-flight connections from 203.0.113.5 share one cap, not 50 individual caps. This is the intended behavior for a per-IP limiter; Phase B's per-token discrimination provides the per-client cap operators may want.
- **k8s health-probe pathways must remain unrated.** Phase A applies the GovernorLayer to `protected_routes` only — `public_routes` (which holds `/v1/healthz` + `/v1/readyz`) explicitly bypasses the limiter. A regression that mounts the layer on the merged router would silently cap k8s probe traffic; the integration test `rate_limit_does_not_apply_to_public_routes` floods both public routes at the tightest cap (burst=2, per_second=1) and asserts ZERO 429s.
- **`X-Forwarded-For` is NOT honored.** A reverse proxy in front of the daemon will appear as a single source IP to the limiter. Phase B will add `SmartIpKeyExtractor` (which reads `X-Forwarded-For` and `Forwarded` headers per RFC 7239) gated behind a trust-the-proxy flag so operators behind cloud LBs / k8s ingress controllers get correct per-client discrimination.
- **No timing-attack coordination with #1's auth-failure handler.** Phase B couples the rate limiter with #1's RBAC / auth-failure handler so an attacker can't differentiate "wrong token + over cap" (429) from "wrong token + under cap" (401) by timing. Phase A keeps the surface decoupled — adversarial auth-failure timing is in #1's threat model, not #6's.
- **axum 0.7 coupling.** `tower_governor 0.5.x` is the latest release compatible with the workspace's axum 0.7 pin. When the workspace migrates to axum 0.8 (a separate, larger workstream), the `tower_governor` pin MUST bump in lockstep to 0.7+ — flag this on the axum-bump tracking issue so the limiter doesn't get silently left at 0.5 against an axum 0.8 daemon.

## Cross-track contracts

- **Sub-track #1 (RBAC / hash-chained audit log):** the `rate_limit_blocked` event uses the same `target = "graphreact::observer::audit"` convention so when #1's hash-chained sink is wired, every 429 lands in the NDJSON chain alongside `auth_fail` / `bind` / `tls_*`. No callsite change.
- **Sub-track #4 (SIGTERM drain + rolling-version):** the precedence ladder `drain > rate_limit > auth > bridge_saturated` is documented above. The integration test `rate_limit_does_not_apply_to_public_routes` indirectly guards the drain contract (drain affects `/v1/readyz` first, NOT a rate-limit-gated 429).
- **Sub-track #5 (observability completeness):** the `grx_rate_limit_blocked_total{route}` counter is one of the metric families enumerated in `docs/SLO.md`'s "Surface availability" row. The 429-vs-503 precedence audit (originally listed as Open work in #5) is replaced by the precedence ladder section above.
- **Sub-track #8 (packaging / helm chart):** the helm chart's `readinessProbe` + `livenessProbe` point at `/v1/readyz` + `/v1/healthz` — those routes bypass the limiter unconditionally, so a misconfigured probe that polls aggressively cannot 429.
- **Sub-track #9 (chaos + fuzz):** the `fuzz_management_api_bodies` libFuzzer target landed in #9 Phase α does NOT exercise the rate limiter (it fuzzes JSON request bodies, not request rate). Phase B may add a `chaos_rate_limit_burst` cell to the chaos arm.

## Phase B / C / D roadmap

| Phase | Theme | Trigger |
|---|---|---|
| B | Per-token-key extractor | `KeyExtractor` impl reading `Authorization: Bearer …`; falls back to `PeerIpKeyExtractor` when no bearer is present. Lifts NAT limitation. |
| B | `SmartIpKeyExtractor` for `X-Forwarded-For` / `Forwarded` | Operators behind cloud LB / k8s ingress get correct per-client IP discrimination, gated behind `--trust-x-forwarded-for` flag. |
| B | Auth-failure rate coupling with #1 | Bucket separately on `auth_fail` events so an attacker can't time-attack-distinguish 401 from 429. |
| B | `tracing` feature audit + opt-in | Audit whether `tower_governor`'s own tracing events overlap with `graphreact::observer::audit` cleanly; opt-in if so. |
| C | Distributed / Redis-class shared store | Multi-replica daemons share rate-limit state via Redis or a similar TTL-backed store. Today's in-memory state is per-replica. |
| C | Per-account / per-organisation quotas | Carve the bucket by an `Organisation` header the RBAC track introduces. |
| D | Adaptive caps from `MetricsInner` feedback | Auto-tune per-IP cap based on observed scheduler load — feedback loop between #5 and #6. |
