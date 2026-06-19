# M-Prod — Rate limiting (sub-track #6)

**Status:** SHIPPED Phase A 2026-06-18 — per-IP token-bucket on protected `/v1/*` routes. SHIPPED Phase B Part 1 2026-06-18 — per-authenticated-principal (per-token) token-bucket layered ABOVE per-IP + `X-Forwarded-For` trusted-proxy opt-in honoring + 3-bucket `{per_ip, per_token, per_token_no_match}` discriminator on the audit fan + the `grx_rate_limit_blocked_total{route,bucket}` 2-label Prometheus dimension. Distributed back-end deferred to Phase B Part 2 + C.

This document covers sub-track #6 of the M-Prod milestone (production-hardening): per-source-IP + per-authenticated-principal request rate limiting on the management API + ingress. Sibling tracks (#1 auth + audit chain, #5 observability, #4 lifecycle / drain) reference this substrate; see [Cross-track contracts](#cross-track-contracts).

## Shipped — Phase B Part 1 (2026-06-18)

- Per-token `GovernorLayer` keyed by `TokenIdKeyExtractor` (constant-time match against the Group B-3 `RotatableRegistry`) layered ABOVE the Phase A per-IP layer. ONLY mounted when `state.rbac` is wired — legacy single-token spawn paths keep the Phase A surface bit-for-bit.
- Tighter per-token defaults: **20 req/s sustained + 40 burst** per principal. Env-var overrides `GRX_RATE_LIMIT_PER_TOKEN_PER_SEC` + `GRX_RATE_LIMIT_PER_TOKEN_BURST`.
- 3-bucket vocabulary on the audit fan + the Prometheus counter:
    - `per_ip` — Phase A per-source-IP bucket fired the 429.
    - `per_token` — per-token bucket fired for a KNOWN principal (the noisy client is identifiable by `TokenId`).
    - `per_token_no_match` — per-token bucket fired for `Authorization: Bearer …` carrying a token that doesn't match any registry entry, OR missing entirely. All such requests collapse to ONE shared bucket so the per-token-keyed limiter's internal state stays bounded.
- Denial precedence locked at `drain > per_token > per_ip > auth > bridge_saturated`. The outer-runs-first invariant means a per-token cap on a known principal fires BEFORE the per-IP cap, attributing the 429 to the right bucket.
- `X-Forwarded-For` trusted-proxy OPT-IN via `GRX_TRUSTED_PROXIES=<cidr,cidr,…>`. Default behavior is bit-for-bit Phase A: `X-Forwarded-For` is **IGNORED**, the per-IP bucket keys on the TCP peer IP. When `GRX_TRUSTED_PROXIES` is wired, the leftmost `X-Forwarded-For` IP is honored **ONLY** when the TCP peer is in the trusted set — the silent-spoofing footgun guard.
- `bucket` field added to the `rate_limit_blocked` audit record + a new 2-label `grx_rate_limit_blocked_total{route,bucket}` Prometheus vocabulary (cardinality bounded `≤ 12 × 3 = 36` series).
- Retry-After clamp pattern preserved on BOTH layers: one helper `rate_limited_response_from_error_with_bucket` services both per-IP and per-token error_handlers; the Phase A verify-finding patch (clamp survives tower_governor header fold) is inherited automatically by the new layer.
- New `TrustedProxyConfig` parser + matcher (hand-rolled CIDR over IPv4 + IPv6, ~80 LOC, no new crate dep) lives in `crates/graphreact-observer/src/rate_limit_keys.rs`.

## Phase A scope

Phase A ships the **substrate** — a per-IP token-bucket layer mounted on the protected branch of the management-API router. Public health-probe routes (`/v1/healthz`, `/v1/readyz`) explicitly bypass the limiter so k8s liveness + readiness probes cannot trip a 429 on the cluster. Phase B Part 1 above EXTENDS this with per-token discrimination + opt-in `X-Forwarded-For` honoring.

