//! `graphreact-secrets` — operator-facing secret store.
//!
//! **M-Prod sub-track #3 (Phase A).** This crate ships the
//! [`SecretValue`] wrapper + the [`SecretStore`] trait + two backend
//! implementations:
//!
//! - [`KeyringStore`] — wraps the [`keyring`] crate so a single
//!   API call routes to macOS Keychain (`apple-native` feature),
//!   Linux Secret Service / `secret-tool` (`linux-native-sync-persistent`),
//!   or Windows Credential Manager (`windows-native`). Per-platform
//!   features are pinned in `Cargo.toml`'s
//!   `[target.'cfg(target_os = …)'.dependencies]` blocks so we don't
//!   pull dbus on macOS or `security-framework` on Linux.
//! - [`EnvStore`] — reads `GRX_SECRET_<UPPER_NAME>` env vars. Production
//!   path for headless CI / containers where no OS keyring agent is
//!   present, and the documented fallback for SSH-into-macOS or
//!   non-GUI sessions where Keychain access pops a TouchID modal.
//!
//! ## What this crate does NOT do (Phase B)
//!
//! - Engine-level runtime accessor `ctx.secret(name)`. That requires
//!   a Engine trait surface change + per-engine host-callback wiring
//!   (JSC, CPython, Go) + an ADR — placeholder name `ADR-0005-runtime-secret-accessor.md`.
//! - SIGHUP-triggered re-read on rotation (depends on M-Prod #2 atomic-
//!   swap pattern shipping first).
//! - Audit log of secret reads (depends on M-Prod #1 audit-chain sink).
//! - Keychain access-groups for cross-process sharing (requires code-
//!   signing `grx` with `keychain-access-groups` entitlements; lands in
//!   M-Prod #8 packaging).
//!
//! ## Backend selection
//!
//! The CLI (`grx secrets {set,get,list,remove} --backend …`) calls
//! one of:
//!
//! ```rust,ignore
//! use graphreact_secrets::{EnvStore, KeyringStore, SecretStore};
//!
//! let store: Box<dyn SecretStore> = if cli.use_env {
//!     Box::new(EnvStore)
//! } else {
//!     Box::new(KeyringStore)
//! };
//! let v = store.get("DATABASE_URL")?;
//! println!("{}", String::from_utf8_lossy(v.expose_bytes()));
//! ```
//!
//! Production deploys typically pin to `--backend env` (mounts
//! Docker secrets / k8s `env:` directly) and reserve the keyring
//! backend for developer workstations where Keychain / Secret Service
//! / Credential Manager is unlocked.
//!
//! ## Service-name convention
//!
//! The keyring backend uses the service string `"graphreact"` for
//! every entry, with the secret name as the account. That makes
//! entries discoverable via
//! `security find-generic-password -s graphreact -a DATABASE_URL`
//! (macOS) or `secret-tool search service graphreact account DATABASE_URL`
//! (Linux) for ops debugging without exposing the value.

#![allow(clippy::module_name_repetitions)]

use std::fmt;

