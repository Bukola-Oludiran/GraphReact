# M-Prod — Lifecycle (sub-track #4)

**Status:** SHIPPED Phase α 2026-06-17 — SIGTERM drain primitive landed (drain bit + `/v1/readyz` extension + ingress 503 `draining` + `DaemonHandle::start_drain`); rolling-version negotiation (Phase β) + M14.11 delegated-restart (Phase γ) deferred; Phase β SHIPPED 2026-06-18 — WIRE_VERSION bumped to 5 + MIN_SUPPORTED_VERSION at 4 (one-bump rolling-upgrade compat) + per-engine capability bitmap.

This document covers sub-track #4 of the M-Prod milestone (production-hardening): orderly daemon lifecycle — graceful drain on SIGTERM, rolling-version handshake compatibility, and delegated restart for the M14.11 hot-reload escalation path. Sibling tracks (#2 TLS / SIGHUP topology, #5 observability `/v1/readyz`, #1 audit-sink for drain events, #9 chaos arms) reference this substrate; see [Cross-track contracts](#cross-track-contracts).

## Phase α scope

Phase α ships the **SIGTERM drain primitive** — every future drain trigger (kubelet SIGTERM today, REST `POST /v1/admin/drain` later, operator `grx admin drain --deadline=Ns`, M14.11 reload-driven drain) calls into a single load-bearing entry point so the observer surface ALWAYS de-routes ingress the same way: load balancer sees `/v1/readyz` go 503 with body `draining`, new ingress POSTs see the same 503 `draining` body, and the audit log carries one `drain_started` / `drain_completed` / `drain_deadline_exceeded` row per drain.