| Deliverable | Status | Location |
|---|---|---|
| `tower_governor 0.5` dependency on graphreact-observer | SHIPPED | [`crates/graphreact-observer/Cargo.toml`](../../crates/graphreact-observer/Cargo.toml) |
| `GovernorLayer` mounted on `protected_routes` branch | SHIPPED | [`crates/graphreact-observer/src/management_api.rs`](../../crates/graphreact-observer/src/management_api.rs) `build_router_with_rate_limit` |
| `PeerIpKeyExtractor` keyed by `ConnectInfo<SocketAddr>` (Phase A) → `TrustedProxyAwarePeerIpExtractor` (Phase B Part 1, drop-in compatible) | SHIPPED | injected per accept in `accept_loop` |
| Canonical 429 response shape (`{error: "rate_limited", retry_after_secs}`) | SHIPPED | `rate_limited_response_from_error_with_bucket` |
| `Retry-After` header on every 429 | SHIPPED | `rate_limited_response_from_error_with_bucket` |
| `rate_limit_observer` middleware (audit + metric fan) | SHIPPED | `management_api.rs::rate_limit_observer` |
| `grx_rate_limit_blocked_total{route,bucket}` Prometheus counter (2-label vocabulary) | SHIPPED | `metrics.rs::MetricsInner::rate_limit_blocked` |
| Audit event `rate_limit_blocked` with `bucket` field | SHIPPED | inlined in `rate_limit_observer`; rides the standard `target = "graphreact::observer::audit"` |
| `RateLimitConfig` knob struct + `SpawnOptions::rate_limit` | SHIPPED | `management_api.rs::RateLimitConfig` |
| Env-var operator overrides (`GRX_RATE_LIMIT_PER_IP_PER_SEC`, `GRX_RATE_LIMIT_PER_IP_BURST`, `GRX_RATE_LIMIT_PER_TOKEN_PER_SEC`, `GRX_RATE_LIMIT_PER_TOKEN_BURST`) | SHIPPED | `RateLimitConfig::from_env` |
| Per-token (bearer-token-keyed) discrimination | SHIPPED Phase B Part 1 (2026-06-18) | `TokenIdKeyExtractor` + `rate_limit_injector` |
| `X-Forwarded-For` trusted-proxy opt-in (`GRX_TRUSTED_PROXIES`) | SHIPPED Phase B Part 1 (2026-06-18) | `TrustedProxyConfig` + `SpawnOptions::trusted_proxies` |
| Unit + integration tests | SHIPPED | `management_api::rate_limit_tests` + [`tests/rate_limit.rs`](../../crates/graphreact-observer/tests/rate_limit.rs) + [`tests/rate_limit_per_token.rs`](../../crates/graphreact-observer/tests/rate_limit_per_token.rs) |
| Distributed / Redis-class shared limiter store | DEFERRED → Phase B Part 2 / C | n/a |
| Timing-attack-safe auth-failure rate-limit coordination with #1 | DEFERRED → Phase B Part 2 | n/a |

## Defaults

```text
per-IP:
  sustained:   50 requests / second / source IP   (DEFAULT_RATE_LIMIT_PER_IP_PER_SEC)
  burst:      100 tokens                          (DEFAULT_RATE_LIMIT_PER_IP_BURST)

per-token (Phase B Part 1, only when RBAC is wired):
  sustained:   20 requests / second / principal   (DEFAULT_RATE_LIMIT_PER_TOKEN_PER_SEC)
  burst:       40 tokens                          (DEFAULT_RATE_LIMIT_PER_TOKEN_BURST)
```

50 req/s sustained per-IP comfortably absorbs a single human-driven `curl` + a modest batch script while still pinning an unauth flood. Burst = 2× sustained matches the classic token-bucket convention: a page-load fanning ~30 parallel requests is well under cap, but a sustained 60-req/s curl loop trips the bucket within ~2 seconds.

