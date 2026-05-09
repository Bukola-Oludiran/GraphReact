//! TLS configuration helpers.
//!
//! Every cluster member holds:
//! - Its own cert chain + private key (the node's identity).
//! - The cluster's root CA (trust anchor used to validate peer
//!   certs).
//! - mTLS: required by default — both sides authenticate via cert.
//!   `require_mtls = false` is a dev-mode escape hatch and emits a
//!   warning at `Cluster::new` time (lands in M14.3).
//!
//! Production configs are loaded from PEM files via
//! [`TlsConfig::from_pem_bytes`]. Test-mode configs are generated
//! via [`crate::dev_tls`] (only compiled under `#[cfg(test)]`; rcgen
//! is a `dev-dependencies` entry).

use rustls::client::ClientConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ServerConfig, WebPkiClientVerifier};
use rustls::{ALL_VERSIONS, RootCertStore};
use std::sync::Arc;

use crate::DistError;

/// Bundled server + client TLS configuration for a cluster member.
///
/// Both sides are populated from the same identity material so the
/// node can act as a TLS server (accepting inbound peers) and a TLS
/// client (dialling outbound peers) without duplicating cert
/// loading.
#[derive(Clone)]
pub struct TlsConfig {
    pub server: Arc<ServerConfig>,
    pub client: Arc<ClientConfig>,
    pub require_mtls: bool,
}

impl TlsConfig {
    /// Build a `TlsConfig` from already-decoded PEM material.
    ///
    /// `cert_chain` is the leaf cert followed by any intermediates
    /// (the order rustls expects on the wire). `private_key` matches
    /// the leaf cert. `root_ca` is the trust anchor — typically the
    /// cluster's self-signed CA cert.
    ///
    /// `require_mtls = true` (recommended) makes the server demand
    /// and verify a client cert chained to `root_ca`. `false` runs
    /// server-auth-only TLS, appropriate ONLY for development.
    pub fn from_pem_bytes(
        cert_chain: Vec<CertificateDer<'static>>,
        private_key: PrivateKeyDer<'static>,
        root_ca: Vec<CertificateDer<'static>>,
        require_mtls: bool,
    ) -> Result<Self, DistError> {
        if cert_chain.is_empty() {
            return Err(DistError::Tls("empty cert chain".into()));
        }

        let mut roots = RootCertStore::empty();
        for ca in root_ca {
            roots
                .add(ca)
                .map_err(|e| DistError::Tls(format!("invalid root cert: {e}")))?;
        }

        let provider = Arc::new(rustls::crypto::ring::default_provider());

        // Server side.
        let server_builder = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(ALL_VERSIONS)
            .map_err(|e| DistError::Tls(format!("server protocol versions: {e}")))?;
        let server = if require_mtls {
            let verifier = WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots.clone()),
                provider.clone(),
            )
            .build()
            .map_err(|e| DistError::Tls(format!("client verifier: {e}")))?;
            server_builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(cert_chain.clone(), private_key.clone_key())
                .map_err(|e| DistError::Tls(format!("server cert: {e}")))?
        } else {
            server_builder
                .with_no_client_auth()
                .with_single_cert(cert_chain.clone(), private_key.clone_key())
                .map_err(|e| DistError::Tls(format!("server cert: {e}")))?
        };

        // Client side.
        let client = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(ALL_VERSIONS)
            .map_err(|e| DistError::Tls(format!("client protocol versions: {e}")))?
            .with_root_certificates(roots)
            .with_client_auth_cert(cert_chain, private_key)
            .map_err(|e| DistError::Tls(format!("client cert: {e}")))?;

        Ok(Self {
            server: Arc::new(server),
            client: Arc::new(client),
            require_mtls,
        })
    }
}
