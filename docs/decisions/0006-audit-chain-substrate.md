# 0006 — M-Prod audit chain is a strict-additive SINK substrate behind the existing tracing convention (NOT a replacement); hash-chained NDJSON; fsync-per-append in Phase A; sink failures DEGRADE never PANIC; future RBAC events ride the same sink

**Status:** DECIDED — recorded 2026-06-17. M-Prod sub-track #1 Phase A SHIPPED 2026-06-17.
**Related:** ADR 0004 (M-Ingress as substrate primitive, [0004-ingress-as-substrate-primitive.md](0004-ingress-as-substrate-primitive.md)). This is an addendum that extends 0004's substrate-not-MVP discipline to the audit-log shape. Cross-references: [m-prod-auth.md](../backend/m-prod-auth.md) (milestone narrative), [m-prod-tls-rotation.md](../backend/m-prod-tls-rotation.md) (sibling sub-track that already lands `tls_reload_*` events on the same target).

## The decisions (five)

1. **`AuditSink` trait is fan-out, NOT replace**: the pre-existing `target="graphreact::observer::audit"` tracing stream KEEPS FIRING bit-for-bit unchanged. The sink Arc (when installed) gets a parallel structured `AuditRecordInput` push. Two surfaces; the tracing-side is the operator-facing JSON-log stream + journald target; the sink-side is the tamper-evident on-disk record.
2. **Hash-chained newline-delimited JSON (NDJSON)**, NOT CBOR / protobuf / postcard. The chain digest is SHA-256 over the canonical-JSON bytes of `{seq, ts_ms, prev_hash, kind, fields}` (the `this_hash` field itself is omitted from the hash input).
3. **fsync-per-append in Phase A**. Correctness over throughput for the substrate. Batched-fsync + worker-task fan-out are explicit Phase B work — the `auth_success` event multi-token RBAC adds runs on every authorized request, which moves the synchronous-per-append cost from "noise" to "load-bearing".
4. **Sink failure DEGRADES (counter + tracing::warn!) and NEVER PANICS**. Audit is meant to be observed, not load-bearing on liveness; an attacker who can fill the disk MUST NOT also be able to crash the daemon. The tracing-side stream keeps firing because the existing tracing line runs BEFORE the sink fan-out at every callsite.
5. **Future RBAC verdict events ride the same sink + same on-disk schema** (extending the open `fields` map). The `token_id` derived from a constant-time-comparable hash of the token bytes joins existing `peer` / `path` / `reason` keys without bumping the schema; an older `replay` tool that doesn't know the new keys still verifies the chain correctly.

## Why fan-out (not replace) the tracing-side stream

The first instinct is to deprecate the `target="graphreact::observer::audit"` tracing target the moment Phase A lands the hash-chained NDJSON file. That conflates two audiences:

- **Tracing-side** is consumed by `journalctl -u grx --grep 'graphreact::observer::audit'`, the unified JSON-log subscriber's stderr stream, `tracing-journald` shipping to a central syslog, OTel collectors that subscribe to `target=` matchers, and ad-hoc operator triage via `tail | jq`. None of these audiences gain anything from also reading the hash chain.
- **Sink-side** is consumed by SOC review pipelines, CI-style cron `grx audit verify` runs, compliance auditors, and incident-response forensics. None of these audiences want to grep a stderr stream for the records they care about.

A single-surface choice would pick one audience and ask the other to migrate. Two surfaces with disjoint guarantees serve both audiences without coupling them. The cost is one extra `if let Some(sink) = …` call per audit event — negligible on the audit hot path (max event rate at Phase A is ~10/s under credential-stuffing flood).

The substrate-not-MVP rule from ADR 0004 demands the substrate cover the application space without re-litigating it. Two surfaces is what the application space actually needs; Phase B's `auth_success` and RBAC verdict events don't add a third surface — they ride the same two.

## Why NDJSON, not CBOR

Three concrete operator workflows for the audit-log file:

- **A1**: `tail -f /var/log/grx/audit.ndjson | jq 'select(.kind == "auth_fail")'` for real-time tamper triage.
- **A2**: `grep -c 'auth_fail' /var/log/grx/audit.ndjson` for credential-stuffing rate estimates.
- **A3**: Python stdlib `hashlib.sha256` + `json.loads` to self-verify the chain on an air-gapped box (no `grx` binary required).

