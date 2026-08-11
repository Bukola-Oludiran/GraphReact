//! **M16-Backend Phase 5+6.** Tiny hand-rolled HTTP/1.1 client for the
//! `grx ps / tree / top / trace / observer` subcommands. Talks to a
//! running daemon's management API (P4) over plain TCP or rustls TLS.
//!
//! ## Why hand-rolled
//!
//! `reqwest` would add ~5MB + ~40 transitive deps for a use-case
//! that needs exactly: GET, optional bearer auth, optional TLS,
//! parse JSON response. The substrate-not-MVP framing means we
//! own the wire path here — same shape as the test helpers in
//! `crates/graphreact-observer/tests/`.
//!
//! ## Auth + URL parsing
//!
//! - `--target <addr>`: `host:port` or `https://host:port`.
//! - `--token-env <NAME>`: read bearer token from env var.
//! - `--token-file <PATH>`: read bearer token from file (mode-0600
//!   enforced, mirrors `BearerToken::from_file`).
//!
//! Without TLS the URL is `http://`; with TLS it's `https://`. P6
//! adds the rustls TLS path via [`tokio_rustls::TlsConnector`] using
//! the system trust store from `rustls-native-certs`. The
//! `--insecure-no-tls` flag (exposed only on `grx observer`) disables
//! peer cert verification — DEV ONLY; production binds must use a
//! real CA-signed cert.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Error)]
pub enum ObserverClientError {
    #[error("tcp connect to {addr} failed: {detail}")]
    Connect { addr: String, detail: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("response framing missing CRLF CRLF")]
    Framing,
    #[error("HTTP status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("token load: {0}")]
    TokenLoad(String),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("response is not valid UTF-8")]
    NotUtf8,
    #[error("response timeout")]
    Timeout,
    #[error("tls setup: {0}")]
    Tls(String),
}

/// Connection target — the management API URL prefix the daemon
/// is bound on. Constructed from a CLI string like
/// `127.0.0.1:9090` (defaults to plain HTTP).
#[derive(Debug, Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl Target {
    pub fn parse(spec: &str) -> Result<Self, ObserverClientError> {
        let (rest, tls) = if let Some(s) = spec.strip_prefix("https://") {
            (s, true)
        } else if let Some(s) = spec.strip_prefix("http://") {
            (s, false)
        } else {
            (spec, false)
        };
        // Strip any trailing path; we always issue against `/v1/...`.
        let rest = rest.split('/').next().unwrap_or(rest);
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => {
                let port: u16 = p
                    .parse()
                    .map_err(|e| ObserverClientError::InvalidUrl(format!("port `{p}`: {e}")))?;
                (h.to_string(), port)
            }
            None => {
                return Err(ObserverClientError::InvalidUrl(format!(
                    "expected `host:port`, got `{spec}`"
                )))
            }
        };
        Ok(Self { host, port, tls })
    }

    fn host_port(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Resolve the bearer token from a CLI flag set (env var name or
/// file path). Returns `None` if neither is set.
pub fn resolve_token(
    env_name: Option<&str>,
    file_path: Option<&Path>,
) -> Result<Option<String>, ObserverClientError> {
    match (env_name, file_path) {
        (Some(_), Some(_)) => Err(ObserverClientError::TokenLoad(
            "pass either --token-env OR --token-file, not both".into(),
        )),
        (Some(name), None) => match std::env::var(name) {
            Ok(s) if s.is_empty() => Err(ObserverClientError::TokenLoad(format!(
                "env var `{name}` is empty"
            ))),
            Ok(s) => Ok(Some(s)),
            Err(e) => Err(ObserverClientError::TokenLoad(format!(
                "env var `{name}`: {e}"
            ))),
        },
        (None, Some(path)) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let meta = std::fs::metadata(path)
                    .map_err(|e| ObserverClientError::TokenLoad(format!("stat: {e}")))?;
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    return Err(ObserverClientError::TokenLoad(format!(
                        "token file mode {mode:#o} (must be 0600)"
                    )));
                }
            }
            let mut bytes = std::fs::read(path)
                .map_err(|e| ObserverClientError::TokenLoad(format!("read: {e}")))?;
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
            }
            let s = String::from_utf8(bytes).map_err(|_| ObserverClientError::NotUtf8)?;
            Ok(Some(s))
        }
        (None, None) => Ok(None),
    }
}

/// TLS verification policy for the observer client. P6 ships two
/// modes; cert-pinning lands when the canonical-plan acceptance
/// gate calls for it.
#[derive(Debug, Clone, Copy, Default)]
pub enum TlsMode {
    /// Use the system trust store (rustls-native-certs). The default.
    #[default]
    Native,
    /// **DEV ONLY.** Skip peer-cert verification. Surfaced through
    /// `grx observer --insecure-no-tls`.
    InsecureSkipVerify,
}

/// Issue a single GET against the management API + return the
/// response body as a `String`. Body must be UTF-8. Returns an
/// `Err` on any non-2xx status. Defaults to native TLS verification
/// when the target is `https://`.
pub async fn get(
    target: &Target,
    path: &str,
    token: Option<&str>,
) -> Result<String, ObserverClientError> {
    get_with_tls(target, path, token, TlsMode::default()).await
}

/// `get` with explicit TLS-mode selection. Used by `grx observer
/// --insecure-no-tls` to opt into the no-verify code path during
/// development scrapes against self-signed certs.
pub async fn get_with_tls(
    target: &Target,
    path: &str,
    token: Option<&str>,
    tls_mode: TlsMode,
) -> Result<String, ObserverClientError> {
    let host_port = target.host_port();
    let req_bytes = build_request(&host_port, path, token);

    let raw = if target.tls {
        tls_round_trip(target, &host_port, &req_bytes, tls_mode).await?
    } else {
        plain_round_trip(&host_port, &req_bytes).await?
    };

    parse_response(&raw)
}

