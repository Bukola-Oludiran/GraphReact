# M-Prod — TLS rotation (sub-track #2)

**Status:** SHIPPED Phase A 2026-06-17 + Phase B Part 1 SIGHUP 2026-06-18 — atomic-swap substrate + SIGHUP signal handler landed; ACME / cert-manager (Phase C) + session-resumption ticket rotation + Windows path + dist-mTLS rotation deferred to Phase B Part 2 / later.

This document covers sub-track #2 of the M-Prod milestone (production-hardening): in-process TLS certificate rotation for the observer HTTPS surface. Sibling tracks (#1 token rotation, #4 lifecycle / SIGHUP topology, #9 chaos arm) reference this substrate; see [Cross-track contracts](#cross-track-contracts).

## Shipped — Phase B Part 1 (SIGHUP wire-up)

**Status:** SHIPPED 2026-06-18 — SIGHUP signal handler triggers `reload_tls_from_disk` against the cert + key paths captured at boot, atomically swapping through Phase A's `ApiServerHandle::reload_tls` substrate. Unix only. ACME / cert-manager + session-ticket rotation + Windows path remain DEFERRED to Phase B Part 2 / Phase C.

| Deliverable | Status | Location |
|---|---|---|
| SIGHUP signal stream in `drive_handle_until` | SHIPPED | `crates/graphreact-cli/src/commands.rs::drive_handle_until` (new `sighup.recv()` arm alongside SIGTERM + SIGINT + tick) |
| `DaemonHandle::reload_tls_from_disk` helper | SHIPPED | `crates/graphreact-cli/src/commands.rs` |
| `TlsPaths` struct + `DaemonOptions.observer_tls_paths` + `DaemonHandle.tls_paths` retention | SHIPPED | `crates/graphreact-cli/src/commands.rs` + `crates/graphreact-cli/src/bin/grx.rs` |
| `tls_reload_skipped` audit event (drain-respect) | SHIPPED | `crates/graphreact-observer/src/audit.rs::log_tls_reload_skipped` |
| Boot warning on relative TLS paths (`tls_paths_relative`) | SHIPPED | `crates/graphreact-cli/src/commands.rs::validate_tls_paths_absolute` |
| Integration test (direct-call + drain-respect + subprocess SIGHUP) | SHIPPED | `crates/graphreact-cli/tests/sighup_cert_rotation.rs` + `sighup_cert_rotation_subprocess.rs` |

### Topology decision: one tokio::select! task, four arms

