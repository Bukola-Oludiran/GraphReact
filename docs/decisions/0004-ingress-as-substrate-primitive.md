# 0004 — M-Ingress is its own milestone with substrate-not-MVP scope; correlation_id rides on `Message`, not via ephemeral reply nodes

**Status:** DECIDED — recorded 2026-05-09. M-Ingress (Phases 1 + 2 + 3) SHIPPED 2026-05-09.
**Related:** Decision 0002 (backend-only architecture, [0002-graphreact-is-backend-only.md](0002-graphreact-is-backend-only.md)) and decision 0003 (bifurcation, [0003-graphreact-bifurcates-into-backend-and-db.md](0003-graphreact-bifurcates-into-backend-and-db.md)) both stay valid; this decision sits on top. Cross-references: [m16.5-ingress.md](../backend/m16.5-ingress.md) (milestone narrative), [m16.5-ingress-protocol.md](../backend/m16.5-ingress-protocol.md) (wire spec).

## The decisions (three)

1. **M-Ingress is its own milestone**, NOT a sub-phase of M-Prod or a fold-in to M16-Backend. Sequenced AFTER M16-Backend (which shipped observability) and BEFORE M-Prod (which hardens the existing surfaces). The complete substrate primitive set ships in one milestone: single-shot inject + request/response call + bulk NDJSON. Phases inside the milestone are implementation sequencing only.

2. **Default-deny manifest gating** for ingress. A node accepts external HTTP messages ONLY for types listed in its `accepts_external` (single-shot/bulk) OR matching pairs in its `accepts_external_call` (call shape). Pre-M-Ingress, no node accepted external messages because the runtime had no path to inject them. Post-M-Ingress, existing applications gain zero attack surface from the upgrade.

3. **`correlation_id: Option<String>` on `graphreact_core::Message`** for HTTP-call request/response, NOT ephemeral synthetic reply nodes spawned per call. The cluster wire-format `WIRE_VERSION 3 → 4` bump + `INGRESS_CALL_V1_CAPABILITY` capability flag mirror M16-Backend Phase 10's `trace_context` propagation pattern exactly.

## Why M-Ingress is its own milestone

The first instinct is to fold "external HTTP injection" into M-Prod alongside auth-tightening, rate limits, audit logs, idempotency, and TLS rotation. That conflates two different tracks:

- **M-Prod** is a *hardening* milestone. It refines existing capabilities so a v1 production deploy can survive contact with internet-shaped traffic. Auth, rate limits, audit, TLS rotation — all sharpening surfaces that already exist post-M16-Backend.
- **M-Ingress** is a *new capability* milestone. Pre-M-Ingress, the runtime has no path for an external HTTP client to push a message into an actor node's inbox. Every backend application has to BYO HTTP listener and inject-message glue — exactly the asymmetry the [substrate-not-MVP rule](../../../projects/-Users-damioludiran-jays-workspace/memory/feedback_complete_substrate_not_mvp.md) warns against.

Folding new capability into a hardening milestone re-litigates M-Prod's scope and delays both. Splitting M-Ingress out keeps M-Prod focused on hardening and lets M-Ingress ship the complete primitive set in one self-contained arc.

**Also distinct from M16-DB**: M16-DB ships *typed reducers against domain state* — a database-style RPC shape. M-Ingress ships *untyped (or app-typed) message injection into actor inboxes* — an actor-system primitive. They live at different abstraction levels and serve different audiences. Decision 0003's bifurcation is the load-bearing reason these don't collapse into one feature.

## Why substrate-not-MVP — all three patterns ship together

A "phase 1 MVP that covers single-shot, defer call + bulk" was a real option on the table. Rejecting it: the [substrate-not-MVP rule](../../../projects/-Users-damioludiran-jays-workspace/memory/feedback_complete_substrate_not_mvp.md) says ship the complete primitive set the application space needs, not a phase-1 MVP that biases toward the named near-term projects.