CBOR would require operators to install `cbor2`/`ciborium` / a custom decoder for each workflow. NDJSON is `cat`-friendly out of the box. The hash chain's canonical-bytes definition is "whatever `serde_json::to_vec` emits for `{seq, ts_ms, prev_hash, kind, fields}` minus the trailing newline" — stable across builds because serde's derived `Serialize` impl emits fields in struct-declaration order.

**Phase B can flip to CBOR behind an `audit_format=cbor` knob** if RBAC fan-out makes the JSON encode cost load-bearing (substrate doesn't preclude the migration; the `AuditSink` trait shape + the chain construction are format-agnostic), but Phase A's load profile doesn't justify the operator-tooling tax.

`serde_json` is already pinned at the workspace level (Cargo.toml:59); CBOR would add `ciborium` (NEW dep).

## Why fsync-per-append

The chain integrity guarantee depends on the bytes for `record[N].this_hash` being on disk BEFORE the in-memory `prev_hash` advances to that value. Without fsync, an OS-level crash between the write and the next append could:

- Lose `record[N]` entirely → `record[N+1].prev_hash` points to a hash whose source bytes don't exist anywhere → chain irrecoverable.
- Lose `record[N]`'s trailing bytes → `record[N]` parses as half a record → `replay` fails at the parse step with a chain whose tail can never be reconciled.

`sync_data` (not full `sync_all`) is the chosen sync: persists the bytes but skips the metadata sync. File-size + mtime updates don't matter for chain integrity, and skipping them shaves ~20µs per append on fast NVMe.

Phase B's batched-fsync mode trades the integrity guarantee for throughput on a knob-controlled basis. The Phase A default is "always fsync"; an operator who deliberately picks "periodic 250ms" accepts that a kernel-level crash could lose up to 250ms of audit records — they're betting that an audit-rotation policy that runs more often than the crash MTTF makes this safe.

## Why sink-failure degrades to a counter + warning, NOT a panic

Three options were considered:

- **(a) Panic the scheduler.** UNACCEPTABLE — audit is meant to be observed, not load-bearing on liveness; would worsen prod posture by making "fill the disk" a DoS vector. Defeats the whole point of running a hardened daemon.
- **(b) Degrade silently.** UNACCEPTABLE — an attacker who can fill the disk could silence audit, then exfiltrate, then nothing in the audit log shows the exfil. Defeats the tamper-evidence guarantee.
- **(c) Degrade-with-trace + counter.** CHOSEN. The sink emits a one-shot rate-limited `tracing::warn!(target: "graphreact::observer::audit", event = "audit_sink_write_failed", …)` on every I/O error — the tracing-side stream still flows, so JSON-log subscribers see the warning. Phase B adds an `audit_sink_write_failures_total` counter on `MetricsInner`; Phase C extends `/v1/readyz` to participate (sink-failing → NOT-READY) so the load balancer drains the daemon before the attacker's window opens.

The in-memory chain state continues advancing past the failed `seq` so the next successful write re-seats at the correct seq + prev_hash. Without this, a brief I/O failure followed by recovery would tear the chain.

## Why future RBAC events ride the same on-disk schema

Phase A's `AuditRecord.fields` is `serde_json::Value` — an open map. Phase B's multi-token RBAC adds new kinds (`auth_success`, `auth_denied_by_acl`) and new field keys (`token_id`, `route`, `verdict`). These ride the open `fields` map; the `AuditRecord` struct doesn't grow new top-level fields.

Two consequences:

1. **Phase B doesn't break Phase A chain verification.** The hash is computed over `{seq, ts_ms, prev_hash, kind, fields}` regardless of what `fields` contains. An older replay tool that doesn't know `auth_success` records still verifies the chain — it sees the new `kind`, walks past, and rehashes.
2. **Operator tooling is forward-compatible.** A Phase A operator's `tail -f | jq '.fields.peer'` doesn't break when Phase B adds `.fields.token_id` — it just doesn't see the new key. Same `jq` pipeline keeps working.

The `token_id` shape (truncated SHA-256 hex over the raw token bytes, computed at `BearerToken::from_bytes` time, kept in a sibling field on the wrapper) means even Phase B never logs the raw secret. Sub-track #6 (constant-time compare) guarantees the comparator path stays `subtle::ConstantTimeEq::ct_eq`; the audit-side hash is independent.

## Substrate-not-MVP receipt — what we deliberately did NOT defer

Three Phase A scope decisions were "could-have-deferred" candidates:

- **Restart-resume.** `FileAuditSink::open` reads the file's last line + recovers the chain head before opening the append-mode writer. We could have shipped "process restart = new chain file" and forced operators to manage chain continuity externally. Rejected because the substrate-not-MVP rule from ADR 0004 says ship the complete primitive the application space needs — and "a daemon restart breaks my chain" is exactly the broken edge case operators would file after their first incident.
- **`replay()` + `grx audit verify`.** We could have shipped the sink-side write path only and made operators BYO verifier. Rejected for the same reason — the chain is only useful if there's a way to walk it without re-implementing the hash + serde-canonicalization logic.
- **Strict-additive callsite refactor.** We could have shipped a separate `audit_sink::log_bind(…)` family that callers OPT IN to alongside the legacy `audit::log_bind(…)`. Rejected because the regression-risk lens demands the no-sink-installed path produces bit-for-bit identical tracing output, AND every callsite that gains a sink fan-out demands an audit-trail of which one. Threading `Option<&Arc<dyn AuditSink>>` through the existing helpers gives the compiler enforcement of "every audit-side callsite is reviewable in one place".

What WAS deferred to Phase B with a documented contract:

- Multi-token RBAC + the `auth_success` event.
- Batched-fsync worker task (the per-request-path sync cost becomes load-bearing under RBAC's `auth_success` fan-out).
- Atomic-swap of the audit-log file across SIGHUP rotation (rides sub-track #4's lifecycle handler).
- The `audit_sink_write_failures_total` counter on `MetricsInner` + `/v1/readyz` participation.

Each deferred item ships with its enabling work; folding any of them into Phase A would have either (a) demanded the secrets-keyring (#3) and lifecycle-SIGHUP (#4) sub-tracks be co-shipped (multiplying Phase A's blast radius) or (b) shipped half-formed without their dependencies (multiplying operator footguns).

## Cross-track contracts (locked here, executed elsewhere)

| Track | Contract |
|---|---|
| #2 TLS rotation | `tls_reload_attempt` / `tls_reloaded` / `tls_reload_failure` are FIRST-CLASS audit kinds. The `ApiServerHandle::audit_sink` field surfaces the sink so `reload_tls()` fans without re-threading state. (SHIPPED.) |
| #3 Secrets keyring | Phase B's `token_id` derivation uses the secrets keyring as the credential source; `token_id` is the truncated SHA-256 hex over the token bytes. The raw secret NEVER reaches the audit record. |
| #4 SIGHUP lifecycle | Phase B's audit-log file rotation rides the same SIGHUP handler #4 introduces for master config + cookie + token files. Atomic-swap semantics carry the prior file's last `this_hash` as the new file's first `prev_hash`. |
| #5 Observability | Phase B's `audit_sink_write_failures_total` counter participates in `/v1/readyz` (Phase C); a sink-failing daemon is NOT-READY so the load balancer drains it. |
| #6 Constant-time compare | The audit `token_id` is derived from a comparator-safe hash of the token; never logged raw. The `subtle::ConstantTimeEq::ct_eq` path stays the sole comparator. |
| #9 Chaos arm | Phase D's audit-log forwarder is a chaos-arm-instrumentation target: a chaos run that fills the disk MUST observe the sink-failure counter + tracing-side warning + the daemon-side `/v1/readyz` flip + the load balancer drain — all without daemon crash. |

## Open questions tracked (not blockers)

- **CBOR migration trigger threshold.** Phase B adds the `auth_success` event; once measured per-event encode cost crosses (operator-tunable) X µs the format flip becomes viable. No threshold pinned yet — Phase B will collect production-scale measurements first.
- **`audit verify --since <SEQ>`.** Phase A's verifier walks the whole file; a large chain (millions of records) takes seconds. Phase B's `grx admin audit-verify` daemon-side endpoint can answer "verify everything since seq=K" in O(records-since-K) by carrying the daemon's in-memory chain head as the starting `prev_hash`.
- **Multi-file chain spans.** Phase B's rotation lands chain-spanning, but operators may want to verify ONLY the most-recent file. Phase C will add a `--allow-prev-hash <HASH>` knob to `grx audit verify` so an operator can verify file N+1 against the tail of file N without concatenating.
