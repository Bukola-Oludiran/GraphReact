//! **M16-Backend Phase 2.** Observer HTTP-server bind configuration.
//!
//! Encodes the locked-in P2 plan rule: **127.0.0.1:9090 constructs
//! without flags; ANY other bind requires a token AND a TLS cert AND
//! a TLS key, all three together, all named explicitly via CLI
//! flags or env vars.** The construction error fires synchronously
//! on `BindConfig::from_flags`, so the operator sees the missing
//! flag immediately at process start rather than during the first
//! HTTP connect.
//!
//! ## Why synchronous validation
//!
//! A deferred check ("we'll validate when the server starts
//! listening") is harder to reason about: a daemon could be running
//! for minutes before its observer port even gets a connection,
//! during which a misconfigured remote bind without auth would be
//! quietly accepting unauthenticated requests. By failing
//! construction the moment the operator types `--observer-bind
//! 0.0.0.0:9090` without `--observer-cert`, the daemon refuses to
//! start, and the operator sees the error before any traffic is
//! routed.
//!
//! ## What "remote" means
//!
//! Loopback is `127.0.0.1`, `::1`, and `localhost`. Anything else —
//! including `0.0.0.0`, public IPs, private RFC1918 addresses, or
//! IPv6 ULAs — is treated as remote and triggers the
//! token+cert+key requirement. There is no opt-out: the substrate-
//! not-MVP commitment means we DON'T offer a flag to bind a remote
//! socket without auth, even for development. Use a loopback bind
//! + an SSH tunnel for dev-time scraping from another host.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use crate::auth::BearerToken;
use crate::tls::ServerTlsConfig;

/// Default observer bind address. 127.0.0.1:9090 — Prometheus's
/// conventional default scrape port, scoped to loopback so the
/// no-flags case is safe by construction.
pub const DEFAULT_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 9090);

/// Resolved observer-server bind configuration. Owns the
/// [`BearerToken`] (or `None` if unset) and the
/// [`ServerTlsConfig`] (or `None` if loopback-no-TLS). Constructed
/// via [`BindConfig::from_flags`].
pub struct BindConfig {
    /// Socket to bind. Defaults to [`DEFAULT_BIND`].
    pub addr: SocketAddr,
    /// Bearer token required on `/metrics` requests. `None` means
    /// no auth (loopback only).
    pub token: Option<BearerToken>,
    /// TLS configuration (cert + key). `None` means plaintext HTTP
    /// (loopback only).
    pub tls: Option<ServerTlsConfig>,
}