20 req/s sustained per-token (Phase B Part 1) is TIGHTER than the per-IP cap because per-token traffic is single-identity by construction — one principal's normal operation rarely needs to sustain 50 req/s, and a tighter cap catches stolen/leaked credentials sooner without affecting legitimate operators. Burst stays 2× sustained (40 tokens) for the same micro-burst absorption.

## Layer ordering (the load-bearing invariant)

```text
request →
  rate_limit_observer       (outermost — captures peer + classified route, fires
                             audit + metric on 429, reads RateLimitDeniedBucket +
                             PerTokenBucketHintMarker response extensions)
    → rate_limit_injector   (Phase B Part 1; snapshots RBAC + trusted-proxies,
                             stashes TokenIdKey + bucket_hint + EffectivePeerIp
                             into request extensions; stamps PerTokenBucketHintMarker
                             onto the response on the way back)
      → per-token GovernorLayer (only when RBAC is wired; ABOVE per-IP)
        → per-IP GovernorLayer  (TrustedProxyAwarePeerIpExtractor — drop-in for
                                 PeerIpKeyExtractor, falls back to
                                 ConnectInfo<SocketAddr> when no EffectivePeerIp)
          → auth_layer          (BearerToken::matches OR auth_layer_rbac)
            → handler           (registered route function)
```

Putting both GovernorLayers **outside** `auth_layer` is the operator contract: an unauthenticated flood over either cap is rejected by the governor before `BearerToken::matches` runs its constant-time `subtle::ConstantTimeEq` check. Sub-track #1's auth-side CPU budget is safe from cap-bound floods.

Putting the **per-token** layer ABOVE the per-IP layer locks the denial precedence `per_token > per_ip`. A request blocked by the per-token cap never reaches the per-IP layer, so the 429 attributes the block to `bucket="per_token"` (or `"per_token_no_match"`) and operators can find the noisy principal by `TokenId` without sifting through IP-keyed buckets.

Putting the **`rate_limit_injector`** above both governor layers is mandatory: tower_governor's `KeyExtractor::extract` takes only `&Request<T>` (no access to `ApiState`), so the extractors read the precomputed `PerRequestRateLimitCtx` + `EffectivePeerIp` from request extensions. The injector is the only layer that has access to `ApiState.rbac` + `TrustedProxyConfig` — it does the constant-time RBAC walk + X-Forwarded-For resolution ONCE, then stashes the result into extensions for the extractors to read.

Putting `rate_limit_observer` **outside** everything else is also load-bearing: the governors' `error_handler` closures don't receive the request, so they can't read the peer or matched path. The observer middleware sits on the response path, snapshots `(peer, classified_route)` on entry, reads the `RateLimitDeniedBucket` + `PerTokenBucketHintMarker` response extensions, and fires the audit event + metric increment if the response status is `429 Too Many Requests`.

`rate_limit_observer` runs OUTSIDE the GovernorLayers; an alternative is per-route audit logging inside a custom KeyExtractor. We picked the observer pattern because:

1. The Phase A `PeerIpKeyExtractor` (and the Phase B Part 1 `TrustedProxyAwarePeerIpExtractor`) are the right primitives for their respective phases. A custom KeyExtractor with audit-emission baked in would require us to maintain forked extractor logic.
2. `tower_governor`'s `tracing` feature emits its events under target `tower_governor` (NOT `graphreact::observer::audit`), so enabling it would bypass the M-Prod #1 audit-sink fan. The observer middleware keeps the audit surface canonical.

## 429 ↔ 503 precedence (joint contract with sub-track #5)

When a client both **exceeds the per-IP cap** AND **the scheduler bridge is saturated**, the outer-runs-first invariant means the 429 fires before the 503. This is the correct operator semantic:

- `429 rate_limited`: actionable client-side — back off, retry per `Retry-After`.
- `503 bridge_saturated`: actionable operator-side — scheduler thread is slow.

Surfacing the 429 first means the client backs off without waking the operator. The full precedence ladder across all backpressure-class responses is:

```text
drain (#4)  >  rate_limit::per_token (#6)  >  rate_limit::per_ip (#6)  >  auth (#1)  >  bridge_saturated (#5)
```

