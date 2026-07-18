//! **M-Prod sub-track #2 Phase A.** Atomic-swap TLS rotation
//! substrate.
//!
//! The observer's HTTPS surface (the axum management API in
//! [`crate::management_api`] and the legacy hyper-1.x scrape server
//! in [`crate::http_server`]) historically loaded a single
//! `Arc<rustls::ServerConfig>` at bind time, then cloned it into a
//! `TlsAcceptor` once per spawn. That model wires the cert into the
//! daemon's lifetime — the only way to ship a new cert was to
//! restart the daemon, which severs every in-flight observer
//! scrape + management-API connection and breaks at-least-once
//! semantics for the management-API auto-correlation reply path
//! (M-Ingress phase 2).
//!
//! Phase A replaces that fixed `Arc<ServerConfig>` with a
//! `RotatableServerConfig`: a thin newtype around
//! `Arc<ArcSwap<Arc<rustls::ServerConfig>>>`. Reads (the per-accept
//! task) call [`RotatableServerConfig::current`] which loads the
//! current `Arc` and returns a cheap clone. Writes (the eventual
//! Phase-B SIGHUP handler / Phase-C ACME renewal path) call
//! [`RotatableServerConfig::rotate`] which atomically swaps in a
//! new `Arc<ServerConfig>`. Old `Arc<ServerConfig>` snapshots held
//! by in-flight handshakes stay alive until the per-connection
//! task drops — no use-after-free, no torn config.
//!
//! ## Why this shape (and not `ArcSwap<ServerConfig>`)
//!
//! rustls's `ServerConfig` is large enough that we never want to
//! clone it by value; the `Arc<ServerConfig>` indirection is the
//! conventional rustls handle shape (every public API consuming
//! a server config takes an `Arc<ServerConfig>`). `ArcSwap` on
//! sized `T` returns a `Guard<'_, Arc<T>>` from `.load()`, so
//! load-and-clone gives us back an `Arc<ServerConfig>` — exactly
//! what `tokio_rustls::TlsAcceptor::from` expects.
//!
//! ## Why per-accept, not per-spawn, `TlsAcceptor`
//!
//! `tokio_rustls::TlsAcceptor::from(Arc<ServerConfig>)` snapshots
//! the `Arc<ServerConfig>` at *construction* time. If we kept one
//! `TlsAcceptor` per spawn and rotated through `RotatableServerConfig`
//! underneath it, the old config would stay bound to the existing
//! acceptor and rotation would silently no-op. Phase A therefore
//! moves `TlsAcceptor` construction *inside* the per-connection
//! task (see [`crate::management_api`] accept loop) — each
//! handshake loads the current `Arc<ServerConfig>`, constructs a
//! fresh `TlsAcceptor`, and accepts the socket. The per-handshake
//! overhead is one `Arc::clone` (sub-µs) — negligible vs the
//! handshake itself (~5–50ms).
//!
//! ## Audit
//!
//! [`crate::audit::log_tls_reload_attempt`] /
//! [`crate::audit::log_tls_reload_success`] /
//! [`crate::audit::log_tls_reload_failure`] fire on every
//! [`ApiServerHandle::reload_tls`](crate::ApiServerHandle::reload_tls)
//! call. Cert + key bytes are NEVER logged — only the SHA-256
//! fingerprint of the leaf cert (first cert in the chain) goes in
//! the success record so operators can confirm a rotation actually
//! changed the served identity.
//!
//! ## What Phase A explicitly does NOT ship
//!
//! - SIGHUP handler (Phase B). Phase A exposes the rotation
//!   substrate via [`crate::ApiServerHandle::reload_tls`]; the
//!   signal-handler wiring is owned by M-Prod sub-track #4
//!   (lifecycle).
//! - ACME renewal (Phase C). External cert-manager / certbot
//!   integration is out of scope.
//! - dist mTLS rotation. The cluster-transport surface in
//!   `crates/graphreact-dist` has its own mTLS path (server +
//!   client configs both) and is intentionally untouched.
//! - Session-ticket rotation. Clients holding a session ticket
//!   from the old `ServerConfig` will fall back to a full
//!   handshake against the new config (correct + expected for
//!   Phase A; ticket rotation is Phase B).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use arc_swap::ArcSwap;
use rustls::server::ServerConfig;

/// Atomic-swap container for an `Arc<rustls::ServerConfig>`. Cheap
/// to read on the per-connection hot path (one `Arc::clone`); the
/// rotation entry point ([`Self::rotate`]) replaces the inner
/// `Arc` atomically so in-flight handshakes continue to use their
/// existing snapshot.
///
/// Cloning a `RotatableServerConfig` clones the inner `Arc<ArcSwap<_>>`
/// — every clone observes the same rotation. This is the intended
/// shape: the bind config, the management-API accept loop, the
/// legacy `/metrics` accept loop, and the management-API handle's
/// [`ApiServerHandle::reload_tls`](crate::ApiServerHandle::reload_tls)
/// entry point all hold clones of the same handle.
#[derive(Clone)]
pub struct RotatableServerConfig {
    inner: Arc<ArcSwap<ServerConfig>>,
}

