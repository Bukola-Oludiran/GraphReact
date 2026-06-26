# 0011 — Audit-log forwarder substrate: pluggable trait + spool/stderr reference impls

**Status:** DECIDED — recorded 2026-06-26. M-Prod sub-track #1 Phase B Part 2 (Group 1d) SHIPPED 2026-06-26.

**Related:** [ADR 0006](0006-audit-chain-substrate.md) (hash-chained NDJSON audit-log; the forwarder fans the same `AuditRecord` shape), [ADR 0009](0009-token-registry-file-format.md) (mode-0o600 file invariant the spool forwarder reuses), [ADR 0010](0010-oidc-jwt-bridge-config-format.md) (the Group 1c sibling that established the "file-based + sidecar-driven" pattern this ADR generalizes). Cross-references: [m-prod-auth.md §"Shipped — Phase B Part 2 (Group 1d)"](../backend/m-prod-auth.md), [m-prod.md](../backend/m-prod.md) (cross-track contracts table), [m-prod-lifecycle.md](../backend/m-prod-lifecycle.md) (the `OBSERVER_SHUTDOWN_DEADLINE` budget composition).

## Why an ADR (and why now)

The original Phase B/C/D scope of M-Prod sub-track #1 names "audit-log forwarder backends (S3 / Loki / GCS / OpenSearch)" as a deliverable. The naive read is "pull aws-sdk-s3 / OpenSearch / GCP / Loki client libraries into the daemon." That path would (a) inflate the dependency tree by a factor of 3–4×, (b) require credential management + retry-with-backoff + connection pooling for each backend, and (c) duplicate every operator's existing logging pipeline (every production k8s cluster + every bare-metal node already runs a log-collector sidecar). This ADR pins the decision to SHIP A PLUGGABLE TRAIT + REFERENCE IMPLS that match how operators already integrate audit streams, and explicitly defers the bundled SDKs.

## The decisions (eight)