Application classes the unified substrate must serve (per [project_graphreact_application_space.md](../../../projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_application_space.md)):

- **Thymus** + **ComplyFlow** + **NewsCaster** need single-shot inject (Phase 1).
- **LARQL** + **adaptive education** + **biological pathway sim** need request/response (Phase 2).
- **NewsCaster firehose** + **ComplyFlow audit logs** + **reputation systems** need bulk (Phase 3).

A phase-1-only ship would have left half the application space "speculative until phase 2" — exactly the asymmetry the rule warns against. Phases 1 + 2 + 3 are implementation sequencing within the milestone (each builds on the prior phase's bridge + manifest infrastructure), but they ship together within one milestone arc.

## Why default-deny manifest gating

Two viable choices:
- **(A)** Accept-by-default: any node receives any external HTTP message; rejection is operator policy at the reverse proxy.
- **(B)** Default-deny: a node accepts external messages only for types listed in its manifest's `accepts_external` / `accepts_external_call`.

**Choosing (B) — default-deny.**

- **Zero attack-surface upgrade**: pre-M-Ingress applications mounted via existing `.grx` files have no `accepts_external` declarations. Post-M-Ingress, they continue to refuse every external type because the manifest lists are empty. Operators upgrade the runtime without auditing every component for inadvertent exposure.
- **Component-author opt-in matches the trust model**: the component author knows which message types are safe to accept from external clients. Gating at the manifest puts that decision next to the handler code where it's reviewable.
- **Distinct from `accepts`** so the runtime can distinguish trusted node-to-node delivery from external HTTP injection. A handler receiving a message via internal `useEmit` chain shouldn't accidentally also be reachable via HTTP just because the type appears in `accepts`.
- **Substrate-defensible without RBAC**: M-Prod adds multi-token RBAC + per-node ACLs. Default-deny gives M-Ingress a defensible posture without that work — operators can safely deploy ingress while M-Prod is in flight.

Phase 2 introduces a parallel `accepts_external_call: Vec<ExternalCall { request_type, reply_type }>` for the call shape because the call's reply-routing requires the manifest to declare BOTH halves of the pair. Same default-deny semantics; same reasoning.

## Why `correlation_id` on `Message`, NOT ephemeral reply nodes

Two viable correlation models for `POST /v1/nodes/:id/call`:

- **(A)** Add `correlation_id: Option<String>` to `Message` + scheduler auto-tags the first matching outgoing emit with the inbound's correlation_id; HTTP layer awaits a `oneshot::Sender<Value>` registered in `PendingCalls` keyed by correlation_id.
- **(B)** Spawn an **ephemeral synthetic reply node** per call — `__reply_<uuid>__` mounted at call entry, listening for `useEmit('reply', ...)` from the target handler, forwarding to the HTTP layer, then unmounted at call exit.

**Choosing (A) — `correlation_id` on Message.**

| Dimension | (A) correlation_id | (B) ephemeral nodes |
|---|---|---|
| Per-call mount/unmount cost | **Zero** | Two scheduler ops per call (mount + unmount), plus `NodeMounted`/`NodeUnmounted` telemetry noise |
| Cross-cluster routing | Wire format propagates the field; receiver-side resolution natural | Synthetic node names must be globally unique; cluster-wide cleanup on partition is fragile |
| Telemetry pollution | One field on `MessageDelivered`; opt-in `on_call_reply` event | Mount/unmount events on every call; `accepts: ['reply_type']` ad-hoc per synthetic |
| Wire-format cost | One bump (3 → 4) + one cap-flag-gated `Option<String>` field | Zero (in-process only) |
| Auto-propagation logic | New `current_call_context` slot in scheduler; `make_emit` checks reply_type match | None (each synthetic node is its own routing target) |
| Component-author ergonomics | `useEmit('reply', payload)` as normal; runtime does the wiring | Same |

(A) wins on per-call cost + telemetry cleanliness + cross-cluster shape. The wire-format bump cost is real (one-time ship; hard floor v3 ↔ v4) but the precedent is well-trodden — M16-Backend Phase 10 already did the exact same shape (bumped 2 → 3 + added `OBSERVER_OTEL_V1` capability flag for graceful in-version mixed deployments) for `trace_context` propagation. M-Ingress P2 follows that precedent verbatim:

| Pattern | M16-Backend P10 | M-Ingress P2 |
|---|---|---|
| Wire bump | `WIRE_VERSION 2 → 3` | `WIRE_VERSION 3 → 4` |
| Min supported | `MIN_SUPPORTED_VERSION = WIRE_VERSION` (hard floor) | `MIN_SUPPORTED_VERSION = WIRE_VERSION` (hard floor) |
| New `Message` field | `trace_context: Option<[u8; 24]>` | `correlation_id: Option<String>` |
| Capability flag | `OBSERVER_OTEL_V1_CAPABILITY = "observer-otel-v1"` | `INGRESS_CALL_V1_CAPABILITY = "ingress-call-v1"` |
| Outbound gating | Sender strips when target peer lacks the capability | Sender strips when target peer lacks the capability |
| Auto-propagation site | `make_emit` reads thread-local OTel span | `make_emit` reads `current_call_context` |
| Sink consumer | `OtelSink` attaches `Link` to receiver-side span | `PendingCalls` resolves awaiting oneshot |

Rolling-version negotiation across the wire-version floor remains M-Prod scope (same deferral as P10). v3 + v4 daemon mixes are not interop-supported within M-Ingress; operators upgrade cluster-wide.

### Auto-propagation policy: first-match-only on outgoing emit

Per-handler-invocation, the runtime captures `(correlation_id, reply_type)` when an inbound message of that call shape arrives. When the handler emits, `make_emit` checks: outgoing `type == reply_type` AND no prior matching emit fired in this invocation → tag the outgoing message with the correlation_id AND fire `on_call_reply`. After the first match, the `tagged` flag flips so subsequent emits in the same invocation are NOT tagged even if their `type` matches.

A handler doing `emit("log", ...); emit("reply", ...); emit("metric", ...)` tags exactly the `"reply"`. This decouples reply identity from emit ordering and protects against handlers emitting multiple downstream messages from accidentally tagging the wrong one.

### `on_call_reply` fires at emit-time, not delivery-time

`PendingCalls` resolves awaiting oneshots from the `TelemetrySink::on_call_reply` event, which `make_emit` fires immediately when it auto-tags an outgoing message — BEFORE the reply Message enters the scheduler queue. This means leaf-node call handlers (no neighbors, no `direct: [...]` self-target) work without a reply destination. The reply Message also flows through the normal queue for any graph subscribers; the registry resolution is independent of actual graph delivery.

Without this decoupling, a leaf handler that emits to its (empty) neighbors would silently drop the reply, and the call would time out — bad ergonomics. The substrate-not-MVP commitment to the ingestion-shaped + leaf-shaped application space drove this design.

## What didn't ship in M-Ingress (deferred)

| Deferred item | Target |
|---|---|
| Multi-token RBAC, per-node ACLs, JWT claims | M-Prod #4-#6 |
| Per-IP / per-token rate limiting | M-Prod #6 (`tower_governor`) |
| Audit-log compliance refinement (formal audit-spec) | M-Prod (existing `target: "graphreact::observer::audit"` tracing convention extends to ingress events) |
| TLS cert rotation under SIGHUP | M-Prod #7 |
| Origin allowlists / credential modes for CORS | M-Prod (M-Ingress ships baseline `*` for loopback) |
| Timing-attack-safe response on auth failure | M-Prod |
| Streaming subscriptions for replies (vs request/response) | Already in M16-Backend P7 (SSE/WS via `/v1/observer/events`); M-Ingress's call shape is request/response, not streaming |
| File uploads via `multipart/form-data` | Outside scope (substrate need: send actor messages, not blob data) |
| gRPC transport for ingress | Wrong shape for browser consumers (per decision 0002, browsers are first-class consumers) |
| Typed reducers against domain state | M16-DB territory (per decision 0003) |
| `.grx` syntax extensions | Manifest stays component-owned; parser does not change |
| Rolling-version negotiation across `WIRE_VERSION` floor | M-Prod (same deferral as M16-Backend P10) |

## Reversibility

This decision is reversible at substantial cost:

- **Bumping back to ephemeral reply nodes**: would require deleting `correlation_id` from `Message` + `wire::Message`, a wire-format bump (4 → 5), cluster-wide upgrade, and rewriting `PendingCalls` to consume `NodeMounted`/`NodeUnmounted` events. Substrate cost is real; rolling back without a forcing function is unjustified.
- **Folding M-Ingress into M-Prod retroactively**: would require renumbering milestones in the plan file + memory aggregates. Doable but symbolic-only — the implementation is shipped.

**Forcing functions that would justify revisit:**
- A real, measured per-call overhead from the wire-format `Option<String>` field exceeding the noise band on cross-cluster benchmarks (M-Prod scope).
- A new application class (>10 simultaneous in-flight calls per millisecond) where `PendingCalls`'s 10k-entry cap becomes binding (operators can already tune via env var ahead of any code change).
- A user-with-budget asking for a non-actor-shaped ingress surface (typed reducers, domain-state RPC) — that's M16-DB territory, not M-Ingress.

## Implementation evidence

- Phase 1 (single-shot inject): `crates/graphreact-core/src/runtime/types.rs` (`accepts_external` field), `crates/graphreact-core/src/runtime/scheduler.rs` (`__ingress__` origin split + manifest gate at delivery), `crates/graphreact-observer/src/management_api.rs` (`InjectMessage` variant + `inject_message_handler` + `IdempotencyCache` + CORS layer), `crates/graphreact-observer/tests/inject_e2e.rs` (8 tests).
- Phase 2 (call): `Message::correlation_id` + `ExternalCall` + `accepts_external_call` (types.rs); `current_call_context` per-handler-invocation slot + auto-propagation (scheduler.rs); `WIRE_VERSION 3 → 4` + `INGRESS_CALL_V1_CAPABILITY` (graphreact-dist); `PendingCalls` registry + `inject_call_handler` + `on_call_reply` `TelemetrySink` method (graphreact-observer); `crates/graphreact-core/tests/runtime_call_propagation.rs` (3 tests) + `crates/graphreact-observer/tests/call_e2e.rs` (7 tests) + cluster.rs cap-flag tests (2) + capability string pin (1) + wire round-trip (1).
- Phase 3 (bulk): `BulkInjectResult` + `InjectMessageBulkLine` + `bulk_inject_handler` streaming NDJSON (management_api.rs); `crates/graphreact-observer/tests/bulk_e2e.rs` (10 tests).
- Phase 4 closeout (this doc): `case_h_external_inject` bench (within macOS noise band: -0.4% vs `case_a` baseline on the M-Ingress P4 acceptance machine), `crates/graphreact-cli/tests/ingress_e2e_subprocess.rs` E2E real-binary subprocess test (mounts a Python `.grx` with `accepts_external: ['ping']`, POSTs through `grx cluster daemon`'s observer bind, verifies via `/v1/nodes/:id/messages/recent` ring buffer), this decision doc, [m16.5-ingress-protocol.md](../backend/m16.5-ingress-protocol.md) wire-format spec, plan-file flip + memory aggregate.

**Total tests added across Phases 1-4: +43** (8 inject_e2e + 7 call_e2e + 3 core auto-propagation + 2 cluster cap-flag + 1 observability cap-string-pin + 1 wire round-trip for correlation_id + 10 bulk_e2e + 1 ingress_e2e_subprocess + cluster wire round-trip kept from P1 + the existing P1 wire round-trip integration). Workspace test count: ~966 (pre-M-Ingress) → ~1031 (default features) on the milestone-close machine.