/// Shared readiness state for TLS reloads. A counter (rather than a
/// boolean) permits an outer file-read guard and the inner config-swap
/// guard to nest without briefly reporting ready between them.
#[derive(Clone, Default)]
pub struct TlsReadiness {
    rotations_in_flight: Arc<AtomicUsize>,
}

impl TlsReadiness {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin_rotation(&self) -> TlsRotationGuard {
        self.rotations_in_flight.fetch_add(1, Ordering::AcqRel);
        TlsRotationGuard {
            state: self.clone(),
        }
    }

    #[must_use]
    pub fn is_rotating(&self) -> bool {
        self.rotations_in_flight.load(Ordering::Acquire) > 0
    }
}

#[must_use = "dropping the guard ends the TLS rotation readiness window"]
pub struct TlsRotationGuard {
    state: TlsReadiness,
}

impl Drop for TlsRotationGuard {
    fn drop(&mut self) {
        let previous = self
            .state
            .rotations_in_flight
            .fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "unbalanced TLS rotation readiness guard");
    }
}

impl RotatableServerConfig {
    /// Wrap an initial `Arc<ServerConfig>` for rotation.
    ///
    /// The first `current()` call returns the supplied config; the
    /// first `rotate(...)` call replaces it.
    #[must_use]
    pub fn new(initial: Arc<ServerConfig>) -> Self {
        Self {
            inner: Arc::new(ArcSwap::new(initial)),
        }
    }

    /// Load the current `Arc<ServerConfig>`. Cheap (one
    /// `ArcSwap::load_full`); returns an owned `Arc` that is safe
    /// to hold across `.await` (unlike `ArcSwap::load`'s `Guard`,
    /// which is not-`Send`).
    ///
    /// Callers in the per-connection accept path use this to build
    /// a fresh `tokio_rustls::TlsAcceptor` per handshake — see the
    /// module-level docs for why this is required (vs caching the
    /// acceptor at spawn time).
    #[must_use]
    pub fn current(&self) -> Arc<ServerConfig> {
        self.inner.load_full()
    }

    /// Atomically swap in a new `Arc<ServerConfig>`. The next
    /// `current()` call observes `next`; existing snapshots held
    /// by in-flight handshakes keep serving the old config until
    /// they drop.
    pub fn rotate(&self, next: Arc<ServerConfig>) {
        self.inner.store(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::ALL_VERSIONS;

    fn build_config() -> Arc<ServerConfig> {
        let mut params =
            rcgen::CertificateParams::new(vec!["rotate.test".into()]).unwrap();
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            "rotate.test".to_string().try_into().unwrap(),
        ));
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let cert_der: CertificateDer<'static> = cert.der().clone();
        let key_der: PrivateKeyDer<'static> =
            PrivatePkcs8KeyDer::from(key.serialize_der()).into();

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(ALL_VERSIONS)
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        Arc::new(cfg)
    }

    #[test]
    fn current_returns_initial() {
        let cfg_a = build_config();
        let rot = RotatableServerConfig::new(cfg_a.clone());
        assert!(Arc::ptr_eq(&rot.current(), &cfg_a));
    }

    #[test]
    fn rotate_swaps_in_new_config() {
        let cfg_a = build_config();
        let cfg_b = build_config();
        let rot = RotatableServerConfig::new(cfg_a.clone());

        // Held snapshot pre-rotation observes A.
        let snap = rot.current();
        assert!(Arc::ptr_eq(&snap, &cfg_a));

        rot.rotate(cfg_b.clone());

        // New `current()` observes B.
        assert!(Arc::ptr_eq(&rot.current(), &cfg_b));
        // Pre-rotation snapshot still points at A — no UAF.
        assert!(Arc::ptr_eq(&snap, &cfg_a));
    }

    #[test]
    fn clone_observes_rotation() {
        let cfg_a = build_config();
        let cfg_b = build_config();
        let rot1 = RotatableServerConfig::new(cfg_a);
        let rot2 = rot1.clone();
        rot1.rotate(cfg_b.clone());
        assert!(Arc::ptr_eq(&rot2.current(), &cfg_b));
    }

    #[test]
    fn readiness_tracks_nested_rotation_guards() {
        let readiness = TlsReadiness::new();
        assert!(!readiness.is_rotating());
        let outer = readiness.begin_rotation();
        assert!(readiness.is_rotating());
        {
            let _inner = readiness.begin_rotation();
            assert!(readiness.is_rotating());
        }
        assert!(readiness.is_rotating());
        drop(outer);
        assert!(!readiness.is_rotating());
    }
}
