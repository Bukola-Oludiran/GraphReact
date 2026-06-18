# M-Prod — Auth + audit-log chain (sub-track #1)

**Status:** SHIPPED Phase A 2026-06-17 — hash-chained tamper-evident audit-log SINK substrate landed; the existing tracing-side `target="graphreact::observer::audit"` stream STILL fires bit-for-bit unchanged. Multi-token RBAC (Phase B), batched-fsync worker task (Phase B), and SIGHUP atomic-swap of the audit-log file (Phase B) are deferred — see [Phase B/C/D scope](#phase-bcd-scope).

This document covers sub-track #1 of the M-Prod milestone (production-hardening): operator-facing authentication, RBAC, and audit-log tamper-evidence for the observer HTTPS surface. Sibling tracks (#2 TLS rotation, #3 secrets keyring, #4 lifecycle / SIGHUP topology, #5 observability, #6 constant-time compares, #9 chaos arm) reference this substrate; see [Cross-track contracts](#cross-track-contracts).

## Phase A scope

Phase A ships the **SINK substrate** — the on-process append-only hash-chained NDJSON writer that future trigger surfaces (RBAC verdict events, audit replay tooling, SOC review pipelines) all consume through one stable on-disk schema. Phase A explicitly does NOT widen the auth middleware: no new tokens, no JWT, no RBAC checks, no per-node ACLs. The single bearer-token model from M16-Backend P4 stays put; Phase A is sink-only.

| Deliverable | Status | Location |
|---|---|---|
| `AuditSink` trait (object-safe, infallible `append`) | SHIPPED | [`crates/graphreact-observer/src/audit_chain.rs`](../../crates/graphreact-observer/src/audit_chain.rs) |
| `NopAuditSink` (default; no-op) | SHIPPED | `audit_chain.rs` |
| `FileAuditSink` (append-only NDJSON, per-line `sync_data`, restart-resume, **mode 0o600 enforced on Unix**) | SHIPPED | `audit_chain.rs` |
| `AuditRecord` / `AuditRecordInput` types | SHIPPED | `audit_chain.rs` |
| `replay(path)` chain verifier + `AuditChainError` typed errors | SHIPPED | `audit_chain.rs` |
| `ApiState::audit_sink` field + `SpawnOptions::audit_sink` | SHIPPED | [`crates/graphreact-observer/src/management_api.rs`](../../crates/graphreact-observer/src/management_api.rs) |
| `ApiServerHandle::audit_sink` field + `reload_tls` fan | SHIPPED | `management_api.rs` |
| `audit::log_*` helpers take `Option<&Arc<dyn AuditSink>>` last-arg | SHIPPED | [`crates/graphreact-observer/src/audit.rs`](../../crates/graphreact-observer/src/audit.rs) |
| `auth_layer` (missing / wrong bearer) fans to sink | SHIPPED | `management_api.rs::auth_layer` |
| `accept_loop` accept-err + per-peer TLS handshake-fail fan to sink | SHIPPED | `management_api.rs::accept_loop` |
| `DaemonHandle::shutdown` `otel_shutdown_timeout` fans to sink | SHIPPED | [`crates/graphreact-cli/src/commands.rs`](../../crates/graphreact-cli/src/commands.rs) |
| `--observer-audit-log <PATH>` CLI flag on `grx cluster daemon` | SHIPPED | [`crates/graphreact-cli/src/cli.rs`](../../crates/graphreact-cli/src/cli.rs) |
| `grx audit verify <PATH>` subcommand | SHIPPED | [`crates/graphreact-cli/src/audit_commands.rs`](../../crates/graphreact-cli/src/audit_commands.rs) |
| Unit tests (10) + integration tests (3 observer e2e + 3 CLI verify) | SHIPPED | `crates/graphreact-observer/{src,tests}` + `crates/graphreact-cli/tests/audit_verify.rs` |
| Mode-0o600 enforcement on create + on open-existing (verify-finding patch) | SHIPPED | `audit_chain.rs::FileAuditSink::open` + 2 unit tests |
| Multi-token RBAC + per-route ACLs | DEFERRED → Phase B | n/a |
| Batched-fsync worker task | DEFERRED → Phase B | n/a |
| Atomic-swap of audit-log file across SIGHUP rotation | DEFERRED → Phase B | n/a |
| `grx admin audit-verify` cron-style verification | DEFERRED → Phase C | n/a |
| ACME-style audit-log forwarder (S3 / Loki / GCS) | DEFERRED → Phase D | n/a |

## On-disk record shape

`FileAuditSink::append` emits **one NDJSON line per record**, formatted as the `serde_json::to_vec` encoding of:

```rust
pub struct AuditRecord {
    pub seq: u64,           // monotonic counter starting at 0
    pub ts_ms: u128,        // Unix epoch milliseconds at append time
    pub prev_hash: String,  // hex SHA-256 of the previous record's hash input
    pub kind: String,       // bind / accept / auth_fail / response / tls_reload_{attempt,success,failure} / accept_failed / tls_handshake_failed / otel_shutdown_timeout
    pub fields: serde_json::Value, // opaque blob (peer / path / reason / cert_bytes_len / etc.)
    pub this_hash: String,  // hex SHA-256 of {seq, ts_ms, prev_hash, kind, fields}
}
```

Genesis record (seq=0) has `prev_hash = "0".repeat(64)`. Each subsequent record's `prev_hash` is the prior record's `this_hash`. The hash is SHA-256 over the canonical-JSON bytes of `{seq, ts_ms, prev_hash, kind, fields}` (this_hash itself omitted, because hashing the hash is circular).

### Why NDJSON, not CBOR

1. Operators `jq` / `grep` / `tail -f` the file the same way they consume `GRX_OBSERVER_LOG` and the unified JSON-log subscriber's stderr stream. Zero new tooling.
2. Canonicalization is straightforward — the canonical bytes input to the hash are exactly what `serde_json` emits for `{seq, ts_ms, prev_hash, kind, fields}` minus the trailing newline.
3. Phase A's load profile is bind / accept / auth-fail / response / TLS-rotate — modest per-second volume. The JSON encode cost is dwarfed by the file write + `sync_data`.
4. `serde_json` is already a workspace dep at `Cargo.toml:59`. CBOR would pull `ciborium` (NEW dep) and break operator replay tooling.
5. Phase B can flip the wire format to CBOR behind an `audit_format=cbor` option if perf/size becomes load-bearing once RBAC fans `auth_success` events on every authorized request — without changing the `AuditSink` trait shape or the chain-hash construction (canonical bytes become the CBOR serialization).

### Phase B forward-compat for the `fields` blob

Phase A populates `fields` with the same structured key/value pairs the existing `tracing::{info,warn}!` line emits at every callsite:

| `kind` | `fields` keys |
|---|---|
| `bind` | `addr`, `tls`, `auth` |
| `accept` | `peer`, `path` |
| `auth_fail` | `peer`, `reason` |
| `response` | `peer`, `path`, `status`, `bytes` |
| `tls_reload_attempt` | `cert_bytes_len`, `key_bytes_len` |
| `tls_reload_success` | `leaf_fingerprint_sha256` |
| `tls_reload_failure` | `error_kind`, `detail` |
| `accept_failed` | `detail` |
| `tls_handshake_failed` | `peer`, `detail` |
| `otel_shutdown_timeout` | `deadline_ms` |

Phase B's multi-token RBAC will add `token_id` (stable short hash over the token bytes), `route`, and `verdict` keys to `fields` for the new `auth_success` / `auth_denied_by_acl` kinds — without bumping the schema. The `replay` walker hashes the WHOLE record (whatever shape `fields` carries), so an older replay tool that doesn't know the new keys still verifies the chain correctly; the new keys ride through as opaque payload.

## Strict-additive guarantee

The substrate-not-MVP contract for Phase A is: a callsite with no sink installed produces **BIT-FOR-BIT IDENTICAL tracing output** to the pre-M-Prod baseline. This is the regression-risk lens's headline check.

Implementation: every `audit::log_*` helper grows an `Option<&Arc<dyn AuditSink>>` last-parameter; when the parameter is `None`, the existing `tracing::{info,warn}!(target: "graphreact::observer::audit", …)` line runs unchanged and the fan-out short-circuits. When `Some(sink)`, the tracing line ALSO runs unchanged, and an `AuditRecordInput` is pushed to the sink in parallel. The two surfaces are independent — a sink-side I/O failure NEVER stops the tracing-side stream.

Callsites where no sink can be threaded through (the legacy P2 `/metrics` server pre-management-API, the `JsonLogTelemetry::from_env()` file-open fallback at process-start, the json_escape `audit_sink_write_failed` self-warning) pass `None` explicitly + are commented to make the policy auditable.

## fsync policy

Phase A fsyncs (`std::fs::File::sync_data`) on every append: correctness over throughput for the substrate. The audit-record write path is synchronous on the same thread the auth middleware future / accept-loop tokio task runs on, so:

- `bind` (1× per daemon start), `tls_reload_*` (operator-driven), `accept_failed` (rare), `tls_handshake_failed` (rare, per-peer), `otel_shutdown_timeout` (1× per daemon shutdown): no observable latency cost.
- `auth_fail` (per-bad-bearer-request): one disk sync per failed auth attempt. On a fast NVMe this is sub-millisecond; on rotational disks or shared cloud storage it can be 5–10ms. Operators tuning a high-auth-fail-flood deploy should consider Phase B's batched-fsync mode.

Phase B will (a) move the sink to an `mpsc`-fed worker task off the request path, (b) batch fsyncs (per N records OR per 250ms, whichever first), (c) expose a `--observer-audit-log-fsync=always|periodic|never` knob so compliance deploys can demand per-record sync while latency-sensitive deploys opt out.

## File permissions (mode 0o600)

`FileAuditSink::open` enforces mode `0o600` on the audit-log file on Unix:

1. **On create.** The `OpenOptions` call passes `.mode(0o600)` via `std::os::unix::fs::OpenOptionsExt` so a fresh file lands at user-readable-only, bypassing the process umask (typically `0o022` → `0o644`).
2. **On open of a pre-existing file.** `std::fs::set_permissions(path, PermissionsExt::from_mode(0o600))` runs unconditionally after open. Without this, a chain file that was world-readable BEFORE this patch landed (or one a misconfigured operator created with `touch`) would silently stay world-readable across daemon restarts.

Why this matters: audit records carry **peer IPs of every scraper / auth-failer**, **TLS leaf SHA-256 fingerprints** (rotation receipts that ops correlate against secret material), **auth-failure shape** (`missing` / `wrong` + peer), and full chain integrity. The records do **not** carry bearer-token values or PEM bytes — that invariant is enforced at the record-construction sites in `audit.rs` — but the metadata is operator-secret-class data on multi-tenant hosts (CI runners, dev workstations, k8s nodes with non-root pods, shared bastions). Mode-0o600 keeps every unprivileged user on the box from `cat`-ing the chain.

Windows: deferred — the substrate doesn't currently expose a chmod-equivalent. Operators running on Windows hosts should rely on directory-level ACLs as a stop-gap; a follow-up slice can wire `Sid` ACLs once Phase B's auth surface lands.

Verified by two unit tests in `audit_chain::tests`:

- `file_audit_sink_enforces_0600_mode_on_create` — fresh file lands at `0o600`.
- `file_audit_sink_tightens_mode_on_preexisting_file` — a planted `0o644` file is tightened to `0o600` on next open.

## Sink-failure policy

`AuditSink::append` is infallible at the trait. Sinks degrade as follows on I/O error:

1. `FileAuditSink::append` writes / flushes / `sync_data` each return `Result`. On error, the sink emits a one-shot `tracing::warn!(target: "graphreact::observer::audit", event = "audit_sink_write_failed", …)` — the line FLOWS through the tracing-side stream, so JSON-log subscribers still see it.
2. The in-memory chain state advances past the failed `seq`. Without this, a follow-up successful write would reuse the same `seq` and tear the chain.
3. The next successful write re-seats the chain at the correct `prev_hash` (the in-memory `prev_hash` was advanced in step 2 to the would-have-been `this_hash`).
4. Phase B adds an `audit_sink_write_failures_total` counter on `MetricsInner`. Operators alert on the counter; Phase C extends `/v1/readyz` to participate (sink-failing → NOT-READY).

The substrate refuses to **panic** on audit-write failure: audit is meant to be observed, not load-bearing on liveness. An attacker who can fill the disk MUST NOT also be able to crash the daemon.

## Replay + verification

`graphreact_observer::replay(path)` walks the file from `seq=0`, recomputes each record's `this_hash` from its canonical bytes, and verifies:

- monotonic `seq` (`expected_seq == record.seq`)
- chain links (`expected_prev == record.prev_hash`)
- hash integrity (`recomputed == record.this_hash`)

Returns the full chain on success or one of:

- `AuditChainError::TamperDetected { seq, expected_hash, found_hash }` — any byte-level mutation of an existing record breaks here.
- `AuditChainError::PrevHashMismatch { seq, expected_prev_hash, found_prev_hash }` — record reordering / insertion of a forged record without re-hashing the chain breaks here.
- `AuditChainError::SequenceGap { expected_seq, found_seq }` — record deletion mid-chain breaks here.
- `AuditChainError::Parse { line_index, detail }` — NDJSON syntax / schema failure.
- `AuditChainError::Io { path, source }` — file unreadable.

### `grx audit verify <PATH>`

Phase A ships the CLI verifier:

```bash
$ grx audit verify /var/log/grx/audit.ndjson
audit chain OK: 5132 records
$ echo $?
0

$ grx audit verify /tmp/tampered.ndjson
tamper detected at seq=247: expected_hash=8a3f… found_hash=deadbeef…
$ echo $?
1
```

The success line goes to stdout (CI pipelines scrape it); the structured failure message goes to stderr.

### Self-verify recipe (no `grx` binary)

For air-gapped audits where the `grx` binary may not be present, a stdlib Python snippet verifies the chain:

```python
import hashlib, json, sys

prev = "0" * 64
for n, line in enumerate(open(sys.argv[1])):
    rec = json.loads(line)
    assert rec["seq"] == n, f"sequence gap at line {n}"
    assert rec["prev_hash"] == prev, f"prev-hash mismatch at seq {rec['seq']}"
    body = json.dumps({k: rec[k] for k in ("seq", "ts_ms", "prev_hash", "kind", "fields")}, separators=(",", ":"), sort_keys=False).encode()
    expected = hashlib.sha256(body).hexdigest()
    # NOTE: serde_json preserves struct-declaration order. Python's
    # json.dumps with sort_keys=False also preserves dict insertion
    # order. The two are equivalent here because the Python dict
    # is constructed in the same declaration order as the Rust
    # struct's serde derive emits.
    if expected != rec["this_hash"]:
        sys.exit(f"tamper at seq {rec['seq']}: expected {expected} found {rec['this_hash']}")
    prev = rec["this_hash"]
print(f"audit chain OK: {n + 1} records")
```

## File rotation

`FileAuditSink` is **append-only forever** within a process. The chain `prev_hash`/`seq` invariants demand it:

- Truncation (`logrotate copytruncate`) BREAKS the chain — the bytes a `replay` walker depends on disappear mid-file.
- In-process rotation (rename file + open a new one) would lose the chain-head anchor unless the new file's first record's `prev_hash` is the old file's last `this_hash`. Phase A intentionally refuses to implement this. The chain restart anchor across a process restart already works — the `FileAuditSink::open` constructor reads the file's last line, parses its `this_hash`, and resumes — but in-process file swap is Phase B's job.

**Recommended operator pattern**: rotate by renaming the file path externally + restart the daemon. Phase B will land SIGHUP-driven atomic-swap rotation (rides the same code path sub-track #2 uses for TLS rotation), where the new file inherits the prior file's last `this_hash` as its first `prev_hash`. Until then: rename + restart.

## CLI surface

```
grx cluster daemon \
    --node-id alice@127.0.0.1:9000 \
    --listen 127.0.0.1:9000 \
    --cookie-env COOKIE \
    --cert ./alice.crt --key ./alice.key --ca ./ca.crt \
    --observer-bind 127.0.0.1:9090 \
    --observer-token-env OBS_TOKEN \
    --observer-cert ./obs.crt --observer-key ./obs.key \
    --observer-audit-log /var/log/grx/audit.ndjson
```

The flag is optional — when absent, the existing `target="graphreact::observer::audit"` tracing stream is the sole audit surface (bit-for-bit unchanged from pre-M-Prod). When set, every audit event ALSO writes a chained NDJSON record to the file.

```
grx audit verify /var/log/grx/audit.ndjson
```

Phase B will add `grx audit replay`, `grx audit inspect <SEQ>`, and `grx audit follow <PATH>` (tail-style follower).

## Phase B/C/D scope

| Phase | Scope |
|---|---|
| **B** | Multi-token RBAC (per-token id, per-route ACL); `auth_success` audit event; batched-fsync worker task (request-path latency mitigation); SIGHUP atomic-swap of audit-log file (cross-rotation chain continuity); `audit_sink_write_failures_total` MetricsInner counter; `grx admin reload-tokens` operator surface. |
| **C** | `/v1/readyz` participates in sink-failure (sink-failing → NOT-READY); `grx admin audit-verify` daemon-side endpoint for live chain verification without copying the file off-box; ACME-style audit-log forwarder cooperating with sub-track #9 chaos arm. |
| **D** | Audit-log forwarder backends (S3 / Loki / GCS / OpenSearch); per-tenant RBAC + audit-namespace partitioning; integration with [`ADR 0006`](../decisions/0006-audit-chain-substrate.md)'s downstream substrate work. |

## Cross-track contracts

| Track | Contract |
|---|---|
| **#2 TLS rotation** (Phase A SHIPPED) | TLS rotation events (`tls_reload_attempt` / `tls_reloaded` / `tls_reload_failure`) ride the same `AuditSink` surface this track ships. The `ApiServerHandle::audit_sink` field surfaces the sink so `reload_tls()` fans to it without re-threading state. |
| **#3 Secrets keyring** (Phase A SHIPPED) | Phase B token-id derivation uses the secrets keyring as the credential source. Audit records carry the derived `token_id` (truncated SHA-256 hex over the token bytes computed at load-time on the `BearerToken` constructor) — never the raw secret. |
| **#4 SIGHUP lifecycle** (DEFERRED) | Phase B audit-log file rotation rides the same SIGHUP handler sub-track #4 introduces for the master config + cookie + token files. Atomic-swap semantics carry the prior file's last `this_hash` as the new file's first `prev_hash` so the chain spans rotations. |
| **#5 Observability** (Phase A SHIPPED) | `audit_sink_write_failures_total` counter (Phase B) participates in `/v1/readyz` (Phase C) — a sink-failing daemon is NOT-READY so the load balancer drains it before an attacker can exploit the silenced audit. |
| **#6 Constant-time compare** (Phase A SHIPPED) | The audit record's `token_id` is derived from a constant-time-comparable hash of the token; never logged in raw form. The `subtle::ConstantTimeEq::ct_eq` path stays the sole comparator. |
| **#9 Chaos arm** (DEFERRED) | Phase D's audit-log forwarder is a chaos-arm-instrumentation target: a chaos run that fills the disk MUST observe the sink-failure counter + tracing-side warning, the daemon-side `/v1/readyz` flip to NOT-READY, and the load balancer drain — all without daemon crash. |

## ADR

The contract is locked in [`ADR 0006 — audit chain is a strict-additive sink substrate`](../decisions/0006-audit-chain-substrate.md), an addendum to [ADR 0004's substrate-primitive convention](../decisions/0004-ingress-as-substrate-primitive.md).