Read this top-to-bottom: a draining daemon responds 503 `draining` to every request regardless of IP or token cap. A non-draining daemon over the per-token cap (for a known principal) responds 429 `rate_limited` with `bucket="per_token"` regardless of IP cap. A non-draining, under-token-cap daemon over the per-IP cap responds 429 with `bucket="per_ip"`. A non-draining, under-cap, authenticated daemon over scheduler capacity responds 503 `bridge_saturated`.

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
# HELP grx_rate_limit_blocked_total Per-(route,bucket) count of HTTP requests rejected by the rate limiter with 429. ...
# TYPE grx_rate_limit_blocked_total counter
grx_rate_limit_blocked_total{bucket="per_ip",route="/v1/nodes/:id/messages"}             12
grx_rate_limit_blocked_total{bucket="per_token",route="/v1/identity"}                     3
grx_rate_limit_blocked_total{bucket="per_token_no_match",route="/v1/nodes/:id/call"}      7
```

The `route` label is **bounded** to the 11 registered protected-route patterns plus an `"unknown"` sentinel — 12 routes. The `bucket` label (Phase B Part 1) is bounded to a closed 3-entry vocabulary `{per_ip, per_token, per_token_no_match}`. Total cardinality is `12 × 3 = 36` series. The rate-limit observer middleware feeds the bounded `route` label by passing the raw URI path through `classify_protected_path`, which collapses concrete instantiations like `/v1/nodes/abc123/messages` back to the pattern `/v1/nodes/:id/messages`.

The metric is **suppressed entirely** until the first 429 fires (Prometheus best practice — don't emit empty families). A steady-state daemon that never rate-limits never publishes the `grx_rate_limit_blocked_total` family.

### Audit log

```json
{"seq":42,"ts_ms":1718630400000,"kind":"rate_limit_blocked","fields":{"peer":"203.0.113.5:51234","route":"/v1/nodes/:id/messages","bucket":"per_token"},"prev_hash":"…","this_hash":"…"}
```

Same `target = "graphreact::observer::audit"` target convention as `auth_fail` / `bind` / `tls_*`. When a `FileAuditSink` is wired via `SpawnOptions::audit_sink` (`graphreact-cli --observer-audit-log`), the record lands in the same NDJSON file alongside the bind + auth-fail records, hash-chained and tamper-evident.

Fields are intentionally minimal: peer + bounded route + bounded bucket. Bearer tokens, request payloads, `Authorization` header bytes are **NEVER** included — the integration test `rate_limit_blocked_audit_event_fires` asserts no `token` / `payload` / `authorization` field keys appear in the record. The `bucket` field is the closed-vocabulary 3-entry discriminator `{per_ip, per_token, per_token_no_match}` Phase B Part 1 added; correlating high `per_token` counts in the audit log with a noisy `TokenId` in the `auth_success` audit stream identifies the misbehaving principal without exposing the credential.

## Configuration

### Production tuning

```bash
# Per-IP cap (Phase A; always active):
GRX_RATE_LIMIT_PER_IP_PER_SEC=200 \
GRX_RATE_LIMIT_PER_IP_BURST=400 \
# Per-token cap (Phase B Part 1; only effective when RBAC is wired):
GRX_RATE_LIMIT_PER_TOKEN_PER_SEC=50 \
GRX_RATE_LIMIT_PER_TOKEN_BURST=100 \
# X-Forwarded-For opt-in (Phase B Part 1; only effective when peer is in the trusted set):
GRX_TRUSTED_PROXIES="10.0.0.0/8,127.0.0.1,fc00::/7" \
grx daemon ...
```

All four rate-limit env vars accept positive integers; empty / unparseable / zero values fall through to the in-code defaults. This matches the `OBSERVER_SHUTDOWN_DEADLINE_SECS` parsing contract from sub-track #5.

`GRX_TRUSTED_PROXIES` is a comma-separated list of CIDR ranges + host routes (IPv4 + IPv6, with optional prefix). Parse failure logs a `trusted_proxies_parse_failure` event under target `graphreact::observer::audit` and FAIL-CLOSES (treats as if unset — `X-Forwarded-For` is IGNORED rather than honored on a malformed list).

Suggested re-tuning surface for operators behind a corporate NAT (many internal users behind one egress IP): with RBAC wired, the per-IP cap can stay TIGHT because the per-token cap is what discriminates legitimate users. Without RBAC, bump `GRX_RATE_LIMIT_PER_IP_PER_SEC` to 500–1000 and re-tune from observed traffic.

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

These are deliberate Phase A / B Part 1 trade-offs, NOT bugs:

- **Corporate NAT'd deployments now get per-token discrimination when RBAC is wired** → SHIPPED Phase B Part 1. Without RBAC the corporate NAT still collapses users into one IP bucket; operators tune `GRX_RATE_LIMIT_PER_IP_PER_SEC` upward in that configuration.
- **Concurrent connections from one IP share the bucket.** `tower_governor`'s key is global to the IP — 50 in-flight connections from 203.0.113.5 share one per-IP cap. With RBAC wired, the per-token cap discriminates by principal regardless of source IP, which is the operator's lever for "per-client cap".
- **k8s health-probe pathways must remain unrated.** Phase A applies both governor layers to `protected_routes` only — `public_routes` (which holds `/v1/healthz` + `/v1/readyz`) explicitly bypasses the limiter. A regression that mounts the layer on the merged router would silently cap k8s probe traffic; the integration test `rate_limit_does_not_apply_to_public_routes` floods both public routes at the tightest cap and asserts ZERO 429s.
- **`X-Forwarded-For` opt-in shipped Phase B Part 1.** Set `GRX_TRUSTED_PROXIES=<cidr,…>` to honor the header. The trust gate is mandatory: an untrusted peer sending `X-Forwarded-For` is silently ignored. Phase B Part 1 only parses leftmost IPv4/IPv6 (with optional port suffix); RFC 7239 `Forwarded:` header parsing defers to Phase B Part 2 if the surface expands.
- **Per-token cap is dormant when RBAC is not wired.** The per-token `GovernorLayer` is mounted only when `state.rbac = Some(...)`. Legacy single-token spawn paths keep the Phase A surface bit-for-bit — the per-token layer never fires because it would degrade to a global cap with the tight 20/40 defaults. Operators on legacy single-token paths get the per-IP cap only.
- **No timing-attack coordination with #1's auth-failure handler.** Phase B Part 2 couples the rate limiter with #1's RBAC / auth-failure handler so an attacker can't differentiate "wrong token + over cap" (429) from "wrong token + under cap" (401) by timing. Phase A + Phase B Part 1 keep the surface decoupled — adversarial auth-failure timing is in #1's threat model, not #6's.
- **axum 0.7 coupling.** `tower_governor 0.5.x` is the latest release compatible with the workspace's axum 0.7 pin. When the workspace migrates to axum 0.8 (a separate, larger workstream), the `tower_governor` pin MUST bump in lockstep to 0.7+ — flag this on the axum-bump tracking issue so the limiter doesn't get silently left at 0.5 against an axum 0.8 daemon.

## Operator decisions

### Why `X-Forwarded-For` opt-in (not opt-out)

The header is operator-controlled in the deployment topology — if the daemon sits directly on the internet, an attacker can trivially spoof `X-Forwarded-For: <any IP>` to bypass the per-IP cap. The opt-in (`GRX_TRUSTED_PROXIES`) makes the trust call explicit. The trust gate is at the TCP-peer level: only requests from a peer IN the configured trusted set get their `X-Forwarded-For` honored. This matches the standard reverse-proxy threat model and prevents the "silent honoring = client-IP-spoofing footgun" failure mode.

### Why the per-token cap is tighter than the per-IP cap (20/40 vs 50/100)

Per-token traffic is single-identity by construction: one principal's normal operation rarely needs to sustain 50 req/s. Tightening the per-token cap to 20/40 catches stolen credentials sooner without affecting legitimate operators. Operators who run a heavy-batch principal (CI runner, large dashboard scraper) tune `GRX_RATE_LIMIT_PER_TOKEN_PER_SEC` + `GRX_RATE_LIMIT_PER_TOKEN_BURST` upward for that deployment.

### Why we don't honor `X-Forwarded-For` for the per-token cap

The per-token cap keys on `TokenId` — the principal identity is determined by the matched RBAC entry, not the source IP. So `X-Forwarded-For` is structurally irrelevant to the per-token bucket; it only affects the per-IP cap.

## Cross-track contracts

- **Sub-track #1 (RBAC / hash-chained audit log):** the `rate_limit_blocked` event uses the same `target = "graphreact::observer::audit"` convention so when #1's hash-chained sink is wired, every 429 lands in the NDJSON chain alongside `auth_fail` / `bind` / `tls_*`. No callsite change.
- **Sub-track #4 (SIGTERM drain + rolling-version):** the precedence ladder `drain > rate_limit > auth > bridge_saturated` is documented above. The integration test `rate_limit_does_not_apply_to_public_routes` indirectly guards the drain contract (drain affects `/v1/readyz` first, NOT a rate-limit-gated 429).
- **Sub-track #5 (observability completeness):** the `grx_rate_limit_blocked_total{route}` counter is one of the metric families enumerated in `docs/SLO.md`'s "Surface availability" row. The 429-vs-503 precedence audit (originally listed as Open work in #5) is replaced by the precedence ladder section above.
- **Sub-track #8 (packaging / helm chart):** the helm chart's `readinessProbe` + `livenessProbe` point at `/v1/readyz` + `/v1/healthz` — those routes bypass the limiter unconditionally, so a misconfigured probe that polls aggressively cannot 429.
- **Sub-track #9 (chaos + fuzz):** the `fuzz_management_api_bodies` libFuzzer target landed in #9 Phase α does NOT exercise the rate limiter (it fuzzes JSON request bodies, not request rate). Phase B may add a `chaos_rate_limit_burst` cell to the chaos arm.

## Phase B / C / D roadmap

| Phase | Theme | Trigger |
|---|---|---|
| ~~B~~ | ~~Per-token-key extractor~~ → SHIPPED 2026-06-18 (Phase B Part 1) | `TokenIdKeyExtractor` reads precomputed `PerRequestRateLimitCtx` from request extensions injected by `rate_limit_injector`. Lifts NAT limitation when RBAC is wired. |
| ~~B~~ | ~~`SmartIpKeyExtractor` for `X-Forwarded-For` / `Forwarded`~~ → SHIPPED 2026-06-18 (Phase B Part 1) | `TrustedProxyAwarePeerIpExtractor` + `TrustedProxyConfig` + `GRX_TRUSTED_PROXIES`. Leftmost IP only; RFC 7239 `Forwarded:` deferred. |
| B Part 2 | Auth-failure rate coupling with #1 | Bucket separately on `auth_fail` events so an attacker can't time-attack-distinguish 401 from 429. |
| B Part 2 | RFC 7239 `Forwarded:` header parsing | Operators using LBs that emit RFC 7239-style `Forwarded:` get correct discrimination. Phase B Part 1 only parses `X-Forwarded-For`. |
| B Part 2 | `tracing` feature audit + opt-in | Audit whether `tower_governor`'s own tracing events overlap with `graphreact::observer::audit` cleanly; opt-in if so. |
| C | Distributed / Redis-class shared store | Multi-replica daemons share rate-limit state via Redis or a similar TTL-backed store. Today's in-memory state is per-replica. |
| C | Per-account / per-organisation quotas | Carve the bucket by an `Organisation` header the RBAC track introduces. |
| D | Adaptive caps from `MetricsInner` feedback | Auto-tune per-IP cap based on observed scheduler load — feedback loop between #5 and #6. |