/// Reasons construction can fail. Each variant maps to a single
/// missing-flag scenario so the operator-visible error message
/// names exactly what to add.
#[derive(Debug, thiserror::Error)]
pub enum BindConfigError {
    /// `--observer-bind` is non-loopback but no token source was
    /// provided. Add `--observer-token-env GRX_OBSERVER_TOKEN` (or
    /// equivalent) to the CLI invocation.
    #[error(
        "remote observer bind {addr} requires authentication; pass \
         --observer-token-env <NAME> or --observer-token-file <PATH>"
    )]
    RemoteWithoutToken { addr: SocketAddr },

    /// Remote bind, token present, but no TLS cert. The plan refuses
    /// to serve plaintext + token over a non-loopback socket: the
    /// token would be visible to network observers between scraper
    /// and daemon.
    #[error(
        "remote observer bind {addr} requires TLS; pass \
         --observer-cert <PATH> + --observer-key <PATH>"
    )]
    RemoteWithoutCert { addr: SocketAddr },

    /// Remote bind, token + cert present, but no TLS key.
    #[error(
        "remote observer bind {addr} requires a TLS key; pass \
         --observer-key <PATH> alongside --observer-cert"
    )]
    RemoteWithoutKey { addr: SocketAddr },

    /// `--observer-token-env` and `--observer-token-file` both set;
    /// pick exactly one (the env var avoids a filesystem read; the
    /// file path is the systemd-secret-friendly path).
    #[error("observer token cannot be loaded from BOTH env and file; pick one")]
    TokenSourceConflict,

    /// Token loading itself failed (env unset, file missing, file
    /// mode insecure, etc.). Wraps the underlying error so the
    /// operator sees the specific cause.
    #[error("observer token load failed: {0}")]
    TokenLoad(#[source] crate::auth::AuthError),

    /// TLS construction failed (cert can't be parsed, key/cert
    /// mismatch, etc.). Wraps the underlying error.
    #[error("observer TLS configuration failed: {0}")]
    TlsLoad(#[source] crate::tls::TlsError),
}

/// Raw flag values as parsed by clap-derive. `None` for any unset
/// flag. [`BindConfig::from_flags`] cross-validates them against
/// the addr-shape rule.
#[derive(Default)]
pub struct BindFlags {
    pub addr: Option<SocketAddr>,
    pub token_env: Option<String>,
    pub token_file: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

impl BindConfig {
    /// Apply the addr-shape rules to a raw flag set + load the
    /// token + TLS material if specified. Synchronous (no async I/O).
    ///
    /// # The four rules
    ///
    /// 1. No `--observer-bind`: default to `127.0.0.1:9090`. Token,
    ///    cert, and key are optional; if any are passed, they MUST
    ///    all three be passed (we don't offer a "loopback + token
    ///    but no TLS" mode — TLS without auth or auth without TLS
    ///    is a footgun).
    /// 2. `--observer-bind` is loopback (`127.0.0.1`/`::1`): same
    ///    rule as (1).
    /// 3. `--observer-bind` is non-loopback: token, cert, and key
    ///    all REQUIRED. Missing any returns the matching
    ///    `BindConfigError` variant.
    /// 4. Token sources conflict (`--observer-token-env` AND
    ///    `--observer-token-file`): refuse at construction time.
    pub fn from_flags(flags: BindFlags) -> Result<Self, BindConfigError> {
        if flags.token_env.is_some() && flags.token_file.is_some() {
            return Err(BindConfigError::TokenSourceConflict);
        }

        let addr = flags.addr.unwrap_or(DEFAULT_BIND);
        let is_loopback = addr.ip().is_loopback();

        // Load the token (if any source is provided).
        let token = if let Some(env_name) = flags.token_env.as_deref() {
            Some(BearerToken::from_env(env_name).map_err(BindConfigError::TokenLoad)?)
        } else if let Some(path) = flags.token_file.as_deref() {
            Some(BearerToken::from_file(path).map_err(BindConfigError::TokenLoad)?)
        } else {
            None
        };

        // Load TLS (if both cert + key are present; one without the
        // other is rejected below).
        let tls = match (flags.cert.as_deref(), flags.key.as_deref()) {
            (Some(cert), Some(key)) => {
                Some(ServerTlsConfig::from_pem_files(cert, key).map_err(BindConfigError::TlsLoad)?)
            }
            (Some(_), None) | (None, Some(_)) => {
                // Cert without key (or vice-versa) is always
                // invalid, regardless of remote/loopback. Surface
                // the same error variant as the remote-bind case.
                return Err(BindConfigError::RemoteWithoutKey { addr });
            }
            (None, None) => None,
        };

        // Apply the remote-bind requirement.
        if !is_loopback {
            if token.is_none() {
                return Err(BindConfigError::RemoteWithoutToken { addr });
            }
            if tls.is_none() {
                return Err(BindConfigError::RemoteWithoutCert { addr });
            }
        }

        Ok(Self { addr, token, tls })
    }

    /// Default loopback config: `127.0.0.1:9090`, no auth, no TLS.
    /// Equivalent to `from_flags(BindFlags::default())`.
    #[must_use]
    pub fn default_loopback() -> Self {
        Self {
            addr: DEFAULT_BIND,
            token: None,
            tls: None,
        }
    }

    /// Whether this config is bound to a loopback address.
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        self.addr.ip().is_loopback()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> BindFlags {
        BindFlags::default()
    }

    #[test]
    fn defaults_to_loopback_9090_with_no_flags() {
        let cfg = BindConfig::from_flags(flags()).expect("default");
        assert_eq!(cfg.addr, DEFAULT_BIND);
        assert!(cfg.token.is_none());
        assert!(cfg.tls.is_none());
    }

    #[test]
    fn loopback_bind_does_not_require_token() {
        let cfg = BindConfig::from_flags(BindFlags {
            addr: Some("127.0.0.1:9091".parse().unwrap()),
            ..flags()
        })
        .expect("loopback no-auth ok");
        assert!(cfg.is_loopback());
    }

    #[test]
    fn ipv6_loopback_does_not_require_token() {
        let cfg = BindConfig::from_flags(BindFlags {
            addr: Some("[::1]:9091".parse().unwrap()),
            ..flags()
        })
        .expect("ipv6 loopback ok");
        assert!(cfg.is_loopback());
    }

    #[test]
    fn remote_without_token_refuses() {
        let err = BindConfig::from_flags(BindFlags {
            addr: Some("0.0.0.0:9090".parse().unwrap()),
            ..flags()
        })
        .err()
        .expect("expected error");
        assert!(matches!(err, BindConfigError::RemoteWithoutToken { .. }));
    }

    #[test]
    fn remote_without_cert_refuses_after_token() {
        std::env::set_var("GRX_OBSERVER_TEST_TOKEN_RWZBQ", "secret");
        let err = BindConfig::from_flags(BindFlags {
            addr: Some("0.0.0.0:9090".parse().unwrap()),
            token_env: Some("GRX_OBSERVER_TEST_TOKEN_RWZBQ".into()),
            ..flags()
        })
        .err()
        .expect("expected error");
        std::env::remove_var("GRX_OBSERVER_TEST_TOKEN_RWZBQ");
        assert!(
            matches!(err, BindConfigError::RemoteWithoutCert { .. }),
            "expected RemoteWithoutCert, got {err:?}"
        );
    }

    #[test]
    fn cert_without_key_refuses_even_on_loopback() {
        let err = BindConfig::from_flags(BindFlags {
            cert: Some(PathBuf::from("/tmp/nope.pem")),
            ..flags()
        })
        .err()
        .expect("expected error");
        assert!(matches!(err, BindConfigError::RemoteWithoutKey { .. }));
    }

    #[test]
    fn token_source_conflict_refuses() {
        let err = BindConfig::from_flags(BindFlags {
            token_env: Some("GRX_OBSERVER_TEST_TOKEN_X".into()),
            token_file: Some(PathBuf::from("/tmp/x")),
            ..flags()
        })
        .err()
        .expect("expected error");
        assert!(matches!(err, BindConfigError::TokenSourceConflict));
    }
}