Both SIGTERM (Phase α, sub-track #4) and SIGHUP (this Phase B Part 1) run as `tokio::signal::unix::signal(SignalKind::*)` streams selected inside `drive_handle_until`'s existing `tokio::select!`. Pros vs. independent spawned tasks: no extra shutdown coordination, no race against `DaemonHandle` mutation, signal handling concentrated in one file. The arms are ordered `tick` → `sigint` → `sigterm` → `sighup`; `tokio::select!` is fair across arms so a SIGHUP storm cannot starve ticks. SIGHUP is NOT gated by `if duration.is_none()` (unlike SIGINT/SIGTERM) because it does not exit the loop — duration-bounded daemons (CI tests, scheduled cron-style runs) still need to respond to operator-driven rotation.

### Drain-respect

When a SIGTERM drain is in progress, SIGHUP is a no-op — rotating the cert mid-drain would generate a confusing `tls_reload_attempt` + `tls_reload_success` audit pair mid-drain that operators would have to explain, AND we're tearing down the accept loop in seconds anyway. Drain-respect is enforced by `DaemonHandle::reload_tls_from_disk` checking `self.drain.as_ref().is_some_and(|f| f.is_draining())` BEFORE the file read; on the skip path it emits `log_tls_reload_skipped("draining", sink)` and returns. The skip event is `WARN`-level on the `graphreact::observer::audit` target with field `reason="draining"`.

### Boot-time path validation

`start_daemon` validates that cert + key paths are absolute when `observer_tls_paths` is `Some`. Relative paths are accepted (back-compat) but a `tls_paths_relative` WARN audit row fires at boot — relative paths resolve against the daemon's CWD which can shift after systemd `WorkingDirectory` changes or `chdir`, making SIGHUP re-read non-deterministic. Operators should always pass absolute paths via `--observer-cert` / `--observer-key`.

### Audit-event additions

| Event | Level | Fields | Emitted by |
|---|---|---|---|
| `tls_reload_skipped` | WARN | `reason: &'static str` (currently only `"draining"`) | `DaemonHandle::reload_tls_from_disk` when drain is in progress |
| `tls_paths_relative` | WARN | `cert_path: String`, `key_path: String` | `start_daemon`'s `validate_tls_paths_absolute` (boot-only; tracing-only, not fanned to AuditSink — mirrors `drain_budget_misconfig` precedent) |

## Phase A scope

Phase A ships the **substrate** — the on-process atomic-swap mechanism that future trigger surfaces (SIGHUP, ACME, REST endpoint, manual `grx admin reload-tls`) all drive through a single load-bearing entry point. Phase A explicitly does NOT ship a trigger surface; operators today exercise the rotation via direct `ApiServerHandle::reload_tls(cert_pem, key_pem)` calls from in-process integration tests / embedding code.

| Deliverable | Status | Location |
|---|---|---|
| `RotatableServerConfig` atomic-swap container | SHIPPED | [`crates/graphreact-observer/src/tls_rotation.rs`](../../crates/graphreact-observer/src/tls_rotation.rs) |
| `ServerTlsConfig.server` → `RotatableServerConfig` | SHIPPED | [`crates/graphreact-observer/src/tls.rs`](../../crates/graphreact-observer/src/tls.rs) |
| Per-accept `TlsAcceptor` construction on management API | SHIPPED | [`crates/graphreact-observer/src/management_api.rs`](../../crates/graphreact-observer/src/management_api.rs) `accept_loop` |
| Per-accept `TlsAcceptor` construction on legacy `/metrics` server | SHIPPED | [`crates/graphreact-observer/src/http_server.rs`](../../crates/graphreact-observer/src/http_server.rs) `accept_loop` |
| `ApiServerHandle::reload_tls(cert_pem, key_pem)` public API | SHIPPED | `management_api.rs::ApiServerHandle::reload_tls` |
| `TlsRotationError` typed errors | SHIPPED | `tls.rs::TlsRotationError` |
| Audit events (`tls_reload_attempt` / `tls_reloaded` / `tls_reload_failure`) | SHIPPED | [`crates/graphreact-observer/src/audit.rs`](../../crates/graphreact-observer/src/audit.rs) |
| Integration tests (7) | SHIPPED | [`crates/graphreact-observer/tests/tls_rotation.rs`](../../crates/graphreact-observer/tests/tls_rotation.rs) |
| ~~SIGHUP signal handler~~ | ~~DEFERRED → Phase B~~ → **SHIPPED Phase B Part 1 2026-06-18** | `crates/graphreact-cli/src/commands.rs::drive_handle_until` + `DaemonHandle::reload_tls_from_disk` |
| ACME / cert-manager auto-renewal | DEFERRED → Phase C | n/a |
| Dist crate mTLS rotation (server + client) | DEFERRED — separate track | `crates/graphreact-dist/src/tls_config.rs` (untouched) |
| Session-ticket rotation | DEFERRED → Phase B Part 2 | n/a |

## RotatableServerConfig shape

```rust
pub struct RotatableServerConfig {
    inner: Arc<ArcSwap<rustls::ServerConfig>>,
}
```

- `Arc<ArcSwap<_>>` so every clone observes the same rotation. `BindConfig.tls`, the management-API accept loop, the legacy `/metrics` accept loop, and `ApiServerHandle.tls` are all clones of the same handle.
- `.current() -> Arc<ServerConfig>` is the hot-path read; calls `ArcSwap::load_full` (one atomic load + one `Arc::clone`) and returns an owned `Arc` that is safe to hold across `.await` — unlike `ArcSwap::load`'s `Guard`, which is not-`Send`.
- `.rotate(next: Arc<ServerConfig>)` is the write path; atomically replaces the inner `Arc`. The previous `Arc` stays alive as long as any in-flight handshake holds it.

**Why per-accept, not per-spawn, `TlsAcceptor`.** `tokio_rustls::TlsAcceptor::from(Arc<ServerConfig>)` snapshots the `Arc` at *construction* time. If we kept one acceptor per spawn and rotated underneath it, the cached acceptor would keep using the OLD config indefinitely. Phase A therefore constructs the `TlsAcceptor` inside each per-connection task:

```rust
let acceptor = TlsAcceptor::from(tls_handle.current());
acceptor.accept(socket).await
```

The per-handshake overhead is one `Arc::clone` (sub-µs); the TLS handshake itself dwarfs it (~5–50 ms).

**In-flight handshake safety.** A handshake in flight at the moment of rotation holds its own `Arc<ServerConfig>` clone for the lifetime of `acceptor.accept(socket).await`. The rustls `ServerConnection` does not re-consult the `ServerConfig` after the handshake completes, so post-handshake HTTP/1.1 streams on the OLD config are unaffected by the swap. No torn config, no use-after-free, no truncated streams.

## reload_tls API

```rust
impl ApiServerHandle {
    pub fn reload_tls(
        &self,
        cert_pem: &[u8],
        key_pem: &[u8],
    ) -> Result<(), TlsRotationError>;
}
```

**Inputs.** PEM-encoded cert (one or more `BEGIN CERTIFICATE` blocks; first is the leaf) + PEM-encoded private key (PKCS#8 / RSA / SEC1 — anything rustls accepts).

**Behavior.**
1. Emit `tls_reload_attempt` audit event with `cert_bytes_len` + `key_bytes_len` (no bytes leaked).
2. If `self.tls` is `None` (observer spawned without TLS at bind time): emit `tls_reload_failure` with `error_kind = "not_configured"` and return `TlsRotationError::NotConfigured`.
3. Compute the SHA-256 hex digest of the leaf cert's DER bytes (matches `openssl x509 -fingerprint -sha256 -noout -in cert.pem`).
4. Build a fresh `Arc<rustls::ServerConfig>` via `ServerTlsConfig::build_rotated_server_config`, **preserving the prior config's `alpn_protocols` list**. Failures map 1:1 to `TlsRotationError::{CertParse, EmptyCertChain, KeyParse, Rustls}`; on failure, emit `tls_reload_failure` with the typed `error_kind` and the error's display string as `detail`, and return.
5. Call `tls_handle.rotate(next_arc)`. Atomic; next-accept handshakes pick up the new identity.
6. Emit `tls_reload_success` with `leaf_fingerprint_sha256`.

**Failure semantics.** The rotation is gated on the new `ServerConfig` building cleanly. **Failure leaves the prior config serving** — operators can retry with a corrected secret without restarting the daemon. This is the design mitigation for the partial-write race (cert-manager writes cert before key, ACME writes key before cert): we read both files atomically at the caller, but if the bytes don't pair up the rotation is rejected, not partially applied.

## Audit event shape

All three rotation events ride the same `graphreact::observer::audit` target as the existing `observer listening` / `observer scrape` / `observer auth fail` events. JSON logs in production deploys (via `install_json_logs_subscriber` + `tracing_layer::TracingJsonLayer`) emit each event as a single line.

| Event | Level | Fields | Emitted by |
|---|---|---|---|
| `tls_reload_attempt` | INFO | `cert_bytes_len: usize`, `key_bytes_len: usize` | Entry to `reload_tls` (BEFORE parsing) |
| `tls_reloaded` | INFO | `leaf_fingerprint_sha256: &str` (lowercase hex SHA-256) | After successful `RotatableServerConfig::rotate` |
| `tls_reload_failure` | WARN | `error_kind: &'static str` ∈ {`cert_read`, `cert_parse`, `empty_chain`, `key_read`, `key_parse`, `rustls`, `not_configured`}, `detail: &str` (display of `TlsRotationError` or the I/O error) | After any failure point |

**Invariant: cert / key bytes are NEVER logged.** The `cert_bytes_len` / `key_bytes_len` fields on the attempt event give operators a sanity check that the supplied secret was non-empty, but the bytes themselves never enter the structured-field stream. The integration test [`tls_reload_audit_event_fires`](../../crates/graphreact-observer/tests/tls_rotation.rs) enforces this negatively — it scans every captured event for the substrings `-----BEGIN` and `PRIVATE KEY` and fails if either appears.

**Operator dashboard contract.** Counting `event=tls_reloaded` vs `event=tls_reload_failure` across a window gives the operator a rotation success ratio; alerting on the failure event with a per-`error_kind` split surfaces:

- `cert_read` / `key_read` — file vanished, permissions broke (EACCES), wrong FS mode → check cert-manager pod / file-system / mount points.
- `cert_parse` / `key_parse` — malformed PEM → check cert-manager wrote the file fully (incomplete write), check intermediate cert chain.
- `empty_chain` — cert file present but no PEM blocks parsed → check cert-manager wrote the right artifact (key file instead of cert).
- `rustls` — cert/key mismatch (different keypairs), or cert validity issue → check cert-manager renewed both in lock-step.
- `not_configured` — daemon shouldn't be receiving rotation triggers → check ops automation, daemon misconfiguration.

The `cert_read` / `key_read` split (Phase B Part 1 verify-finding patch 2026-06-18) distinguishes I/O failures from PEM-parse failures — they map to different remediation paths. Prior to the patch, both reported as `cert_parse` / `key_parse`, forcing operators to grep `detail` to disambiguate.

**Readiness line for SIGHUP**. TLS-configured daemons emit `grx-sighup-ready <pid>` on stdout after the SIGHUP signal stream is registered (Phase B Part 1 verify-finding patch). Operator tooling (cert-manager hooks, k8s preStop, integration tests) MUST wait for this line before firing the first SIGHUP — a SIGHUP arriving between `grx-observer-ready` and `grx-sighup-ready` would hit the kernel default action (terminate) instead of the rotation handler. Non-TLS daemons (`grx connect`, daemons without `--observer-cert`/`--observer-key`) do NOT emit `grx-sighup-ready` and do NOT install a SIGHUP handler — `kill -HUP` retains the kernel default (terminate) for those run modes.

## Cross-track contracts

| Sub-track | What it reuses | Phase A obligation |
|---|---|---|
| **#1 Auth & secrets** | The same `Arc<ArcSwap<_>>` shape will back rotatable bearer-token registries (`Arc<ArcSwap<BearerTokenRegistry>>`). | Phase A documents the pattern; sub-track #1 owns its own type. |
| **#4 Lifecycle** | ~~Phase B SIGHUP handler will hold a clone of `ApiServerHandle` and call `reload_tls` on signal.~~ **Phase B Part 1 SHIPPED 2026-06-18** — SIGHUP handler is co-located with #4's SIGTERM handler in `drive_handle_until`'s `tokio::select!` (one signal-kind per arm). | Phase A exposed `reload_tls` as a public API; sub-track #2 Phase B Part 1 owns the signal wiring; Windows fallback (named-event handle or REST `POST /v1/admin/reload-tls`) deferred to Phase B Part 2. |
| **#9 Chaos & fuzz** | A chaos arm fires `reload_tls(rcgen_minted_cert)` in a loop while the chaos harness's HTTP client scrapes `/v1/healthz`. Asserts no handshake fails mid-rotation. | Phase A's per-accept acceptor construction is the contract — sub-track #9 verifies it under load. |

## What Phase A does not ship

| Item | Reason | Lands in |
|---|---|---|
| ~~SIGHUP signal handler~~ | ~~Cross-platform signal handling is sub-track #4's domain. Phase A's `reload_tls` is the call-site target.~~ | **SHIPPED Phase B Part 1 2026-06-18** |
| Reload-on-file-change (`fsnotify`) | Filesystem watchers are platform-specific + the partial-write race lives at the FS layer; ACME / cert-manager are the canonical solutions. | Phase C |
| ACME / cert-manager integration | External cert lifecycle is a deployment concern, not a runtime concern. | Phase C |
| Dist crate mTLS rotation | The cluster transport holds BOTH server + client configs and coordinates handshake-in-flight on both sides of mTLS. Different invariants. | Separate track |
| Session-ticket rotation | Phase A keeps rustls's default session-ticket generator; clients holding tickets from the OLD `ServerConfig` will fall back to a full handshake post-rotation. Operators may see a transient handshake-CPU spike right after rotation. | Phase B Part 2 |
| Windows code path verification | Phase A is pure-Rust + portable; SIGHUP is the Unix-only platform seam. | Phase B Part 2 |

## References

- M-Prod master plan: see `[m-prod row in docs/backend/README.md](README.md)`.
- M16-Backend baseline TLS substrate: [`m16-backend-observer.md`](m16-backend-observer.md).
- Audit-target convention: same `graphreact::observer::audit` target used by every other observer audit event; see [`m-prod-observability.md`](m-prod-observability.md) for the JSON log stream contract.
- arc-swap crate: [`https://docs.rs/arc-swap`](https://docs.rs/arc-swap) — `ArcSwap::load_full` is the API we use; `Guard` (from `load`) is intentionally avoided because it is `!Send`.
