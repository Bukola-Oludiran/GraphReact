//! **M16-Backend Phase 2.** Server-only rustls configuration for the
//! observer HTTP server.
//!
//! Mirrors the `with_no_client_auth().with_single_cert(...)` pattern
//! from `crates/graphreact-dist/src/tls_config.rs:83-87`. The dist
//! crate runs mTLS by default for cluster traffic; the observer
//! server is a *scrape endpoint* — Prometheus / curl / k6 connect
//! as anonymous clients with only their bearer token, so client
//! auth via cert is intentionally NOT required. (Operators who
//! want client certs in addition can layer a reverse proxy in
//! front; we keep the substrate simple here.)
//!
//! ## What the rustls builder gives us
//!
//! - `ServerConfig::builder_with_provider(ring)` — pinned to the
//!   ring crypto provider, matching the dist crate so we don't ship
//!   two crypto backends.
//! - `with_protocol_versions(ALL_VERSIONS)` — TLS 1.2 and 1.3.
//! - `with_no_client_auth()` — anonymous clients OK; auth is the
//!   bearer token's job.
//! - `with_single_cert(cert_chain, key)` — one server identity.
//!
//! Scaling concerns (ALPN, multi-cert, OCSP stapling) are out of
//! scope for P2; this is the minimal substrate for a scrape
//! endpoint.

use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConfig;
use rustls::ALL_VERSIONS;

use crate::tls_rotation::RotatableServerConfig;

/// TLS configuration errors. Each variant maps to a specific
/// failure mode the operator can act on.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// Cert file missing or unreadable.
    #[error("observer TLS cert {path}: {detail}")]
    CertRead { path: String, detail: String },

    /// Key file missing or unreadable.
    #[error("observer TLS key {path}: {detail}")]
    KeyRead { path: String, detail: String },

    /// PEM parse failure (cert or key).
    #[error("observer TLS PEM parse failed for {what}: {detail}")]
    PemParse { what: &'static str, detail: String },

    /// rustls construction failure (cert/key mismatch, bad ASN.1,
    /// unsupported algorithm, etc.).
    #[error("observer TLS rustls config: {detail}")]
    Rustls { detail: String },

    /// Cert chain was empty (the file parsed cleanly but contained
    /// no `BEGIN CERTIFICATE` blocks).
    #[error("observer TLS cert {path}: chain is empty")]
    EmptyCertChain { path: String },
}

/// **M-Prod sub-track #2 Phase A.** Errors raised by the
/// in-process TLS rotation path
/// ([`crate::ApiServerHandle::reload_tls`](crate::ApiServerHandle::reload_tls)).
///
/// Distinct from [`TlsError`] (which covers the file-loading + initial
/// `ServerConfig` build at daemon-start) because the rotation entry
/// point takes already-decoded PEM bytes (cert-manager / k8s secret /
/// keychain), surfaces a `NotConfigured` variant for the
/// "observer was spawned without TLS" case, and never touches the
/// filesystem.
#[derive(Debug, thiserror::Error)]
pub enum TlsRotationError {
    /// PEM-decoded cert bytes did not parse as a `CERTIFICATE` chain.
    /// Includes the underlying `rustls_pki_types::pem::Error` detail
    /// so operators can distinguish "wrong encoding" from "key bytes
    /// pasted into the cert slot".
    #[error("observer TLS rotation: cert PEM parse failed: {detail}")]
    CertParse { detail: String },

    /// Cert bytes parsed cleanly but yielded zero `BEGIN CERTIFICATE`
    /// blocks. rustls would later panic with `EmptyCertChain`; we
    /// reject early so the rotation is a no-op + the old config keeps
    /// serving.
    #[error("observer TLS rotation: cert chain is empty")]
    EmptyCertChain,

    /// PEM-decoded key bytes did not parse as a recognized private
    /// key format (PKCS#8, RSA, SEC1). Same partial-write race
    /// mitigation as `CertParse` — the failure leaves the prior
    /// `ServerConfig` in place.
    #[error("observer TLS rotation: key PEM parse failed: {detail}")]
    KeyParse { detail: String },

    /// rustls rejected the cert+key pair when constructing the new
    /// `ServerConfig` (typically a mismatched cert/key, unsupported
    /// algorithm, or the cryptoprovider builder rejecting the chosen
    /// `ALL_VERSIONS` set).
    #[error("observer TLS rotation: rustls config build failed: {detail}")]
    Rustls { detail: String },

    /// The observer was spawned without TLS configured at bind time
    /// (loopback default). Phase A does not allow rotating from
    /// plaintext to TLS in-place — operators must restart the daemon
    /// with `--observer-cert` + `--observer-key`. (Phase C revisits
    /// this if ACME auto-provisioning lands.)
    #[error("observer TLS rotation: server not configured for TLS at bind time")]
    NotConfigured,
}

/// Server-only TLS config wrapping the rustls `ServerConfig` in a
/// rotatable handle (see [`RotatableServerConfig`]). The HTTP server
/// constructs a fresh `tokio_rustls::TlsAcceptor` per incoming
/// connection by calling `tls.server.current()` — that way a
/// rotation between accepts immediately surfaces the new cert on
/// the next handshake, while in-flight handshakes keep their own
/// `Arc<ServerConfig>` clone alive until they complete.
#[derive(Clone)]
pub struct ServerTlsConfig {
    pub server: RotatableServerConfig,
}

