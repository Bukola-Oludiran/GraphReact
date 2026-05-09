//! Cluster cookie — shared secret used by the OTP-style
//! handshake's HMAC-SHA256(cookie, challenge) digest.
//!
//! Wrapped in `SecretCookie` with `Zeroize + ZeroizeOnDrop` so the
//! bytes are scrubbed from memory when the cookie is dropped. This
//! prevents a panic-dump or core-file from leaking the secret.
//!
//! Loading paths (in order of preference):
//! - [`SecretCookie::from_env`] — `GRX_COOKIE` env var. Primary
//!   production path; ergonomic for systemd / k8s secrets / Docker
//!   `--env-file`.
//! - [`SecretCookie::from_file`] — file with mode-0600 (rw-------)
//!   enforcement on Unix. Mirrors OTP's `~/.erlang.cookie`. World-
//!   or group-readable files are refused at load time.
//! - [`SecretCookie::from_bytes`] — direct construction for tests
//!   and programmatic configs. Production code should NOT accept
//!   cookie literals on the CLI (process listings leak them via
//!   `ps aux`).

use std::path::Path;

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::DistError;

/// Cluster cookie. Equality is constant-time via the underlying
/// `Vec<u8>::eq` is NOT used in security-sensitive paths — HMAC
/// comparison goes through `subtle::ConstantTimeEq`. The `Eq` impl
/// here is for testing convenience only.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretCookie {
    bytes: Vec<u8>,
}

impl SecretCookie {
    /// Build a cookie from raw bytes. Any non-empty length is
    /// accepted; HMAC-SHA256 derives a 32-byte key from any input.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, DistError> {
        if bytes.is_empty() {
            return Err(DistError::Cookie("cookie is empty".into()));
        }
        Ok(Self { bytes })
    }

    /// Read the cookie from an environment variable.
    /// `GRX_COOKIE` is the conventional name; users may override.
    /// Empty / unset environment variables return [`DistError::Cookie`].
    pub fn from_env(name: &str) -> Result<Self, DistError> {
        match std::env::var(name) {
            Ok(s) if s.is_empty() => Err(DistError::Cookie(format!("env var `{name}` is empty"))),
            Ok(s) => Self::from_bytes(s.into_bytes()),
            Err(e) => Err(DistError::Cookie(format!("env var `{name}`: {e}"))),
        }
    }

    /// Read the cookie from a file. On Unix, refuses to load if any
    /// group/other bit is set (mode-0600 enforcement, mirrors OTP's
    /// `~/.erlang.cookie`). One trailing newline is stripped — most
    /// shells append `\n` on `printf "secret" > cookie`.
    pub fn from_file(path: &Path) -> Result<Self, DistError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(path)
                .map_err(|e| DistError::Cookie(format!("stat {}: {e}", path.display())))?;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(DistError::Cookie(format!(
                    "cookie file {} has mode {:#o}; must not be readable by group or other (refusing for safety; chmod 600 the file)",
                    path.display(),
                    mode
                )));
            }
        }
        let mut bytes = std::fs::read(path)
            .map_err(|e| DistError::Cookie(format!("read {}: {e}", path.display())))?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            // Handle CRLF line endings too.
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        if bytes.is_empty() {
            return Err(DistError::Cookie(format!(
                "cookie file {} is empty",
                path.display()
            )));
        }
        Self::from_bytes(bytes)
    }

    /// Raw bytes for the HMAC layer. Marked `pub(crate)` so callers
    /// outside the crate can't accidentally serialise the cookie or
    /// log it.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::fmt::Debug for SecretCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print cookie contents — even Debug-derived logs go
        // to stderr / panics / crashdumps.
        f.debug_struct("SecretCookie").field("bytes", &"<redacted>").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_bytes_rejects_empty() {
        let err = SecretCookie::from_bytes(vec![]).unwrap_err();
        assert!(matches!(err, DistError::Cookie(_)));
    }

    #[test]
    fn from_bytes_accepts_any_nonempty() {
        let c = SecretCookie::from_bytes(b"hello world".to_vec()).unwrap();
        assert_eq!(c.as_bytes(), b"hello world");
    }

    #[test]
    fn debug_redacts_contents() {
        let c = SecretCookie::from_bytes(b"super-secret".to_vec()).unwrap();
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("super-secret"), "Debug leaked cookie: {dbg}");
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn from_env_strips_unset() {
        // Use a name we know is unset — namespaced under crate prefix.
        let err =
            SecretCookie::from_env("GRX_DIST_TEST_COOKIE_DEFINITELY_UNSET_NWPECDQJ").unwrap_err();
        assert!(matches!(err, DistError::Cookie(_)));
    }

    #[cfg(unix)]
    #[test]
    fn from_file_rejects_world_readable() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile_dir();
        let path = dir.join("world-readable.cookie");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(b"secret").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("set permissions");

        let err = SecretCookie::from_file(&path).unwrap_err();
        match err {
            DistError::Cookie(msg) => assert!(msg.contains("mode")),
            other => panic!("expected Cookie error, got {other:?}"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn from_file_accepts_mode_600() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile_dir();
        let path = dir.join("mode-600.cookie");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(b"secret\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("set permissions");

        let cookie = SecretCookie::from_file(&path).expect("load");
        assert_eq!(cookie.as_bytes(), b"secret"); // newline stripped

        std::fs::remove_file(&path).ok();
    }

    /// Per-process unique scratch dir so the file-mode tests don't
    /// race when run in parallel. Using `std::env::temp_dir()` plus
    /// the test's pid+thread keeps it dependency-free.
    #[cfg(unix)]
    fn tempfile_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("grx-dist-secret-test-{pid}-{n}"));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
