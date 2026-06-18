//! **M16-Backend Phase 2.** Per-connection audit logging for the
//! observer HTTP server.
//!
//! The plan ships audit on EVERY accepted connection regardless of
//! whether the bind is loopback or remote. Substrate-not-MVP: a
//! half-on auditor that's only active on remote binds is the kind
//! of thing operators forget about and security audits flag.
//! Loopback connections still get logged — they're cheap, they don't
//! fire often, and operators get a uniform view of "who scraped my
//! observer port" via `journalctl -u grx --grep
//! 'graphreact::observer::audit'`.
//!
//! ## What gets logged
//!
//! - On accept: peer SocketAddr + (optionally) the path being
//!   requested. The path is logged AFTER the request line is
//!   parsed so we don't accidentally leak `?token=...` query
//!   params into audit (P2 doesn't accept query-param auth, but
//!   defense in depth).
//! - On auth-fail: peer + reason ("missing", "wrong"). The token
//!   itself is NEVER logged.
//! - On 200 response: peer + status + bytes (`/metrics` body size
//!   gives operators a rough metric-volume sense).
//!
//! Format: structured `tracing::info!` with
//! `target: "graphreact::observer::audit"`. Operators can pipe this
//! to a separate log file via tracing-subscriber filtering, or
//! ship to syslog via `tracing-journald`.

use std::net::SocketAddr;

/// Log a successful per-connection accept.
#[inline]
pub fn log_accept(peer: SocketAddr, path: &str) {
    tracing::info!(
        target: "graphreact::observer::audit",
        %peer,
        path = %path,
        "observer scrape"
    );
}

/// Log an auth-failure event. `reason` is one of `"missing"` (no
/// `Authorization` header) or `"wrong"` (header present but token
/// didn't match). The token itself is NEVER included.
#[inline]
pub fn log_auth_fail(peer: SocketAddr, reason: &'static str) {
    tracing::warn!(
        target: "graphreact::observer::audit",
        %peer,
        reason,
        "observer auth fail"
    );
}

/// Log a successful 200 response with byte count. Body size is the
/// closest cheap proxy for "scrape volume" operators have until
/// Phase 5 adds a `grx_observer_scrape_bytes_total` self-metric.
#[inline]
pub fn log_response(peer: SocketAddr, path: &str, status: u16, bytes: usize) {
    tracing::info!(
        target: "graphreact::observer::audit",
        %peer,
        path = %path,
        status,
        bytes,
        "observer response"
    );
}

/// Log a server bind. Fired once per daemon start when the listener
/// successfully accepts on the configured address.
#[inline]
pub fn log_bind(addr: SocketAddr, tls: bool, auth: bool) {
    tracing::info!(
        target: "graphreact::observer::audit",
        %addr,
        tls,
        auth,
        "observer listening"
    );
}

/// **M-Prod sub-track #2 Phase A.** TLS rotation attempt entered the
/// daemon. Fired at the top of
/// [`crate::ApiServerHandle::reload_tls`](crate::ApiServerHandle::reload_tls)
/// BEFORE PEM parsing — pairs with either
/// [`log_tls_reload_success`] (rotation completed) or
/// [`log_tls_reload_failure`] (parse / build / not-configured).
///
/// `cert_bytes_len` / `key_bytes_len` give operators a sanity check
/// that the supplied secret was non-empty without ever leaking the
/// material itself.
#[inline]
pub fn log_tls_reload_attempt(cert_bytes_len: usize, key_bytes_len: usize) {
    tracing::info!(
        target: "graphreact::observer::audit",
        event = "tls_reload_attempt",
        cert_bytes_len,
        key_bytes_len,
        "observer TLS rotation attempt"
    );
}