1. **Substrate is a TRAIT + reference impls, not bundled SDKs.** `AuditForwarder` lives in `graphreact-observer::audit_forwarders`; two reference implementations ship: `SpoolFileForwarder` (NDJSON to a spool file) + `StderrJsonLinesForwarder` (JSON-lines to process stderr). Bundled S3 / Loki / GCS / OpenSearch / Splunk / Datadog client backends are EXPLICITLY DEFERRED to γ.next, gated on operator demand. Operators reach these backends today via the sidecar pattern (see decision #2).

2. **Sidecar pattern is the canonical operator path to S3 / Loki / GCS / OpenSearch / Splunk / Datadog.** `fluentd`, `vector`, `promtail`, `fluent-bit`, `opentelemetry-collector`, `aws-for-fluent-bit`, `google-cloud-ops-agent` — every production deployment shape (k8s DaemonSet, systemd unit, container sidecar) — already knows how to tail an NDJSON file or container stderr and ship to any backend. The forwarder substrate emits a wire-compatible NDJSON stream of `AuditRecord` values so any of these sidecars work without daemon-side configuration.

3. **Bounded queue + drop-newest back-pressure** (NOT drop-oldest, NOT blocking). Each registered forwarder gets its own bounded `tokio::sync::mpsc::channel` (default 1024 slots, tunable via `GRX_AUDIT_FORWARDER_QUEUE_DEPTH`). Producer uses `try_send`; on `Full` the new record is dropped and `audit_forwarder_records_total{forwarder, outcome="dropped_queue_full"}` bumps. Rationale: blocking the audit-sink hot path would break the bench-(b) ≤1% gate; drop-oldest would require a custom ring buffer in front of the channel (tokio mpsc exposes no "pop oldest" primitive). Operators observe sustained drops via the queue-depth gauge + the counter; the sidecar's downstream retry is the persistence layer.

4. **Hash-chain visibility is preserved bit-for-bit.** Forwarders receive the FULL `AuditRecord` shape (`seq`, `ts_ms`, `prev_hash`, `kind`, `fields`, `this_hash`). External chain verifiers walking the spool / syslog stream produce the same result as the in-process `grx audit verify` — same hashes, same sequence, same chain markers. The rotation boundary records (`audit_log_rotated` with `boundary: "prior_tail"` / `"new_head"`) ride the same fan-out path, so cross-file linkage is observable externally without consulting the in-process file.

5. **Mode 0o600 is enforced on the spool file.** The spool file mirrors the in-process audit log — peer IPs, auth-failure reasons, TLS leaf fingerprints, full record bytes. Mode 0o600 enforced on create + on a pre-existing file + after rotation, matching the audit-log + token-registry invariant from ADR 0009. Stderr does NOT enforce a mode (it's a process FD; the OS handles permissions on the controlling tty / container log file).

6. **Drain budget composition: forwarder hub drain runs INSIDE `OBSERVER_SHUTDOWN_DEADLINE`, in PARALLEL with the in-process audit sink drain, under a SINGLE deadline read via `tokio::join!`.** The locked drain-budget math from [m-prod-lifecycle.md](../backend/m-prod-lifecycle.md) — `drain_deadline (20s) + OBSERVER_SHUTDOWN_DEADLINE (5s) + slack (5s) ≤ k8s terminationGracePeriodSeconds (30s default)` — stays bit-for-bit unchanged. Per-forwarder drain timeouts surface as `audit_forwarder_drain_timeouts_total{forwarder}` + an `audit_forwarder_drain_timeout` warn on the canonical audit target; the aggregate is `audit_forwarder_drain_timeout_aggregate`.

7. **SIGHUP rotation extends the 1ab/1c fan-out chain.** The gate at `drive_handle_until` becomes a 5-way OR: `tls_paths.is_some() || tokens_path.is_some() || audit_log_path.is_some() || jwks_path.is_some() || audit_forwarder_spool_path.is_some()`. The fan-out arm gains `handle.rotate_audit_forwarder_spool()` immediately after `handle.rotate_audit_log()` — spool rotation is a peer to audit-log rotation in chain semantics. Atomic-rename + 0o600 reopen matches `FileAuditSink::rotate_now`'s shape. The boundary `audit_log_rotated` markers from the in-process rotation are forwarded through the normal fan-out, so the spool's own rotation does NOT re-stamp boundary markers (external verifiers see them via the in-process rotation fan).

8. **Bounded metric cardinality enforced at the type level.** `forwarder: &'static str` + `outcome: &'static str` close the vocabulary at compile time. Three new metric families with a total of 12 series across the closed 2-forwarder × {forwarded, dropped_queue_full, dropped_shutdown, write_failed} matrix:
   - `grx_audit_forwarder_records_total{forwarder, outcome}` (8 series)
   - `grx_audit_forwarder_queue_depth{forwarder}` (2 series)
   - `grx_audit_forwarder_drain_timeouts_total{forwarder}` (2 series)
   Each family is suppressed when its DashMap is empty (Prometheus best practice), so steady-state daemons with no forwarders wired pay zero HELP+TYPE lines on `/metrics`.

## On-wire NDJSON shape

Each line on the spool / stderr stream is one fully-stamped `AuditRecord` — the same JSON shape the in-process audit log uses:

```json
{"seq":0,"ts_ms":1700000000000,"prev_hash":"0000…","kind":"bind","fields":{"addr":"127.0.0.1:9090","tls":false,"auth":false},"this_hash":"3a7f…"}
{"seq":1,"ts_ms":1700000001000,"prev_hash":"3a7f…","kind":"accept","fields":{"peer":"127.0.0.1:55123","path":"/v1/metrics"},"this_hash":"b1c2…"}
```

Operators verifying the chain externally walk the file from the first line, recompute each `this_hash` from canonical bytes (struct-declaration order, omit `this_hash` from the hash input), and check each record's `prev_hash` against the prior record's `this_hash`. Genesis is 64 hex zeros. The recipe is identical to `grx audit verify` against the in-process file.

## Trait shape

```rust
pub trait AuditForwarder: Debug + Send + Sync {
    fn forward(&self, record: &AuditRecord);
    fn name(&self) -> &'static str;
    fn drain<'a>(&'a self, deadline: Duration)
        -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> { /* default true */ }
    fn rotate(&self)
        -> Result<Option<ForwarderRotationOutcome>, AuditChainError> { /* default Ok(None) */ }
}
```

- `forward` is infallible at the trait level. Implementors swallow I/O errors internally (counter bump + one-shot `tracing::warn!` on the canonical audit target). Runs on the per-forwarder worker task; may block for moderate I/O without back-pressuring the audit-sink hot path.
- `name` returns a `&'static str` from a CLOSED vocabulary so the metric label cardinality cap is enforced at compile time. Today: `{"spool_file", "stderr_jsonl"}`. Future reference impls extend the vocabulary.
- `drain` is dyn-compatible via a boxed future (no `async-trait` dep; matches `AuditSink::drain` shape).
- `rotate` is dyn-compatible by being non-async. Default returns `Ok(None)` so trivial forwarders (stderr) compile without overriding.

## CLI flags

| Flag | Effect |
|---|---|
| `--observer-audit-forwarder-spool <PATH>` | Open a `SpoolFileForwarder` writing NDJSON to `<PATH>` (mode 0o600 enforced). SIGHUP rotates the spool via `DaemonHandle::rotate_audit_forwarder_spool`. |
| `--observer-audit-forwarder-stderr` | Attach a `StderrJsonLinesForwarder` writing JSON lines to process stderr. Kubernetes-native — sidecar DaemonSets tail container stderr. |
| (env) `GRX_AUDIT_FORWARDER_QUEUE_DEPTH` | Per-forwarder bounded-queue capacity. Default 1024. |

Both forwarder flags are independent — either can be set without the other, OR both can be combined. When neither is set, the `ForwarderHub` itself is `None` (zero overhead — no worker tasks spawned).

## Reversibility

If γ.next surfaces operator demand for a bundled S3 / Loki / GCS / OpenSearch backend, the surface is purely additive: implement `AuditForwarder` for the new backend, add a CLI flag that constructs it + appends to the forwarder list, and the existing fan-out + metrics + drain machinery picks it up unchanged. The decision to defer (decision #1) is itself reversible without breaking any cross-track contract.

If the bounded-queue policy needs to change to drop-OLDEST (decision #3), the substrate accommodates a ring-buffer adapter in front of each forwarder's `try_send`. The current drop-newest decision is the simpler shape that matches tokio idioms; switching is reversible but requires a custom adapter type.

## Cross-track contracts the substrate preserves bit-for-bit

- `AuditSink::append` signature unchanged (Phase A locked). Forwarder fan-out happens INSIDE `FileAuditSink::append` AFTER the in-process write — substrate is fully additive on top of the Phase A surface.
- `ApiState` / `ApiServerHandle` / `SpawnOptions` extended ONLY by adding `forwarder_hub: Option<Arc<ForwarderHub>>`; legacy spawn paths default to `None`.
- The SIGHUP gate extension is purely additive (5-way OR vs prior 4-way).
- The rotation fan-out order audit-rotate → spool-rotate → tokens → JWKS → TLS preserves the 1ab/1c rule that audit-log rotation goes FIRST so subsequent reload events land in the fresh files.
- Drain-budget math from m-prod-lifecycle.md (20+5+5=30s) unchanged — forwarder hub drain runs in parallel with the in-process audit sink drain under a single deadline read.
- No new workspace dependencies. The reference impls use only `tokio`, `std`, `serde_json`, and existing `audit_chain` types.

## DRR (deferred to γ.next)

Operator-shaped follow-ons that the substrate intentionally does NOT bundle today:

- Bundled S3 / Loki / GCS / OpenSearch / Splunk / Datadog SDK backends. Gate on operator demand surfacing in a real deployment context (today, the sidecar pattern covers every backend).
- Retry-with-backoff for transient forwarder failures. Sidecars already implement this for their downstream targets; bundling it in the daemon would duplicate logic and complicate the drain budget. If a future bundled SDK backend lands, retry-with-backoff comes with it.
- Per-forwarder rate-limiting beyond the bounded queue. Today's drop-newest is the operator-observable rate-limit shape; rich token-bucket / leaky-bucket per forwarder is deferred until an operator demonstrates the bounded queue is insufficient.
- Forwarder health probes on `/v1/readyz`. Today a forwarder failure is observable via the metrics + the audit stream; surfacing it on readyz would change the readyz contract (today: "is the daemon ready to accept new connections", future would conflate "is the audit pipeline healthy").
- A `--observer-audit-forwarder-config <PATH>` JSON file shape for configuring multiple forwarders in one place (the current two-flag interface scales to the 2-impl vocabulary; if the reference impl list grows past ~4, a config-file shape becomes the right surface).