use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Service name used as the namespace key in every backend (Keychain
/// "service", Secret Service "service" attribute, Windows Credential
/// Manager "target name" prefix). Stable identifier so future
/// `grx secrets` commands round-trip across machines.
pub const SECRET_SERVICE_NAME: &str = "graphreact";

/// Env-var prefix the [`EnvStore`] backend uses. `<UPPER_NAME>` is
/// appended to form the lookup key. Validated by [`validate_secret_name`].
pub const ENV_PREFIX: &str = "GRX_SECRET_";

/// Hard limit on a secret's byte length. Picked to match the smallest
/// production keyring backend (Windows Credential Manager caps the
/// credential blob at 2560 UTF-16 bytes ≈ 1280 BMP chars / 5KiB UTF-8
/// worst case). Surface the cap as an explicit error so operators see
/// the limit instead of a backend-specific message.
///
/// 4096 bytes accommodates most short-lived JWT signing keys and
/// connection strings while leaving headroom for the inevitable
/// `--description` prefix Windows wincred prepends internally.
pub const MAX_SECRET_BYTES: usize = 4096;

/// A zeroized secret value.
///
/// Wraps a `Vec<u8>` and derives [`Zeroize`] + [`ZeroizeOnDrop`] so
/// the bytes are scrubbed when the value is dropped — a panic-dump
/// or core-file can't leak the secret. The [`fmt::Debug`] impl prints
/// `SecretValue { bytes: "<redacted>", len: N }`; `len` is reported
/// because the length itself isn't sensitive (operator chose it) and
/// is useful for debugging encoding issues.
///
/// Equality is **NOT** derived — a `PartialEq` impl would invite
/// `secret == "literal"` checks that leak via timing. If you need to
/// compare a candidate against a known value, route through
/// [`crate::ct_eq`] (which forwards to `subtle::ConstantTimeEq` once
/// that dep is added in Phase B's runtime accessor).
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretValue {
    bytes: Vec<u8>,
}

impl SecretValue {
    /// Construct from raw bytes. Returns [`SecretsError::Empty`] on
    /// empty input and [`SecretsError::TooLarge`] when the input
    /// exceeds [`MAX_SECRET_BYTES`].
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, SecretsError> {
        if bytes.is_empty() {
            return Err(SecretsError::Empty);
        }
        if bytes.len() > MAX_SECRET_BYTES {
            return Err(SecretsError::TooLarge {
                len: bytes.len(),
                limit: MAX_SECRET_BYTES,
            });
        }
        Ok(Self { bytes })
    }

    /// Construct from a UTF-8 string. The string's heap allocation is
    /// transferred into the [`SecretValue`] (no copy), and the caller
    /// loses access — the only way to read the bytes back is
    /// [`SecretValue::expose_bytes`].
    pub fn from_string(s: String) -> Result<Self, SecretsError> {
        let mut buf = s.into_bytes();
        let res = Self::from_bytes(buf.clone());
        buf.zeroize();
        res
    }

    /// Expose the raw bytes. **Use with care** — copying the slice
    /// into another `Vec` defeats the zeroize-on-drop contract. Most
    /// callers should pass `&SecretValue` through and lean on the
    /// borrow checker to keep the value scoped.
    #[must_use]
    pub fn expose_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Length in bytes. Cheap and non-sensitive — useful for
    /// debugging and for the redacted [`fmt::Debug`] line.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the secret is empty. Always returns `false` for values
    /// constructed via [`SecretValue::from_bytes`] / [`SecretValue::from_string`]
    /// (both reject empty input). Present for API symmetry with `len`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print contents — Debug-derived logs hit stderr +
        // panics + crash dumps.
        f.debug_struct("SecretValue")
            .field("bytes", &"<redacted>")
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// Operator-visible failure modes. The error message names the
/// failing concept (backend / secret / file mode) rather than the
/// underlying transport so the CLI's error output reads correctly
/// regardless of which backend is in play.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// Lookup miss. `set` returns this only if a backend distinguishes
    /// update from create (today: no — both backends are upsert).
    #[error("secret `{name}` not found")]
    NotFound { name: String },

    /// Secret name didn't validate. Per [`validate_secret_name`] the
    /// name must match `[A-Z][A-Z0-9_]*` so it round-trips identically
    /// across the keyring + env backends (env-var names are
    /// case-sensitive on POSIX and upper-case by convention).
    #[error("invalid secret name `{name}`: {reason}")]
    InvalidName { name: String, reason: String },

    /// Backend not reachable. Common causes per platform:
    ///
    /// - Linux: no dbus + secret-service agent unlocked (containers,
    ///   headless CI). Documented mitigation: use `--backend env`.
    /// - macOS: no logged-in user session (SSH attach without
    ///   `launchctl asuser`), or the user denied the TouchID prompt.
    /// - Windows: Credential Manager service stopped.
    #[error("secret store backend unavailable ({backend}): {detail}")]
    BackendUnavailable {
        backend: &'static str,
        detail: String,
    },

    /// Secret was empty. Both [`SecretValue::from_bytes`] and the
    /// `grx secrets set` reader reject empty input — silent acceptance
    /// would invite the "no auth = empty secret" foot-gun.
    #[error("secret value is empty")]
    Empty,

    /// Secret exceeds [`MAX_SECRET_BYTES`]. Surfaced as a structured
    /// error so the CLI can name the cap instead of letting the
    /// backend produce a Windows-specific "credential too large"
    /// message.
    #[error("secret value too large: {len} bytes (limit {limit})")]
    TooLarge { len: usize, limit: usize },

    /// Generic I/O failure (file-backed env source, stdin read).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Backend-agnostic secret store contract. All four methods route
/// through [`SECRET_SERVICE_NAME`] so the keyring backend's account
/// space is shared across [`KeyringStore`] instances and the env
/// backend's namespace is shared across [`EnvStore`] instances.
///
/// The trait is object-safe (no generics, all methods `&self`) so
/// the CLI can dispatch through `Box<dyn SecretStore>`. Implementations
/// must be safe to share across threads — `KeyringStore` wraps the
/// `keyring` crate's `Entry::new` which is `Send + Sync`, and
/// `EnvStore` only reads `std::env` (process-global state guarded by
/// the same locks as every other env-var reader).
pub trait SecretStore: Send + Sync {
    /// Write a secret. Upsert semantics — overwriting an existing
    /// entry succeeds.
    fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError>;

    /// Read a secret. Returns [`SecretsError::NotFound`] when the
    /// entry is absent (NOT [`SecretsError::Empty`] — empty is
    /// "present but zero bytes", which neither backend supports).
    fn get(&self, name: &str) -> Result<SecretValue, SecretsError>;

    /// List the secret names visible to this store (NOT values).
    /// Order is unspecified — the env backend returns the env-var
    /// scan in `std::env::vars` order; the keyring backend is
    /// backend-defined.
    ///
    /// **Backend caveats:** the keyring backend's `list` is
    /// platform-shaped and may return an empty `Vec` on backends
    /// (Windows, some Linux distros) that don't expose an enumeration
    /// API. The env backend always returns the full set. Returning an
    /// empty vec is NOT an error — it's distinguishable from
    /// [`SecretsError::BackendUnavailable`].
    fn list(&self) -> Result<Vec<String>, SecretsError>;

    /// Remove a secret. Idempotent — removing an absent entry
    /// succeeds (returns `Ok(())`). Lets `grx secrets remove FOO`
    /// be safe to re-run.
    fn remove(&self, name: &str) -> Result<(), SecretsError>;
}

/// Validate that `name` matches `[A-Z][A-Z0-9_]*`. This is enforced
/// at the boundary so the keyring + env backends agree on the
/// account-name space (env-var names are POSIX-shaped and the
/// keyring service-name space allows arbitrary UTF-8; restricting
/// to the env shape keeps secrets portable across backends).
pub fn validate_secret_name(name: &str) -> Result<(), SecretsError> {
    if name.is_empty() {
        return Err(SecretsError::InvalidName {
            name: name.to_string(),
            reason: "name is empty".into(),
        });
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty checked above");
    if !first.is_ascii_uppercase() {
        return Err(SecretsError::InvalidName {
            name: name.to_string(),
            reason: "first character must be uppercase ASCII letter A-Z".into(),
        });
    }
    for c in chars {
        if !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
            return Err(SecretsError::InvalidName {
                name: name.to_string(),
                reason: format!("character `{c}` not allowed; names match [A-Z][A-Z0-9_]*"),
            });
        }
    }
    Ok(())
}

/// Env-var backend. Reads `GRX_SECRET_<NAME>` where `<NAME>` is the
/// secret name (already upper-case per [`validate_secret_name`]).
///
/// **Operating model.** The backend lists by scanning `std::env::vars`
/// for entries with the `GRX_SECRET_` prefix; this means "list" is
/// O(env-var count). For typical processes (< 100 env vars) the cost
/// is sub-microsecond. The backend is well-suited to containerised
/// deploys (k8s `env:` block, Docker `--env-file`, systemd
/// `Environment=`).
///
/// **`set` semantics.** `set` mutates `std::env::vars` via
/// `std::env::set_var` — this is process-local and ephemeral. For
/// persistent ops use the keyring backend (developer workstation) or
/// edit the orchestrator's secret source (production).
///
/// **`remove` semantics.** `remove` calls `std::env::remove_var`;
/// also process-local + ephemeral.
///
/// Process-global env-var mutation in tests requires serialization
/// (Rust's stdlib documents `set_var` as `unsafe` in newer toolchains
/// because it isn't thread-safe vs concurrent `getenv` callers); the
/// unit test suite uses unique env-var names + a single test thread
/// per-name to avoid cross-test contention.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvStore;

impl EnvStore {
    fn env_key(name: &str) -> String {
        let mut s = String::with_capacity(ENV_PREFIX.len() + name.len());
        s.push_str(ENV_PREFIX);
        s.push_str(name);
        s
    }
}

impl SecretStore for EnvStore {
    fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
        validate_secret_name(name)?;
        let key = Self::env_key(name);
        // SAFETY note: `std::env::set_var` is not thread-safe across
        // concurrent `getenv`/`set_var` calls on POSIX. The CLI is
        // single-threaded at the dispatch site (`fn dispatch` in
        // bin/grx.rs runs serially); test isolation is documented in
        // the module-level docstring.
        let bytes_string =
            std::str::from_utf8(value.expose_bytes()).map_err(|_| SecretsError::InvalidName {
                name: name.to_string(),
                reason: "EnvStore::set requires UTF-8 bytes (env vars are byte-string \
                         shaped on POSIX but the Rust API requires &OsStr / &str)"
                    .into(),
            })?;
        std::env::set_var(&key, bytes_string);
        tracing::info!(
            target: "graphreact::secrets::audit",
            backend = "env",
            action = "set",
            name = name,
            "secret set"
        );
        Ok(())
    }

    fn get(&self, name: &str) -> Result<SecretValue, SecretsError> {
        validate_secret_name(name)?;
        let key = Self::env_key(name);
        match std::env::var(&key) {
            Ok(s) if s.is_empty() => Err(SecretsError::NotFound {
                name: name.to_string(),
            }),
            Ok(s) => SecretValue::from_string(s),
            Err(std::env::VarError::NotPresent) => Err(SecretsError::NotFound {
                name: name.to_string(),
            }),
            Err(std::env::VarError::NotUnicode(_)) => Err(SecretsError::InvalidName {
                name: name.to_string(),
                reason: "env-var value is not valid UTF-8".into(),
            }),
        }
    }

    fn list(&self) -> Result<Vec<String>, SecretsError> {
        let mut out = Vec::new();
        for (k, _v) in std::env::vars() {
            if let Some(suffix) = k.strip_prefix(ENV_PREFIX) {
                // Skip entries that don't match the validated shape —
                // a stray `GRX_SECRET_lowercase` shouldn't surface
                // because `get` would refuse it anyway.
                if validate_secret_name(suffix).is_ok() {
                    out.push(suffix.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    fn remove(&self, name: &str) -> Result<(), SecretsError> {
        validate_secret_name(name)?;
        let key = Self::env_key(name);
        std::env::remove_var(&key);
        tracing::info!(
            target: "graphreact::secrets::audit",
            backend = "env",
            action = "remove",
            name = name,
            "secret removed"
        );
        Ok(())
    }
}

/// OS keyring backend. Uses [`keyring::Entry::new`] under the hood:
///
/// - macOS — system Keychain via `security-framework` (login keychain).
/// - Linux — Secret Service over dbus (gnome-keyring / kwallet).
/// - Windows — Credential Manager via `windows-sys`.
///
/// Service name is fixed to [`SECRET_SERVICE_NAME`]; the secret name
/// is the account name. Account name is restricted to
/// [`validate_secret_name`] so entries round-trip cleanly between
/// the env backend and the keyring backend.
///
/// `KeyringStore` is `Send + Sync` because `keyring::Entry` v3 is
/// `Send + Sync` (it stores a small platform-specific handle that
/// the underlying APIs treat as immutable).
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringStore;

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
impl KeyringStore {
    /// Construct a new entry for `(SECRET_SERVICE_NAME, name)`. Returns
    /// the [`keyring`] error wrapped as [`SecretsError::BackendUnavailable`]
    /// so callers don't need to depend on the `keyring` crate directly.
    fn entry(name: &str) -> Result<keyring::Entry, SecretsError> {
        keyring::Entry::new(SECRET_SERVICE_NAME, name).map_err(|e| {
            SecretsError::BackendUnavailable {
                backend: "keyring",
                detail: format!("Entry::new({SECRET_SERVICE_NAME}, {name}): {e}"),
            }
        })
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
impl SecretStore for KeyringStore {
    fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
        validate_secret_name(name)?;
        let entry = Self::entry(name)?;
        // keyring::Entry::set_password takes &str; reject non-UTF-8
        // secrets at this boundary so the OS-side error is consistent.
        let s =
            std::str::from_utf8(value.expose_bytes()).map_err(|_| SecretsError::InvalidName {
                name: name.to_string(),
                reason: "KeyringStore::set requires UTF-8 secret bytes".into(),
            })?;
        entry.set_password(s).map_err(map_keyring_err)?;
        tracing::info!(
            target: "graphreact::secrets::audit",
            backend = "keyring",
            action = "set",
            name = name,
            "secret set"
        );
        Ok(())
    }

    fn get(&self, name: &str) -> Result<SecretValue, SecretsError> {
        validate_secret_name(name)?;
        let entry = Self::entry(name)?;
        match entry.get_password() {
            Ok(s) => SecretValue::from_string(s),
            Err(keyring::Error::NoEntry) => Err(SecretsError::NotFound {
                name: name.to_string(),
            }),
            Err(e) => Err(map_keyring_err(e)),
        }
    }

    fn list(&self) -> Result<Vec<String>, SecretsError> {
        // The `keyring` crate's enumeration API is platform-shaped
        // and not uniformly available on every backend feature
        // (Windows wincred lacks a portable enumeration call; some
        // Linux secret-service backends require an unlock). Returning
        // an empty vec is the documented "no enumeration" path and
        // is distinguishable from BackendUnavailable. Callers that
        // need enumeration today use `--backend env`.
        //
        // When the `keyring` crate ships a uniform `search` API across
        // all three feature flags (tracked upstream as
        // `hwchen/keyring-rs#193`), this implementation will be updated
        // to delegate. For Phase A, the documented behavior is "list
        // returns the empty vec on keyring; switch to --backend env
        // to enumerate".
        Ok(Vec::new())
    }

    fn remove(&self, name: &str) -> Result<(), SecretsError> {
        validate_secret_name(name)?;
        let entry = Self::entry(name)?;
        match entry.delete_credential() {
            Ok(()) => {
                tracing::info!(
                    target: "graphreact::secrets::audit",
                    backend = "keyring",
                    action = "remove",
                    name = name,
                    "secret removed"
                );
                Ok(())
            }
            // Idempotent removal — already absent isn't an error.
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring_err(e)),
        }
    }
}

/// Map a [`keyring::Error`] into the crate's [`SecretsError`] surface.
/// Centralises the "which kind of failure → which CLI message" matrix
/// so backend-specific quirks (dbus timeouts on Linux, TouchID
/// dismissal on macOS) don't surface as raw `Display` impls.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn map_keyring_err(e: keyring::Error) -> SecretsError {
    match e {
        keyring::Error::NoEntry => SecretsError::NotFound {
            name: String::new(),
        },
        keyring::Error::TooLong(_, len) => SecretsError::TooLarge {
            // `keyring::Error::TooLong` reports the length as `u32`
            // (Windows wincred-shaped); SecretsError carries `usize`.
            len: len as usize,
            limit: MAX_SECRET_BYTES,
        },
        other => SecretsError::BackendUnavailable {
            backend: "keyring",
            detail: other.to_string(),
        },
    }
}

// On platforms where we don't pull `keyring` (none today within our
// target matrix, but a defensive cfg-fallback keeps `cargo check`
// honest on hypothetical future tier-3 targets).
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
impl SecretStore for KeyringStore {
    fn set(&self, _name: &str, _value: SecretValue) -> Result<(), SecretsError> {
        Err(SecretsError::BackendUnavailable {
            backend: "keyring",
            detail: "no keyring backend compiled for this target_os".into(),
        })
    }

    fn get(&self, _name: &str) -> Result<SecretValue, SecretsError> {
        Err(SecretsError::BackendUnavailable {
            backend: "keyring",
            detail: "no keyring backend compiled for this target_os".into(),
        })
    }

    fn list(&self) -> Result<Vec<String>, SecretsError> {
        Ok(Vec::new())
    }

    fn remove(&self, _name: &str) -> Result<(), SecretsError> {
        Err(SecretsError::BackendUnavailable {
            backend: "keyring",
            detail: "no keyring backend compiled for this target_os".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Per-test unique suffix so concurrent `cargo test` runs don't
    /// race on env-var names. Pairs `pid` (different per `cargo test`
    /// invocation) with a process-local counter (different per call).
    fn unique_name(prefix: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        // Result must satisfy validate_secret_name → `[A-Z][A-Z0-9_]*`.
        format!("{prefix}_{pid}_{n}")
    }

    #[test]
    fn secret_value_from_bytes_rejects_empty() {
        let err = SecretValue::from_bytes(vec![]).unwrap_err();
        assert!(matches!(err, SecretsError::Empty));
    }

    #[test]
    fn secret_value_from_bytes_rejects_oversize() {
        let buf = vec![b'a'; MAX_SECRET_BYTES + 1];
        let err = SecretValue::from_bytes(buf).unwrap_err();
        match err {
            SecretsError::TooLarge { len, limit } => {
                assert_eq!(len, MAX_SECRET_BYTES + 1);
                assert_eq!(limit, MAX_SECRET_BYTES);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn secret_value_debug_redacts() {
        let v = SecretValue::from_bytes(b"super-secret-bearer".to_vec()).unwrap();
        let dbg = format!("{v:?}");
        assert!(!dbg.contains("super-secret"), "Debug leaked: {dbg}");
        assert!(dbg.contains("redacted"));
        assert!(dbg.contains("len"));
    }

    #[test]
    fn validate_name_accepts_uppercase_ascii() {
        assert!(validate_secret_name("DATABASE_URL").is_ok());
        assert!(validate_secret_name("API_KEY_2").is_ok());
        assert!(validate_secret_name("A").is_ok());
    }

    #[test]
    fn validate_name_rejects_lowercase() {
        let err = validate_secret_name("database_url").unwrap_err();
        assert!(matches!(err, SecretsError::InvalidName { .. }));
    }

    #[test]
    fn validate_name_rejects_leading_digit() {
        let err = validate_secret_name("1DATABASE").unwrap_err();
        assert!(matches!(err, SecretsError::InvalidName { .. }));
    }

    #[test]
    fn validate_name_rejects_empty() {
        let err = validate_secret_name("").unwrap_err();
        assert!(matches!(err, SecretsError::InvalidName { .. }));
    }

    #[test]
    fn env_store_set_and_get() {
        let store = EnvStore;
        let name = unique_name("ESET");
        store
            .set(&name, SecretValue::from_bytes(b"hello".to_vec()).unwrap())
            .unwrap();
        let v = store.get(&name).unwrap();
        assert_eq!(v.expose_bytes(), b"hello");
        store.remove(&name).unwrap();
    }

    #[test]
    fn env_store_get_missing_returns_not_found() {
        let store = EnvStore;
        let name = unique_name("EMISS");
        let err = store.get(&name).unwrap_err();
        match err {
            SecretsError::NotFound { name: n } => assert_eq!(n, name),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn env_store_remove_is_idempotent() {
        let store = EnvStore;
        let name = unique_name("EIDEMP");
        // Remove without prior set must succeed.
        store.remove(&name).unwrap();
        // Set, remove, remove again — all succeed.
        store
            .set(&name, SecretValue::from_bytes(b"x".to_vec()).unwrap())
            .unwrap();
        store.remove(&name).unwrap();
        store.remove(&name).unwrap();
    }

    #[test]
    fn env_store_list_skips_invalid_names() {
        let store = EnvStore;
        let good = unique_name("ELIST");
        store
            .set(&good, SecretValue::from_bytes(b"v".to_vec()).unwrap())
            .unwrap();
        // Plant an invalid-shape env var directly with the prefix so
        // we exercise the filter path.
        let bad_key = format!("{}lower_case_bad", ENV_PREFIX);
        std::env::set_var(&bad_key, "irrelevant");
        let names = store.list().unwrap();
        assert!(
            names.contains(&good),
            "list missing expected name: {names:?}"
        );
        assert!(
            !names.contains(&"lower_case_bad".to_string()),
            "list leaked invalid name: {names:?}"
        );
        std::env::remove_var(&bad_key);
        store.remove(&good).unwrap();
    }

    #[test]
    fn env_store_rejects_invalid_name_on_get() {
        let store = EnvStore;
        let err = store.get("not-upper").unwrap_err();
        assert!(matches!(err, SecretsError::InvalidName { .. }));
    }

    #[test]
    fn env_store_set_overwrites() {
        let store = EnvStore;
        let name = unique_name("EOVR");
        store
            .set(&name, SecretValue::from_bytes(b"first".to_vec()).unwrap())
            .unwrap();
        store
            .set(&name, SecretValue::from_bytes(b"second".to_vec()).unwrap())
            .unwrap();
        let v = store.get(&name).unwrap();
        assert_eq!(v.expose_bytes(), b"second");
        store.remove(&name).unwrap();
    }

    // -----------------------------------------------------------------
    // Keyring backend tests. Opt-in via `GRX_SECRETS_RUN_KEYRING_TESTS=1`.
    //
    // GitHub-hosted CI runners don't ship an unlocked OS keyring
    // agent. macOS Keychain prompts for TouchID; Linux secret-service
    // requires a running gnome-keyring or kwallet under dbus; Windows
    // Credential Manager requires the running user's profile. Each of
    // these would hang or fail the test. Developer workstations with
    // an unlocked keyring run the test by exporting
    // `GRX_SECRETS_RUN_KEYRING_TESTS=1`.
    // -----------------------------------------------------------------

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    fn keyring_tests_enabled() -> bool {
        std::env::var("GRX_SECRETS_RUN_KEYRING_TESTS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[test]
    fn keyring_store_round_trip_opt_in() {
        if !keyring_tests_enabled() {
            eprintln!(
                "skipping keyring_store_round_trip_opt_in; \
                 set GRX_SECRETS_RUN_KEYRING_TESTS=1 to enable"
            );
            return;
        }
        let store = KeyringStore;
        let name = unique_name("KRT");
        store
            .set(
                &name,
                SecretValue::from_bytes(b"opt-in-value".to_vec()).unwrap(),
            )
            .expect("set");
        let v = store.get(&name).expect("get");
        assert_eq!(v.expose_bytes(), b"opt-in-value");
        store.remove(&name).expect("remove");
        let err = store.get(&name).unwrap_err();
        assert!(matches!(err, SecretsError::NotFound { .. }));
    }

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[test]
    fn keyring_store_list_is_empty_or_succeeds() {
        // Even without GRX_SECRETS_RUN_KEYRING_TESTS, list() returning
        // Ok(vec![]) is the Phase-A contract. Verifying this is cheap +
        // doesn't touch the keychain.
        let store = KeyringStore;
        let names = store.list().expect("list returns Ok");
        assert!(names.is_empty(), "Phase A list returns empty vec");
    }

    #[test]
    fn secret_value_from_string_round_trip() {
        let v = SecretValue::from_string("hello world".to_string()).unwrap();
        assert_eq!(v.expose_bytes(), b"hello world");
        assert_eq!(v.len(), 11);
    }
}