fn build_request(host_port: &str, path: &str, token: Option<&str>) -> Vec<u8> {
    let auth_line = match token {
        Some(t) => format!("Authorization: Bearer {t}\r\n"),
        None => String::new(),
    };
    format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n{auth_line}\r\n")
        .into_bytes()
}

async fn plain_round_trip(
    host_port: &str,
    req_bytes: &[u8],
) -> Result<Vec<u8>, ObserverClientError> {
    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(host_port))
        .await
        .map_err(|_| ObserverClientError::Timeout)?
        .map_err(|e| ObserverClientError::Connect {
            addr: host_port.to_string(),
            detail: e.to_string(),
        })?;
    write_request_and_read(&mut stream, req_bytes).await
}

async fn tls_round_trip(
    target: &Target,
    host_port: &str,
    req_bytes: &[u8],
    tls_mode: TlsMode,
) -> Result<Vec<u8>, ObserverClientError> {
    let connector = build_tls_connector(tls_mode)?;
    let server_name = rustls::pki_types::ServerName::try_from(target.host.clone())
        .map_err(|e| ObserverClientError::Tls(format!("server name `{}`: {e}", target.host)))?;

    let tcp = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(host_port))
        .await
        .map_err(|_| ObserverClientError::Timeout)?
        .map_err(|e| ObserverClientError::Connect {
            addr: host_port.to_string(),
            detail: e.to_string(),
        })?;

    let mut stream =
        tokio::time::timeout(Duration::from_secs(5), connector.connect(server_name, tcp))
            .await
            .map_err(|_| ObserverClientError::Timeout)?
            .map_err(|e| ObserverClientError::Tls(format!("handshake: {e}")))?;

    write_request_and_read(&mut stream, req_bytes).await
}

async fn write_request_and_read<S>(
    stream: &mut S,
    req_bytes: &[u8],
) -> Result<Vec<u8>, ObserverClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(req_bytes).await?;
    let mut buf = Vec::new();
    let read_fut = stream.read_to_end(&mut buf);
    tokio::time::timeout(Duration::from_secs(5), read_fut)
        .await
        .map_err(|_| ObserverClientError::Timeout)??;
    Ok(buf)
}

fn parse_response(buf: &[u8]) -> Result<String, ObserverClientError> {
    let response = std::str::from_utf8(buf).map_err(|_| ObserverClientError::NotUtf8)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or(ObserverClientError::Framing)?;
    let status_line = head
        .lines()
        .next()
        .ok_or_else(|| ObserverClientError::InvalidUrl("empty status line".into()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ObserverClientError::InvalidUrl(format!("bad status `{status_line}`")))?;
    if !(200..300).contains(&status) {
        return Err(ObserverClientError::Status {
            status,
            body: body.to_string(),
        });
    }
    Ok(body.to_string())
}

fn build_tls_connector(
    tls_mode: TlsMode,
) -> Result<tokio_rustls::TlsConnector, ObserverClientError> {
    let config = match tls_mode {
        TlsMode::Native => {
            let mut roots = rustls::RootCertStore::empty();
            let result = rustls_native_certs::load_native_certs();
            if !result.errors.is_empty() {
                let first = result
                    .errors
                    .first()
                    .map(|e| format!("{e}"))
                    .unwrap_or_default();
                return Err(ObserverClientError::Tls(format!(
                    "load native certs: {first}"
                )));
            }
            for cert in result.certs {
                roots
                    .add(cert)
                    .map_err(|e| ObserverClientError::Tls(format!("add native cert: {e}")))?;
            }
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        }
        TlsMode::InsecureSkipVerify => rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(insecure::NoVerify))
            .with_no_client_auth(),
    };
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

mod insecure {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    /// **DEV ONLY** server-cert verifier that accepts anything.
    /// Reachable exclusively via `grx observer --insecure-no-tls`;
    /// the production daemon path uses [`super::TlsMode::Native`].
    #[derive(Debug)]
    pub(super) struct NoVerify;

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::ED25519,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parses_plain_host_port() {
        let t = Target::parse("127.0.0.1:9090").unwrap();
        assert_eq!(t.host, "127.0.0.1");
        assert_eq!(t.port, 9090);
        assert!(!t.tls);
    }

    #[test]
    fn target_parses_explicit_http() {
        let t = Target::parse("http://localhost:8080").unwrap();
        assert_eq!(t.host, "localhost");
        assert_eq!(t.port, 8080);
        assert!(!t.tls);
    }

    #[test]
    fn target_parses_https() {
        let t = Target::parse("https://example.com:443").unwrap();
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, 443);
        assert!(t.tls);
    }

    #[test]
    fn target_strips_trailing_path() {
        let t = Target::parse("127.0.0.1:9090/v1/something").unwrap();
        assert_eq!(t.host, "127.0.0.1");
        assert_eq!(t.port, 9090);
    }

    #[test]
    fn target_rejects_missing_port() {
        assert!(matches!(
            Target::parse("127.0.0.1"),
            Err(ObserverClientError::InvalidUrl(_))
        ));
    }

    #[test]
    fn resolve_token_rejects_both_sources() {
        let path = std::path::PathBuf::from("/tmp/x");
        assert!(matches!(
            resolve_token(Some("X"), Some(&path)),
            Err(ObserverClientError::TokenLoad(_))
        ));
    }

    #[test]
    fn resolve_token_returns_none_when_neither_set() {
        assert!(matches!(resolve_token(None, None), Ok(None)));
    }
}