/// **M-Prod sub-track #2 Phase A.** TLS rotation completed; the
/// next accepted connection serves the new leaf cert. The leaf
/// fingerprint is the SHA-256 hex digest of the first cert in the
/// chain — operators can cross-reference against `openssl x509
/// -noout -fingerprint -sha256 -in cert.pem` to confirm the
/// rotation took. Cert / key bytes are NEVER logged.
#[inline]
pub fn log_tls_reload_success(leaf_fingerprint_sha256: &str) {
    tracing::info!(
        target: "graphreact::observer::audit",
        event = "tls_reloaded",
        leaf_fingerprint_sha256,
        "observer TLS rotation success"
    );
}

/// **M-Prod sub-track #2 Phase A.** TLS rotation failed; the old
/// `ServerConfig` keeps serving (rotation is gated on the new
/// config building cleanly). `error_kind` is one of
/// `"cert_parse"`, `"empty_chain"`, `"key_parse"`, `"rustls"`,
/// `"not_configured"`; `detail` carries the underlying message
/// from [`crate::tls::TlsRotationError`] for operator-visible
/// triage. Cert / key bytes are NEVER logged.
#[inline]
pub fn log_tls_reload_failure(error_kind: &'static str, detail: &str) {
    tracing::warn!(
        target: "graphreact::observer::audit",
        event = "tls_reload_failure",
        error_kind,
        detail,
        "observer TLS rotation failure"
    );
}

/// **M-Prod sub-track #4 Phase α.** SIGTERM (or operator-driven)
/// drain entered. Emitted EXACTLY ONCE per daemon lifetime,
/// BEFORE [`crate::DrainFlag`]'s AtomicBool flip becomes
/// observable from other threads — so a `/v1/readyz` scrape racing
/// the flip can't see `ready=true` and `no drain_started in log`.
///
/// `deadline_ms` records the outer drain budget (the
/// `--drain-deadline-secs` CLI flag / `GRX_DRAIN_DEADLINE_SECS`
/// env var) so operators see the configured bound at drain-start
/// without grepping the CLI flag history.
#[inline]
pub fn log_drain_started(deadline_ms: u64) {
    tracing::info!(
        target: "graphreact::observer::audit",
        event = "drain_started",
        deadline_ms,
        "observer drain started"
    );
}

/// **M-Prod sub-track #4 Phase α.** Drain completed inside the
/// deadline budget. `in_flight_settled` is `true` when both
/// `PendingCalls::is_empty()` and the ingress-in-flight counter
/// hit zero before deadline; `cluster_quiesced` is `true` when the
/// cluster's handshake-pending population settled (Phase α reports
/// `true` unconditionally — handshake-quiescence is deferred to
/// Phase β; see `m-prod-lifecycle.md` for why).
///
/// `duration_ms` is the wall-clock span between
/// [`crate::DrainFlag::start`] returning `true` and this emit.
#[inline]
pub fn log_drain_completed(
    duration_ms: u64,
    in_flight_settled: bool,
    cluster_quiesced: bool,
) {
    tracing::info!(
        target: "graphreact::observer::audit",
        event = "drain_completed",
        duration_ms,
        in_flight_settled,
        cluster_quiesced,
        "observer drain completed"
    );
}

/// **M-Prod sub-track #4 Phase α.** Drain budget expired with
/// traffic still in flight. `pending_calls_remaining` is the
/// `PendingCalls::len()` count at expiry;
/// `ingress_in_flight_remaining` is the
/// `IngressInFlight::load()` count. The daemon proceeds to
/// hard-shutdown anyway (the calling orchestrator runs
/// `DaemonHandle::shutdown` right after) so cancelled axum
/// futures surface as connection-reset to the client — operators
/// see the count here to correlate against their LB's in-flight
/// metric. Matches `otel_shutdown_timeout`'s precedent shape
/// (WARN level, deadline_ms field).
#[inline]
pub fn log_drain_deadline_exceeded(
    deadline_ms: u64,
    duration_ms: u64,
    pending_calls_remaining: usize,
    ingress_in_flight_remaining: usize,
) {
    tracing::warn!(
        target: "graphreact::observer::audit",
        event = "drain_deadline_exceeded",
        deadline_ms,
        duration_ms,
        pending_calls_remaining,
        ingress_in_flight_remaining,
        "observer drain exceeded deadline"
    );
}