impl ServerTlsConfig {
    /// Load cert + key from PEM files on disk. Refuses empty cert
    /// chains; rustls rejects mismatched cert/key pairs internally.
    pub fn from_pem_files(cert_path: &Path, key_path: &Path) -> Result<Self, TlsError> {
        let cert_chain = load_cert_chain(cert_path)?;
        let private_key = load_private_key(key_path)?;
        Self::from_pem_bytes(cert_chain, private_key)
    }

    /// Build from already-decoded PEM material. Useful in tests
    /// (rcgen-generated certs) and for callers that want to load
    /// cert/key from a non-file source (k8s secrets via env, etc.).
    pub fn from_pem_bytes(
        cert_chain: Vec<CertificateDer<'static>>,
        private_key: PrivateKeyDer<'static>,
    ) -> Result<Self, TlsError> {
        if cert_chain.is_empty() {
            return Err(TlsError::EmptyCertChain {
                path: "<bytes>".into(),
            });
        }
        let server = build_server_config(cert_chain, private_key).map_err(|detail| {
            TlsError::Rustls { detail }
        })?;
        Ok(Self {
            server: RotatableServerConfig::new(Arc::new(server)),
        })
    }

    /// **M-Prod sub-track #2 Phase A.** Build a fresh
    /// `Arc<rustls::ServerConfig>` from PEM-decoded cert + key bytes,
    /// preserving the `alpn_protocols` list from the supplied
    /// `template`. Operators should NOT have to re-specify ALPN per
    /// rotation; the existing config is the source of truth.
    ///
    /// Returns the new `Arc<ServerConfig>` ready to feed into
    /// [`RotatableServerConfig::rotate`]. Does NOT touch the live
    /// handle — the caller (typically
    /// [`crate::ApiServerHandle::reload_tls`](crate::ApiServerHandle::reload_tls))
    /// is responsible for ordering the rotation + the audit-log
    /// emission.
    pub fn build_rotated_server_config(
        cert_pem: &[u8],
        key_pem: &[u8],
        template: &ServerConfig,
    ) -> Result<Arc<ServerConfig>, TlsRotationError> {
        let cert_chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| TlsRotationError::CertParse {
                detail: e.to_string(),
            })?;
        if cert_chain.is_empty() {
            return Err(TlsRotationError::EmptyCertChain);
        }
        let private_key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| {
            TlsRotationError::KeyParse {
                detail: e.to_string(),
            }
        })?;
        let mut next = build_server_config(cert_chain, private_key)
            .map_err(|detail| TlsRotationError::Rustls { detail })?;
        // Preserve the operator's ALPN configuration through the
        // rotation. Phase A does not surface a knob to change ALPN
        // mid-rotation; operators who need that restart the daemon
        // with the new flags.
        next.alpn_protocols = template.alpn_protocols.clone();
        Ok(Arc::new(next))
    }
}

/// Internal helper: run the rustls builder pipeline against a fixed
/// `(cert_chain, private_key)` pair. Shared between the initial
/// daemon-start path (`from_pem_bytes`) and the rotation path
/// (`build_rotated_server_config`) so the two surfaces don't drift
/// on crypto-provider / protocol-version choices.
fn build_server_config(
    cert_chain: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
) -> Result<ServerConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(ALL_VERSIONS)
        .map_err(|e| format!("protocol versions: {e}"))?
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .map_err(|e| format!("with_single_cert: {e}"))
}

fn load_cert_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let bytes = std::fs::read(path).map_err(|e| TlsError::CertRead {
        path: path.display().to_string(),
        detail: format!("read: {e}"),
    })?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::PemParse {
            what: "cert chain",
            detail: e.to_string(),
        })?;
    if certs.is_empty() {
        return Err(TlsError::EmptyCertChain {
            path: path.display().to_string(),
        });
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let bytes = std::fs::read(path).map_err(|e| TlsError::KeyRead {
        path: path.display().to_string(),
        detail: format!("read: {e}"),
    })?;
    PrivateKeyDer::from_pem_slice(&bytes).map_err(|e| TlsError::PemParse {
        what: "private key",
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a self-signed cert + key in memory using rcgen,
    /// then load it via `from_pem_bytes`. Smoke test confirming
    /// the rustls path accepts a fresh self-signed identity (the
    /// shape every observer-server test will use).
    #[test]
    fn loads_self_signed_cert_via_rcgen() {
        use rustls::pki_types::PrivatePkcs8KeyDer;

        let mut params =
            rcgen::CertificateParams::new(vec!["observer.local".into()]).unwrap();
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            "observer.local".to_string().try_into().unwrap(),
        ));
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();

        let cert_der: CertificateDer<'static> = cert.der().clone();
        let key_der: PrivateKeyDer<'static> =
            PrivatePkcs8KeyDer::from(key.serialize_der()).into();

        let cfg = ServerTlsConfig::from_pem_bytes(vec![cert_der], key_der).unwrap();
        // Sanity: the rustls provider produced a usable config that
        // we can pull through the rotation handle.
        let snapshot = cfg.server.current();
        assert!(Arc::strong_count(&snapshot) >= 1);
    }

    #[test]
    fn empty_cert_chain_rejected() {
        // Constructing with an empty chain is a clear configuration
        // bug — rustls would later panic, so we surface it early.
        use rustls::pki_types::PrivatePkcs8KeyDer;
        let key = rcgen::KeyPair::generate().unwrap();
        let key_der: PrivateKeyDer<'static> =
            PrivatePkcs8KeyDer::from(key.serialize_der()).into();
        let err = ServerTlsConfig::from_pem_bytes(vec![], key_der)
            .err()
            .expect("expected error");
        assert!(matches!(err, TlsError::EmptyCertChain { .. }));
    }
}