| Deliverable | Status | Location |
|---|---|---|
| `DrainFlag` (`Arc<AtomicBool>`) atomic-flip substrate | SHIPPED | [`crates/graphreact-observer/src/drain.rs`](../../crates/graphreact-observer/src/drain.rs) |
| `IngressInFlight` (`Arc<AtomicUsize>`) + RAII `InflightGuard` | SHIPPED | `drain.rs` |
| `ApiState.draining: Option<DrainFlag>` + `.ingress_in_flight` | SHIPPED | [`crates/graphreact-observer/src/management_api.rs`](../../crates/graphreact-observer/src/management_api.rs) |
| `spawn_management_api_with_drain` variant | SHIPPED | `management_api.rs` |
| `/v1/readyz` first-branch `draining` predicate | SHIPPED | `management_api.rs::readyz` |
| Ingress 503 `draining` on `inject_message_handler` / `inject_call_handler` / `bulk_inject_handler` | SHIPPED | `management_api.rs` |
| Audit events (`drain_started` / `drain_completed` / `drain_deadline_exceeded` / `drain_budget_misconfig`) | SHIPPED | [`crates/graphreact-observer/src/audit.rs`](../../crates/graphreact-observer/src/audit.rs) |
| `DaemonHandle::start_drain(deadline) -> DrainOutcome` | SHIPPED | [`crates/graphreact-cli/src/commands.rs`](../../crates/graphreact-cli/src/commands.rs) |
| `LoopExit::{Duration,Sigint,Sigterm}` discriminator | SHIPPED | `commands.rs` |
| SIGTERM signal stream (unix) inside `drive_handle_until` | SHIPPED | `commands.rs` |
| `--drain-deadline-secs` / `GRX_DRAIN_DEADLINE_SECS` / `DEFAULT_DRAIN_DEADLINE_SECS=20` | SHIPPED | `cli.rs` + `commands.rs` |
| Integration tests (5 observer + 3 CLI + 4 unit) | SHIPPED | [`crates/graphreact-observer/tests/drain.rs`](../../crates/graphreact-observer/tests/drain.rs), [`crates/graphreact-cli/tests/drain_orchestration.rs`](../../crates/graphreact-cli/tests/drain_orchestration.rs) |
| Rolling-version handshake (`WIRE_VERSION` floor relax) | SHIPPED — Phase β 2026-06-18 | [`crates/graphreact-dist/src/handshake.rs`](../../crates/graphreact-dist/src/handshake.rs) + `wire/packet.rs` |
| M14.11 delegated-restart (reload-driven drain → spawn) | DEFERRED → Phase γ | n/a |
| Cluster handshake quiescence (`Cluster::begin_drain`) | DEFERRED → Phase β | n/a |
| SIGTERM on Windows (named-event or REST endpoint) | DEFERRED → Phase β | n/a |
| Per-connection 5-min `MAX_CALL_TIMEOUT_MS` interplay with drain | KNOWN GAP (see [Drain-vs-MAX_CALL_TIMEOUT](#drain-vs-max_call_timeout-interaction)) | n/a |

## DrainFlag shape

```rust
pub struct DrainFlag {
    inner: Arc<AtomicBool>,
}

impl DrainFlag {
    pub fn new() -> Self { /* ... */ }
    pub fn start(&self, deadline_ms: u64) -> bool { /* CAS-gated */ }
    pub fn is_draining(&self) -> bool { /* relaxed load */ }
}
```

- `Arc<AtomicBool>` so every clone observes the same flip. The same handle is held by `ApiState` (so the management-API `/v1/readyz` hot path reads it), by `ApiServerHandle` (so `DaemonHandle` can re-clone), and by `DaemonHandle::drain` (so the SIGTERM orchestrator flips it).
- `start()` is **CAS-gated** — only the first call returns `true` and emits the `drain_started` audit row. Duplicate SIGTERMs from the kubelet (or operator-driven double-fire) return `false`, so the audit log carries exactly one `drain_started` line per daemon lifetime and the deadline is never reset.
- `is_draining()` is a **relaxed** load. The hot path (every `/v1/readyz` scrape + every ingress POST) reads this once per request; relaxed ordering is correct because we only care about *eventual* visibility. A request observing the OLD value during the brief flip window completes normally; the next request after the flip sees `true`.
- **There is no `clear()`** — drain is a one-way trip; process exit clears the flag implicitly by tearing down the heap. A daemon that started draining MUST shut down (or be terminated by SIGKILL); operators cannot "cancel" a drain.

**`drain_started` MUST emit BEFORE the AtomicBool flip becomes observable** — otherwise a `/v1/readyz` scrape racing the flip could see `ready=true` AND `no drain_started` in the log. `DrainFlag::start` enforces this ordering by emitting on the success branch of `compare_exchange` BEFORE the function returns, and `tracing::info!` is synchronous w.r.t. the calling thread, so the audit row happens-before the CAS return as observed by any other thread.

## IngressInFlight + RAII guard

```rust
pub struct IngressInFlight {
    inner: Arc<AtomicUsize>,
}

impl IngressInFlight {
    pub fn enter(&self) -> InflightGuard { /* fetch_add 1; Drop fetch_sub 1 */ }
    pub fn load(&self) -> usize { /* relaxed load */ }
}
```

- Lock-free counter bumped on entry into the three ingress handlers (`inject_message_handler` / `inject_call_handler` / `bulk_inject_handler`). The RAII `InflightGuard` decrements on `Drop` so axum future cancellation (HTTP-client disconnect, drain-deadline-driven abort) keeps the count balanced.
- The `bulk_inject_handler` moves the guard into the spawned producer task — the count only drops when the producer has drained EVERY NDJSON line of the request.
- **No hard cap** — existing per-endpoint caps (`PendingCalls::HARD_CAP = 10_000`, `SNAPSHOT_CHANNEL_CAPACITY = 256`) bound the upstream resource consumption.

## Drain-bit check ordering

The drain check fires **BEFORE** the in-flight bump in every ingress handler. This is load-bearing — if the order were reversed, a request that arrived after `start_drain` flipped the bit could bump the counter, race the orchestrator's `ingress_in_flight.load() == 0` poll, and the orchestrator would either undercount the drain population or block indefinitely.

```rust
async fn inject_message_handler(...) -> Response {
    // 1. Check drain BEFORE bumping the counter.
    if state.draining.as_ref().is_some_and(|f| f.is_draining()) {
        return drain_in_progress_response();    // 503 "draining"
    }
    // 2. Now safe to bump — drain wasn't on at entry.
    let _inflight = state.ingress_in_flight.enter();
    // ...
}
```

## `/v1/readyz` predicate (extended)

Phase α adds a new **first-branch** predicate evaluated BEFORE the existing `batches > 0 && bridge_capacity > 0 && cluster_ready` trio:

| Term | Source | Why |
|---|---|---|
| Drain in progress (Phase α) | `state.draining.as_ref().is_some_and(|f| f.is_draining())` | When drain is set, return 503 `draining` ahead of all other not-ready reasons. k8s should never see `scheduler_first_batch_pending` mid-drain. |
| Scheduler has completed ≥1 `process_batch` round | `MetricsInner::process_batch_duration.snapshot().count > 0` | (unchanged from #5) |
| Snapshot bridge has capacity | `state.snapshot_tx.capacity() > 0` | (unchanged from #5) |
| Cluster membership reached (#5 residual) | `cluster_readyz()` probe | (unchanged from #5) |

**Response shape** (new row + existing rows for completeness):

| State | Status | Body | Headers |
|---|---|---|---|
| Ready | 200 OK | `ready` | `Content-Type: text/plain; charset=utf-8` |
| **Draining (Phase α)** | **503 Service Unavailable** | **`draining`** | **`Content-Type: text/plain; charset=utf-8`, `Retry-After: 1`** |
| Pre-tick | 503 Service Unavailable | `scheduler_first_batch_pending` | `Content-Type: text/plain; charset=utf-8`, `Retry-After: 1` |
| Bridge saturated | 503 Service Unavailable | `bridge_saturated` | `Content-Type: text/plain; charset=utf-8`, `Retry-After: 1` |
| Cluster peers configured but none dialed | 503 Service Unavailable | `cluster_no_peers_connected` | `Content-Type: text/plain; charset=utf-8`, `Retry-After: 1` |

## Ingress 503 `draining` contract

Single-shot POSTs (`/v1/nodes/:id/messages`, `/v1/nodes/:id/call`) and bulk POST (`/v1/nodes/:id/messages/bulk`) all return:

```
HTTP/1.1 503 Service Unavailable
Content-Type: application/json
Retry-After: 1

{"error":"draining","retry_after_secs":1}
```

The body string `draining` is symmetric with the `/v1/readyz` reason — every surface that surfaces a drain-driven 503 uses the word `draining`, so operators can grep the LB / proxy log stream uniformly. The 503 shape mirrors `bridge_saturated_response` (status, headers, JSON body shape) — clients with existing 503 + `Retry-After` handling automatically back off correctly.

## DaemonHandle::start_drain

```rust
pub async fn start_drain(&self, deadline: Duration) -> DrainOutcome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainOutcome {
    pub in_flight_settled: bool,
    pub cluster_quiesced: bool,
    pub drain_duration: Duration,
    pub deadline_exceeded: bool,
}
```

Behavior:

1. Idempotent CAS gate on `DrainFlag::start` — duplicate calls return immediately with the no-op outcome (`in_flight_settled = true, deadline_exceeded = false, drain_duration = ZERO`).
2. Audit `drain_started` BEFORE the AtomicBool flip becomes observable.
3. Poll `ingress_in_flight.load() == 0 && pending_calls.is_empty()` on a 25ms cadence until quiescence OR `deadline` elapses.
4. Audit `drain_completed` (success) or `drain_deadline_exceeded` (timeout) with the remaining counts so operators can correlate against their LB's in-flight metric.
5. Return `DrainOutcome`. Does NOT call `shutdown()` — the daemon's runtime stays up so the observer can keep replying 503 `draining` to LB probes while the orchestrator decides when to escalate.

When `self.drain` is `None` (observer wasn't wired at boot) this is a no-op — there's no ingress surface to quiesce.

## SIGTERM → drain → shutdown wiring

`drive_handle_until` now returns a `LoopExit` discriminator:

```rust
pub enum LoopExit { Duration, Sigint, Sigterm }
```

- **`LoopExit::Sigterm`** (kubelet / production trigger): `run_daemon_blocking` runs `handle.start_drain(deadline).await` then `handle.shutdown().await`. The drain phase de-routes the LB before scheduler / cluster teardown begins.
- **`LoopExit::Sigint`** (developer Ctrl-C): straight to `shutdown()`. No drain phase — local-dev exits should be snappy.
- **`LoopExit::Duration`** (test path, `--duration-secs`): straight to `shutdown()`. Tests already gate on duration; drain would add latency without buying anything.

SIGTERM is unix-only — the signal stream registration is `cfg(unix)`-gated. On Windows the SIGTERM arm is omitted at compile time and only `ctrl_c` fires; the daemon falls back to snap-shutdown semantics. Phase β picks up a portable shape (named-event handle or a small REST endpoint).

## Budget math vs k8s `terminationGracePeriodSeconds`

The k8s default `terminationGracePeriodSeconds` is **30 seconds**. The budgets compose:

```
drain_deadline_secs + OBSERVER_SHUTDOWN_DEADLINE + slack ≤ terminationGracePeriodSeconds
       20s         +            5s                + 5s  =       30s ✓
```

Defaults:

- `DEFAULT_DRAIN_DEADLINE_SECS = 20` (this Phase α)
- `DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS = 5` (sub-track #5)
- ~5s slack covers cluster shutdown + engine teardown
- = **30s exactly**, fits the k8s default

**Locked cross-track invariant** (from `m-prod-observability.md:113`): the OTel teardown bound MUST run INSIDE the drain budget, i.e. `drain_deadline ≥ OBSERVER_SHUTDOWN_DEADLINE`. `start_daemon` validates this at boot and emits a `drain_budget_misconfig` warn audit row when the invariant is violated; the daemon still boots (we don't reject misconfigurations) but operators see the misconfig in the audit stream.

Operators raising either bound MUST raise `terminationGracePeriodSeconds` accordingly:

| Configuration | k8s `terminationGracePeriodSeconds` |
|---|---|
| Defaults (20 + 5 + 5) | ≥ 30s |
| Long-running queries (60 + 5 + 5) | ≥ 70s |
| OTel collector flaky (20 + 30 + 5) | ≥ 55s |

## CLI flag matrix

| Flag | Env var | Default | Used by | Notes |
|---|---|---|---|---|
| `--drain-deadline-secs <N>` | `GRX_DRAIN_DEADLINE_SECS=<N>` | 20s (`DEFAULT_DRAIN_DEADLINE_SECS`) | `grx cluster daemon` | Outer SIGTERM drain budget. Explicit flag wins over env var. Empty / unparseable / zero falls back to default. |
| (existing) | `OBSERVER_SHUTDOWN_DEADLINE=<N>` | 5s (`DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS`) | `DaemonHandle::shutdown` | OTel teardown bound. Runs INSIDE the drain budget. |

`grx cluster connect` does NOT yet expose `--drain-deadline-secs` — `connect` is a one-shot dial with no ingress surface, so drain is a no-op. Phase β picks it up if/when `connect` grows an observer bind.

## Audit-event shape

All four lifecycle events ride the same `graphreact::observer::audit` target as the existing `observer listening` / `observer scrape` / `tls_reloaded` / `otel_shutdown_timeout` rows. JSON logs in production (via `install_json_logs_subscriber` + `tracing_layer::TracingJsonLayer`) emit each event as a single line.

| Event | Level | Fields | Emitted by |
|---|---|---|---|
| `drain_started` | INFO | `deadline_ms: u64` | `DrainFlag::start` (only the first CAS-winner; duplicate SIGTERMs are no-ops) |
| `drain_completed` | INFO | `duration_ms: u64`, `in_flight_settled: bool`, `cluster_quiesced: bool` | `DaemonHandle::start_drain` on quiescence within deadline |
| `drain_deadline_exceeded` | WARN | `deadline_ms: u64`, `duration_ms: u64`, `pending_calls_remaining: usize`, `ingress_in_flight_remaining: usize` | `DaemonHandle::start_drain` on deadline expiry |
| `drain_budget_misconfig` | WARN | `drain_deadline_ms: u64`, `observer_shutdown_deadline_ms: u64` | `start_daemon` boot when `drain_deadline < OBSERVER_SHUTDOWN_DEADLINE` |

**Operator dashboard contract.** Counting `event=drain_started` per process gives a daemon-restart rate; pairing with `event=drain_completed` vs `event=drain_deadline_exceeded` over a window surfaces the fraction of drains that hit the deadline (alert when > 5% over a 1h window — indicates either misconfigured `terminationGracePeriodSeconds` or hung handlers). `drain_budget_misconfig` should ALWAYS be 0 in production — alert on > 0.

## Drain-vs-`MAX_CALL_TIMEOUT` interaction

`inject_call_handler` caps its per-call timeout at `MAX_CALL_TIMEOUT_MS = 300_000` (5 minutes). A call that hangs the full 5 minutes will outlive any reasonable drain deadline.

**Phase α behavior:** when `start_drain` hits its deadline with such a call still in flight, the orchestrator emits `drain_deadline_exceeded` with the remaining counts and the caller (`run_daemon_blocking`) proceeds to `shutdown()`. `ApiServerHandle::shutdown` notifies the accept loop, which kills in-flight axum tasks — the hung call's HTTP client sees connection-reset. `PendingCalls` guards' `Drop` evicts cleanly. k8s SIGKILL is the next escalation if `terminationGracePeriodSeconds` runs out.

This is acceptable for Phase α but not ideal — a future Phase β could bound the per-call remaining-time at drain-start (truncate `MAX_CALL_TIMEOUT_MS` down to `drain_deadline_remaining`).

## Cross-track contracts

| Sub-track | What it reuses | Phase α obligation |
|---|---|---|
| **#1 RBAC / hash-chained audit log** | All four lifecycle events use the existing `graphreact::observer::audit` target. When #1 lands its append-only `AuditSink`, the rows flow through automatically — no callsite change. | Phase α audit-event shapes are stable. |
| **#2 TLS / SIGHUP topology** | Phase α adds a SIGTERM signal stream; Phase B of #2 will add a SIGHUP signal stream for cert rotation. The signal-handler topology is one `tokio::signal::unix::signal` stream per signal kind, each select-driven independently inside `drive_handle_until`. | Phase α touched only SIGTERM; the `drive_handle_until` arms are documented as "one arm per signal kind" so the pattern composes. |
| **#5 observability (`/v1/readyz`)** | Phase α extends the readyz predicate with the drain first-branch, completing the deferred row called out at [`m-prod-observability.md:108`](m-prod-observability.md). The drain bound (`drain_deadline_secs`) composes with `OBSERVER_SHUTDOWN_DEADLINE` per the locked contract at [`m-prod-observability.md:113`](m-prod-observability.md). | Phase α extends `m-prod-observability.md`'s readyz predicate table + reason-body row; see [observability doc update](#docsbackendm-prod-observabilitymd-update). |
| **#8 packaging / helm chart** | The helm chart's `terminationGracePeriodSeconds` MUST be ≥ `drain_deadline_secs + OBSERVER_SHUTDOWN_DEADLINE + 5s slack`. Default 30s fits the default config (20 + 5 + 5). | Phase α documents the math; #8 bakes the value into the chart. |
| **#9 chaos / fuzz** | A chaos arm fires SIGTERM repeatedly while the chaos harness's HTTP client hits `/v1/healthz` + `/v1/readyz` + an ingress POST. Asserts: readyz flips 200→503 ≤25ms after SIGTERM; in-flight POSTs that arrived before SIGTERM complete; new POSTs after SIGTERM see 503 `draining`. | Phase α's CAS-gated `DrainFlag::start` is the contract — duplicate SIGTERMs are no-ops. #9 verifies under stress. |

### `docs/backend/m-prod-observability.md` update

Phase α extends the `/v1/readyz` predicate row + adds a 503 reason row for `draining`. See the predicate table above for the new row shape; the doc update appends to the existing `m-prod-observability.md` predicate table without rewriting it.

## What Phase α does NOT ship

| Item | Reason | Lands in |
|---|---|---|
| Rolling-version handshake (`WIRE_VERSION = N`, `MIN_SUPPORTED_VERSION = N-1`) | The wire-protocol crate (`graphreact-dist`) is explicitly out of scope for Phase α per cross-track contract. ~~Phase β picks up the `WIRE_VERSION` floor relax + per-engine capability negotiation.~~ → SHIPPED Phase β 2026-06-18; see [Shipped — Phase β: rolling-version handshake](#shipped--phase-β-rolling-version-handshake). | Phase β |
| M14.11 delegated-restart (reload-driven drain → spawn new process → handoff connections) | Requires both the drain primitive (this Phase α) AND the hot-reload pipeline (M15) AND the rolling-version handshake (Phase β). Phase γ composes all three. | Phase γ |
| Cluster handshake quiescence (`Cluster::begin_drain` softer than `Cluster::shutdown`) | Today `Cluster::shutdown` aborts every per-peer task immediately. Phase α accepts this — handshakes that don't complete in the drain window get re-tried by the dialing peer on next reconnect. `cluster_quiesced` in `DrainOutcome` is reported `true` unconditionally for Phase α. | Phase β |
| SIGHUP signal handler | Cross-platform signal handling for cert rotation is sub-track #2 Phase B's domain. Phase α's `drive_handle_until` topology (one `tokio::signal::unix::signal` arm per signal kind) is the contract Phase B extends. | #2 Phase B |
| SIGTERM on Windows | Phase α is unix-tokio only (`cfg(unix)` gate). Phase β picks up a portable shape (named-event handle, or a small REST `POST /v1/admin/drain`). | Phase β |
| Per-call drain-aware timeout truncation | `MAX_CALL_TIMEOUT_MS = 300_000` (5 min) can outlive any reasonable drain deadline; Phase α surfaces the count via `drain_deadline_exceeded` + lets SIGKILL handle truncation. | Phase β |
| `grx cluster connect` SIGTERM wiring | `connect` is a one-shot dial without an ingress surface; drain would be a no-op. | Phase β if `connect` ever grows an observer bind. |
| `drain_budget_misconfig` boot rejection | Phase α emits a WARN audit row rather than rejecting — back-compat with existing operator configs is preferred over hard failure. | Operators escalate to a hard reject via #8 helm chart Pod validation. |

## Shipped — Phase β: rolling-version handshake

Phase β SHIPPED 2026-06-18 (sub-track #4 Group B-2). Lands the
WIRE_VERSION floor relax + per-engine capability negotiation as a
safe-slice on top of Phase α's drain primitive.

**Scope:**

- `WIRE_VERSION` bumped 4 → **5**
  ([`crates/graphreact-dist/src/wire/packet.rs`](../../crates/graphreact-dist/src/wire/packet.rs)).
- `MIN_SUPPORTED_VERSION` stays at **4** —
  `MIN_SUPPORTED_VERSION + 1 == WIRE_VERSION` is the new one-bump
  compat-window invariant (asserted in
  `wire/frame.rs::tests::min_supported_version_is_one_below_wire_version`).
  Future bumps either move both constants together (no compat window)
  OR move WIRE_VERSION only (widen the window by one).
- `Hello.capabilities_bitmap: u64` +
  `ChallengeAck.capabilities_bitmap: u64` added as a parallel typed
  capability surface alongside the existing `Vec<String>` per-feature
  flags. String-vec stays for per-feature opt-in
  (`"reload-v1"`, `"observer-otel-v1"`, `"ingress-call-v1"`); the
  bitmap is for substrate-level engine capabilities.
- `CAP_MIXED_RUNTIME = 1 << 0`,
  `CAP_DELEGATED_RESTART = 1 << 1`,
  `CAP_AUDIT_CHAIN = 1 << 2` constants reserved
  (re-exported from `graphreact_dist::wire`). M22, Phase γ, and #1
  Phase B respectively are the planned consumers.
- Effective bitmap = `our_bitmap & peer_bitmap`, computed at
  handshake completion and queryable via
  `Cluster::peer_effective_capabilities(name) -> Option<u64>`.
  Phase β only EXPOSES the value — no dispatch path consumes it yet
  (Phase γ ships the first consumer via M14.11 delegated-restart).
- Audit events on the
  `graphreact::observer::audit` target:
  - `cluster_handshake_complete` (INFO) — fields `peer_id`,
    `peer_wire_version`, `local_wire_version`,
    `negotiated_capabilities`. Server-side `peer_wire_version` is
    authoritative (from the peer's Hello); client-side
    `peer_wire_version` is best-effort and equal to our local
    WIRE_VERSION (the Challenge packet doesn't carry the peer's
    version — Phase γ may extend this if precise tracking becomes
    load-bearing).
  - `cluster_handshake_rejected` (WARN) — fields `peer_wire_version`,
    `local_wire_version`,
    `reason ∈ {wire_version_below_floor, wire_version_above_ceiling}`.
- v4 peers (Hello/ChallengeAck without the new `capabilities_bitmap`
  field on the wire) decode via the **dual-struct fallback** in
  [`crate::wire::WireCodec::decode`] — v5 shape tried first; on
  `postcard::Error::DeserializeUnexpectedEnd` the payload is
  re-decoded as `HelloV4` / `ChallengeAckV4` and the missing field
  synthesises to `0` (no substrate-bitmap capabilities advertised).
  Any other postcard error propagates — genuine wire corruption is
  NOT papered over as "v4 peer with zero bitmap". Compat is
  **one-directional**: v5 daemons accept v4 Hellos; v4 daemons see
  v5 Hellos as malformed (trailing u64 they don't recognise). The
  policy implication: operators bump every daemon ONE step (v4→v5)
  before any operator-driven feature (Phase γ +) starts demanding
  `capabilities_bitmap != 0`.
- Above-ceiling rejection: peers with `version > WIRE_VERSION`
  (future peers we don't understand) are rejected at the server-side
  handshake BEFORE the floor check; reuses the existing
  `DistError::UnsupportedVersion` variant with the `min` field
  carrying `MIN_SUPPORTED_VERSION` (dual-use; the operator-facing
  message reads slightly off for above-ceiling rejections — a
  future variant split is acceptable if operator clarity demands
  it).

**Planned consumers** (Phase γ +):

- **#4 Phase γ (M14.11 cross-node delegated-restart)** — gates the
  outbound `delegated_restart` packet on
  `peer_effective_capabilities & CAP_DELEGATED_RESTART != 0`.
- **#1 Phase B (multi-token audit-chain)** — gates the
  hash-chained audit-event payload on
  `peer_effective_capabilities & CAP_AUDIT_CHAIN != 0`.

**Cross-track contract integrity** (Phase β):

- Audit-target convention (`graphreact::observer::audit`) preserved —
  both new events ride the same target.
- ApiState additive-only contract — unaffected; the handshake doesn't
  touch ApiState.
- Atomic-swap pattern — unaffected.
- Drain-budget math — unaffected.
- Back-pressure response envelope + precedence ladder — unaffected
  (handshake is the dist layer, not the ingress layer).
- mode 0o600 — unaffected.

## References

- M-Prod master plan: see `m-prod row in docs/backend/README.md`.
- Sub-track #5 cross-track contract: [`m-prod-observability.md:113`](m-prod-observability.md) (OTel teardown ≤ drain bound; drain ≥ OTel teardown).
- Audit-target convention: same `graphreact::observer::audit` target used by every other observer audit event; see [`m-prod-observability.md`](m-prod-observability.md) for the JSON log stream contract.
- Sister doc: [`m-prod-tls-rotation.md`](m-prod-tls-rotation.md) — TLS rotation substrate Phase A; the SIGHUP trigger surface for cert reload lands as #2 Phase B and composes with this Phase α SIGTERM stream.
