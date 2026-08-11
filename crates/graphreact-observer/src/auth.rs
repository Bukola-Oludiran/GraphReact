//! **M16-Backend Phase 2.** Bearer-token authentication for the
//! observer HTTP server.
//!
//! Mirrors the [`graphreact_dist::SecretCookie`] pattern in
//! `crates/graphreact-dist/src/secret.rs`: bytes wrapped in a type
//! that derives [`Zeroize`] + [`ZeroizeOnDrop`] so a panic-dump or
//! core-file can't leak the secret. Comparison uses
//! `subtle::ConstantTimeEq` so the wire-side check doesn't leak
//! prefix length via timing.
//!
//! ## Loading paths
//!
//! - [`BearerToken::from_env`] — read a named env var. Primary
//!   production path (systemd `EnvironmentFile=`, k8s secrets,
//!   Docker `--env-file`).
//! - [`BearerToken::from_file`] — read a file with mode-0600
//!   enforcement on Unix. Mirrors `SecretCookie::from_file` exactly.
//! - [`BearerToken::from_bytes`] — direct construction for tests.
//!
//! Operators MUST NOT pass the token literal as a CLI argument
//! (process listings via `ps aux` would leak it). The CLI
//! `--observer-token-env <NAME>` / `--observer-token-file <PATH>`
//! flags name the source rather than the value.

use std::path::Path;

use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Bearer token. Constant-time equality via `subtle::ConstantTimeEq`
/// is the ONLY equality check; the `PartialEq` impl exists for
/// convenience in test fixtures and forwards to the same path.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct BearerToken {
    bytes: Vec<u8>,
}

/// Loading errors. Each variant maps to a specific operator-visible
/// failure mode so the CLI's error output names exactly what to fix.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// `--observer-token-env <NAME>` set but the env var is unset
    /// or empty.
    #[error("observer token env `{name}`: {detail}")]
    Env { name: String, detail: String },

    /// `--observer-token-file <PATH>` set but the file is missing,
    /// unreadable, or empty.
    #[error("observer token file {path}: {detail}")]
    File { path: String, detail: String },

    /// On Unix, the token file is readable by group or other.
    /// Mirrors `SecretCookie::from_file`'s mode-0600 enforcement.
    #[error(
        "observer token file {path} has mode {mode:#o}; must not be \
         readable by group or other (chmod 600 the file)"
    )]
    InsecureFileMode { path: String, mode: u32 },

    /// Token is empty (rejected to avoid the "no auth = empty
    /// token" foot-gun).
    #[error("observer token is empty (set the env var / file with the bearer value)")]
    Empty,
}

impl BearerToken {
    /// Construct from raw bytes. Returns [`AuthError::Empty`] if
    /// `bytes` is empty (no implicit "empty token = no auth" mode).
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, AuthError> {
        if bytes.is_empty() {
            return Err(AuthError::Empty);
        }
        Ok(Self { bytes })
    }

    /// Read the token from an environment variable. Trailing
    /// `\n` / `\r\n` are NOT stripped — env vars don't typically
    /// have them, and we don't want to silently change a token
    /// whose first or last char happens to be a CR/LF.
    pub fn from_env(name: &str) -> Result<Self, AuthError> {
        match std::env::var(name) {
            Ok(s) if s.is_empty() => Err(AuthError::Env {
                name: name.to_string(),
                detail: "env var is empty".into(),
            }),
            Ok(s) => Self::from_bytes(s.into_bytes()),
            Err(e) => Err(AuthError::Env {
                name: name.to_string(),
                detail: e.to_string(),
            }),
        }
    }

    /// Read the token from a file. On Unix, refuses to load if any
    /// group-or-other bit is set on the file's mode. One trailing
    /// newline (`\n` or `\r\n`) is stripped — a `printf "tok" >
    /// file` shell idiom appends the newline most of the time, and
    /// silently letting it through would mean the token-on-the-wire
    /// is `tok\n` while the operator typed `tok`.
    pub fn from_file(path: &Path) -> Result<Self, AuthError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(path).map_err(|e| AuthError::File {
                path: path.display().to_string(),
                detail: format!("stat: {e}"),
            })?;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(AuthError::InsecureFileMode {
                    path: path.display().to_string(),
                    mode,
                });
            }
        }
        let mut bytes = std::fs::read(path).map_err(|e| AuthError::File {
            path: path.display().to_string(),
            detail: format!("read: {e}"),
        })?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        if bytes.is_empty() {
            return Err(AuthError::File {
                path: path.display().to_string(),
                detail: "file is empty after trailing-newline strip".into(),
            });
        }
        Self::from_bytes(bytes)
    }

    /// Constant-time equality check against the wire-side bearer
    /// value. Returns `false` on length mismatch (length comparison
    /// is unavoidable; subtle's `ct_eq` requires equal lengths) —
    /// timing attacks would only reveal the token's *length*, which
    /// the operator chose, not its contents.
    #[must_use]
    pub fn matches(&self, candidate: &[u8]) -> bool {
        if self.bytes.len() != candidate.len() {
            return false;
        }
        self.bytes.ct_eq(candidate).into()
    }

    /// Length of the token in bytes. Exposed for the
    /// `auth_required.rs` integration test that asserts constant-
    /// time compare doesn't statistically leak prefix length —
    /// caller varies length by 1 byte and measures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print token contents.
        f.debug_struct("BearerToken")
            .field("bytes", &"<redacted>")
            .field("len", &self.bytes.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path(suffix: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("grx-observer-auth-test-{pid}-{n}-{suffix}"))
    }

    #[test]
    fn rejects_empty_bytes() {
        let err = BearerToken::from_bytes(vec![]).unwrap_err();
        assert!(matches!(err, AuthError::Empty));
    }

    #[test]
    fn matches_correct_token() {
        let tok = BearerToken::from_bytes(b"hello".to_vec()).unwrap();
        assert!(tok.matches(b"hello"));
        assert!(!tok.matches(b"world"));
        assert!(!tok.matches(b"hell")); // length mismatch
    }

    #[test]
    fn debug_redacts_contents() {
        let tok = BearerToken::from_bytes(b"super-secret-bearer".to_vec()).unwrap();
        let dbg = format!("{tok:?}");
        assert!(!dbg.contains("super-secret"));
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn from_env_rejects_unset() {
        let err =
            BearerToken::from_env("GRX_OBSERVER_TEST_TOKEN_DEFINITELY_UNSET_ZNXBVKQR").unwrap_err();
        assert!(matches!(err, AuthError::Env { .. }));
    }

    #[test]
    fn from_env_strips_nothing() {
        std::env::set_var("GRX_OBSERVER_TEST_TOKEN_NOSTRIP_AAQ", "abc\n");
        let tok = BearerToken::from_env("GRX_OBSERVER_TEST_TOKEN_NOSTRIP_AAQ").unwrap();
        std::env::remove_var("GRX_OBSERVER_TEST_TOKEN_NOSTRIP_AAQ");
        assert!(tok.matches(b"abc\n"));
        assert!(!tok.matches(b"abc"));
    }

    #[cfg(unix)]
    #[test]
    fn from_file_strips_one_newline() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("strip-newline");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"abcdef\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let tok = BearerToken::from_file(&path).unwrap();
        assert!(tok.matches(b"abcdef"));
        std::fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn from_file_rejects_world_readable() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("world-readable");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = BearerToken::from_file(&path).unwrap_err();
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, AuthError::InsecureFileMode { .. }));
    }
}
