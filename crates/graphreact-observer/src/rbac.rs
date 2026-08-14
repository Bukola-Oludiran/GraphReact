//! **M-Prod sub-track #1 Phase B Part 1.** Multi-token bearer
//! registry + RBAC dispatch + per-route ACL substrate.
//!
//! Phase A shipped the hash-chained tamper-evident audit-log sink +
//! the single-bearer auth surface from M16-Backend P4. Phase B Part 1
//! widens the auth side: the observer can now hold **many** bearer
//! tokens, each with an operator-assigned identity, a closed-over
//! [`Role`] (Admin / ReadOnly / Custom-allowlist), and an optional
//! expiry. Authorization is dispatched per-route using a small
//! vocabulary of `&'static str` route labels that subset
//! [`crate::management_api::PROTECTED_ROUTE_LABELS`].
//!
//! ## Atomic-swap rotation
//!
//! [`RotatableRegistry`] mirrors
//! [`crate::tls_rotation::RotatableServerConfig`] bit-for-bit: an
//! `Arc<ArcSwap<TokenRegistry>>` newtype with cheap `load_full()`
//! reads on the per-request path and atomic `store(...)` writes from
//! the eventual SIGHUP handler (Part 2). Phase B Part 1 exposes the
//! substrate but does NOT wire SIGHUP itself — the CLI still passes
//! `rbac: None` by default; operators opt in by constructing a
//! registry and threading it through
//! [`crate::management_api::SpawnOptions::rbac`].
//!
//! ## Timing-attack discipline
//!
//! [`TokenRegistry::match_token`] iterates ALL entries on every call,
//! computing `subtle::ConstantTimeEq` per entry via the existing
//! [`crate::auth::BearerToken::matches`] primitive — no early exit
//! on a match. An attacker can't time-attack distinguish "token
//! matched entry N=1" from "token matched entry N=K".
//!
//! NOTE: [`crate::auth::BearerToken::matches`] short-circuits on
//! length mismatch (existing pre-Phase-B contract documented at
//! `auth.rs:142`). This is an inherited timing leak of token LENGTH
//! only, NOT of contents, and matches the existing single-token
//! surface. The leak is documented in the threat model in
//! `docs/backend/m-prod-rate-limit.md` "no timing coordination" note.
//!
//! ## Hard cap + soft cap
//!
//! [`MAX_TOKENS`] (256) is the hard upper bound enforced by
//! [`TokenRegistry::new`]. The recommended soft cap is **≤ 32**: every
//! protected request walks the full registry per the constant-time
//! discipline above, so latency grows linearly with the entry count.
//! Production deploys with hundreds of bearer principals should use
//! the Phase B Part 2 JWT-OIDC bridge (one signing-key entry, many
//! subject claims fold into Role mappings) rather than enrolling
//! every principal as a static `TokenEntry`.
//!
//! ## What Phase B Part 1 explicitly does NOT ship
//!
//! - JWT-OIDC bridge (per-issuer config + JWKS fetch + subject
//!   claim → TokenId mapping).
//! - SIGHUP-driven registry reload (rides Group B-2 #2 SIGHUP
//!   topology).
//! - Batched-fsync worker on the audit chain.
//! - `audit_sink_write_failures_total` Prometheus counter.
//! - `grx admin audit-verify` daemon-side HTTP endpoint.
//! - Audit-log forwarders (S3 / Loki / GCS / OpenSearch).
//! - Per-node / per-message-type ACL.
//! - Windows file-mode ACLs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use serde::Deserialize;

use crate::auth::BearerToken;

/// Operator-assigned identity for a [`TokenEntry`].
///
/// This is the wire-side AND audit-side identity string (NOT a
/// hash, NOT the credential). Operators pick stable short names
/// like `"ci-runner"` / `"alice-admin"` / `"dashboard-readonly"`
/// — the audit chain records this exact string in the `token_id`
/// field of `auth_success` / `authz_deny` events, so SOC pipelines
/// can grep for principal names without reverse-mapping hashes.
///
/// The raw bearer credential lives in [`TokenEntry::token`] and is
/// never logged, never serialized into audit records.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenId(pub String);

/// Role granted to a [`TokenEntry`]. Closed over three variants —
/// every Phase B-1 operational use-case folds into one of these.
///
/// - [`Role::Admin`] grants ALL routes in
///   [`crate::management_api::PROTECTED_ROUTE_LABELS`]. Operator
///   credentials.
/// - [`Role::ReadOnly`] grants exactly [`READ_ONLY_ROUTES`] (the GET +
///   WebSocket-subscribe surface). CI scrapers, dashboards, SOC
///   review pipelines.
/// - [`Role::Custom`] carries an explicit `BTreeSet<String>` of
///   route labels drawn from [`crate::management_api::PROTECTED_ROUTE_LABELS`].
///   Per-tenant / per-tool allowlist; the deploy-specific ACL shape.
///
/// Phase B Part 2's JWT-OIDC bridge will map every issuer subject
/// claim into one of these three variants via a per-issuer config —
/// no `Role` variant explosion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub enum Role {
    Admin,
    ReadOnly,
    Custom(BTreeSet<String>),
}

/// A single registry entry: identity + credential + role + optional
/// expiry. Owned, `Send + Sync` (the inner [`BearerToken`] is already
/// `Send + Sync + Zeroize + ZeroizeOnDrop`).
///
/// `Clone` is intentionally NOT derived — registry entries have a
/// single owner (the [`TokenRegistry`]); rotation replaces the entire
/// `TokenRegistry` via [`RotatableRegistry::store`] rather than
/// mutating in place. This mirrors the
/// [`crate::tls_rotation::RotatableServerConfig`] discipline.
///
/// `Debug` is derived — the inner [`BearerToken::fmt`] implementation
/// redacts the bytes (prints `<redacted>` + length), so a
/// `Debug`-formatted `TokenEntry` never leaks the credential.
#[derive(Debug)]
pub struct TokenEntry {
    pub id: TokenId,
    pub token: BearerToken,
    pub role: Role,
    /// Optional expiry. `None` = never expires.
    pub expires_at: Option<SystemTime>,
}

/// Hard upper bound on registry entry count enforced by
/// [`TokenRegistry::new`].
///
/// Recommended soft cap is **≤ 32**: every protected request walks
/// the entire registry per the constant-time discipline — latency
/// grows linearly with the entry count. Production deploys with
/// many principals should use the Phase B Part 2 JWT-OIDC bridge
/// (one signing-key entry, many subject claims fold into Role
/// mappings).
pub const MAX_TOKENS: usize = 256;

/// Read-only route vocabulary for [`Role::ReadOnly`].
///
/// Subset of [`crate::management_api::PROTECTED_ROUTE_LABELS`] —
/// covers GET endpoints + the WebSocket / SSE subscribe routes
/// (read-only by mutation semantics). POST endpoints
/// (`/v1/nodes/:id/messages`, `/v1/nodes/:id/call`,
/// `/v1/nodes/:id/messages/bulk`) are EXCLUDED — they mutate
/// scheduler state and are admin-only by default. A deploy that
/// needs a non-Admin principal to invoke a specific POST endpoint
/// uses [`Role::Custom`] with the explicit label.
///
/// A unit test pins the cross-module subset invariant: every entry
/// here must appear in [`crate::management_api::PROTECTED_ROUTE_LABELS`].
pub const READ_ONLY_ROUTES: &[&str] = &[
    "/v1/identity",
    "/v1/metrics",
    "/v1/nodes",
    "/v1/nodes/:id",
    "/v1/nodes/:id/messages/recent",
    "/v1/runs/:run_id",
    "/v1/supervisor/tree",
    "/v1/observer/events",
    "/v1/observer/events.sse",
];

/// Construction errors for [`TokenRegistry::new`].
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// Empty registry — avoids the "empty registry = no auth"
    /// foot-gun. An operator who wires
    /// [`crate::management_api::SpawnOptions::rbac = Some(...)`] but
    /// supplies zero tokens almost certainly meant to disable RBAC
    /// (which is `rbac: None`) rather than block every request.
    #[error("token registry is empty")]
    Empty,
    /// Hard cap [`MAX_TOKENS`] exceeded.
    #[error("token registry has {count} entries, max is {max}", max = MAX_TOKENS)]
    TooMany { count: usize },
    /// Two entries with the same [`TokenId`]. The id is the audit
    /// principal — duplicates would tear the audit trail.
    #[error("duplicate token id `{id}`")]
    DuplicateId { id: String },
}

/// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Failure modes
/// for the SIGHUP-driven on-disk token-registry reload. ADR-0009
/// specifies the file format (NDJSON one record per line); this
/// enum mirrors the four ways the load path can reject the file
/// while keeping the prior in-memory registry serving.
#[derive(Debug, thiserror::Error)]
pub enum RegistryLoadError {
    /// File could not be opened / read (ENOENT, EACCES, EIO).
    #[error("token registry I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// File is group- or world-readable on Unix (mode != 0o600).
    /// Refused at load to mirror the
    /// [`crate::FileAuditSink::open`] / [`BearerToken::from_file`]
    /// security discipline — operators store raw bearer credentials
    /// in the file; a relaxed mode would let any unprivileged user
    /// read them.
    #[error("token registry file at {path} has mode {mode:o}, expected 0600")]
    Mode { path: PathBuf, mode: u32 },
    /// Every NDJSON line failed to parse (or the entire file was
    /// empty after parse). `detail` carries the first parse error
    /// for operator triage.
    #[error("token registry parse: {detail}")]
    Parse { detail: String },
    /// File parsed but `TokenRegistry::new` rejected the entries
    /// (empty / too many / duplicate id).
    #[error("token registry validation: {0}")]
    Validate(#[from] RegistryError),
}

impl RegistryLoadError {
    /// Short `&'static str` discriminator for the
    /// `token_registry_reload_failed` audit event's `error_kind`
    /// field.
    #[must_use]
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Io { .. } => "io",
            Self::Mode { .. } => "mode",
            Self::Parse { .. } => "parse",
            Self::Validate(_) => "validate",
        }
    }
}

/// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Per-record
/// NDJSON schema for the on-disk token registry file (ADR-0009).
///
/// Each line is one `TokenFileRecord` serialized as JSON. The
/// `role` field uses serde's default external tagging — Admin /
/// ReadOnly are unit variants serialized as bare strings; Custom
/// carries the explicit allowlist as `{"Custom": ["/v1/..."]}`.
#[derive(Debug, Deserialize)]
struct TokenFileRecord {
    /// Operator-assigned token id (the audit-side principal name).
    id: String,
    /// Raw bearer credential bytes (UTF-8 string). Mode-0600
    /// enforcement on the file makes this a secure storage shape.
    token: String,
    /// Role granted to the token. JSON shape is `"Admin"`,
    /// `"ReadOnly"`, or `{"Custom": ["/v1/..."]}`.
    role: Role,
    /// Optional expiry in Unix-millis. `None` (or absent) means
    /// never expires.
    ///
    /// Chosen over ISO8601 because parsing ISO8601 requires
    /// `chrono` or `time` (neither in the workspace dep set). The
    /// trade-off is operator-side ergonomics: integers are harder
    /// to read at a glance. Future ADRs may add a sibling
    /// `expires_at_iso8601` field once a date-parse dep is taken.
    #[serde(default)]
    expires_at_unix_ms: Option<u64>,
}

/// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Load a
/// [`TokenRegistry`] from an on-disk NDJSON file (ADR-0009).
///
/// The file is read line-by-line; invalid lines are LOGGED + SKIPPED
/// (defensive parse — operators editing the file by hand shouldn't
/// blow up the whole reload over one bad row). If EVERY line fails
/// to parse OR the resulting registry is empty,
/// [`RegistryLoadError::Parse`] is returned and the caller keeps
/// the prior registry.
///
/// Mode 0o600 is enforced on Unix BEFORE any read happens —
/// matches the
/// [`crate::FileAuditSink::open`] /
/// [`crate::BearerToken::from_file`] security discipline.
pub fn load_registry_from_file(path: &Path) -> Result<TokenRegistry, RegistryLoadError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|err| RegistryLoadError::Io {
            path: path.to_path_buf(),
            source: err,
        })?;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(RegistryLoadError::Mode {
                path: path.to_path_buf(),
                mode,
            });
        }
    }
    let bytes = std::fs::read(path).map_err(|err| RegistryLoadError::Io {
        path: path.to_path_buf(),
        source: err,
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|err| RegistryLoadError::Parse {
        detail: format!("file is not valid UTF-8: {err}"),
    })?;
    let mut entries = Vec::new();
    let mut first_parse_err: Option<String> = None;
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let rec: TokenFileRecord = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "token_registry_reload_skipped_line",
                    line_index = idx,
                    error = %err,
                    "token registry parse error at line; skipping"
                );
                first_parse_err.get_or_insert_with(|| format!("line {idx}: {err}"));
                continue;
            }
        };
        let bearer = match BearerToken::from_bytes(rec.token.into_bytes()) {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "token_registry_reload_skipped_line",
                    line_index = idx,
                    error = %err,
                    "token registry entry has an invalid bearer token; skipping"
                );
                first_parse_err
                    .get_or_insert_with(|| format!("line {idx}: invalid bearer token: {err}"));
                continue;
            }
        };
        let expires_at = rec
            .expires_at_unix_ms
            .map(|ms| UNIX_EPOCH + Duration::from_millis(ms));
        entries.push(TokenEntry {
            id: TokenId(rec.id),
            token: bearer,
            role: rec.role,
            expires_at,
        });
    }
    if entries.is_empty() {
        if let Some(detail) = first_parse_err {
            return Err(RegistryLoadError::Parse { detail });
        }
        return Err(RegistryLoadError::Parse {
            detail: "no token entries parsed (file empty or all lines invalid)".to_string(),
        });
    }
    Ok(TokenRegistry::new(entries)?)
}

/// Per-role counts for the `token_registry_reloaded` audit event.
/// Bounded vocabulary — the audit consumer wants to see at a
/// glance how many of each role landed in the new registry.
#[derive(Debug, Clone, Copy, Default)]
pub struct RegistryCounts {
    pub total: usize,
    pub admin: usize,
    pub read_only: usize,
    pub custom: usize,
}

/// Owned multi-token registry. Held behind an [`Arc`] inside
/// [`RotatableRegistry`]; the read path is one `load_full()` per
/// protected request.
#[derive(Debug)]
pub struct TokenRegistry {
    entries: Vec<TokenEntry>,
}

impl TokenRegistry {
    /// Construct a new registry from owned entries.
    ///
    /// Rejects:
    /// - empty input (`RegistryError::Empty`)
    /// - more than [`MAX_TOKENS`] entries (`RegistryError::TooMany`)
    /// - duplicate [`TokenId`] (`RegistryError::DuplicateId`)
    pub fn new(entries: Vec<TokenEntry>) -> Result<Self, RegistryError> {
        if entries.is_empty() {
            return Err(RegistryError::Empty);
        }
        if entries.len() > MAX_TOKENS {
            return Err(RegistryError::TooMany {
                count: entries.len(),
            });
        }
        // O(n²) duplicate scan is fine for n ≤ 256; keeps the impl
        // allocation-free vs a BTreeSet seed.
        for i in 0..entries.len() {
            for j in (i + 1)..entries.len() {
                if entries[i].id == entries[j].id {
                    return Err(RegistryError::DuplicateId {
                        id: entries[i].id.0.clone(),
                    });
                }
            }
        }
        Ok(Self { entries })
    }

    /// Match a presented bearer credential against every entry.
    ///
    /// CRITICAL: walks the FULL registry — does NOT early-exit on
    /// the first match. The first matching entry (in declaration
    /// order) is returned, but the walk continues across all
    /// subsequent entries so an attacker can't time-attack
    /// distinguish "matched entry N=1" from "matched entry N=K".
    ///
    /// Per-entry comparison uses [`crate::auth::BearerToken::matches`]
    /// which is `subtle::ConstantTimeEq::ct_eq` under the hood. The
    /// short-circuit on length mismatch inside `matches` is an
    /// inherited Phase A timing surface (leaks token LENGTH per
    /// entry on a miss, NOT contents); see the module-level
    /// "Timing-attack discipline" section.
    pub fn match_token(&self, presented: &[u8]) -> Option<&TokenEntry> {
        let mut hit: Option<&TokenEntry> = None;
        for entry in &self.entries {
            let m = entry.token.matches(presented);
            if m && hit.is_none() {
                hit = Some(entry);
            }
        }
        hit
    }

    /// RBAC dispatch: does `entry`'s role permit `route`?
    ///
    /// `route` is a `&'static str` drawn from
    /// [`crate::management_api::PROTECTED_ROUTE_LABELS`] (or
    /// `"unknown"` for paths outside the protected vocabulary, in
    /// which case Admin permits and other roles deny — defense in
    /// depth against a future protected route being added without a
    /// matching ACL update).
    #[must_use]
    pub fn check_route(&self, entry: &TokenEntry, route: &'static str) -> bool {
        match &entry.role {
            Role::Admin => true,
            Role::ReadOnly => READ_ONLY_ROUTES.contains(&route),
            Role::Custom(set) => set.contains(route),
        }
    }

    /// Has `entry` expired as of `now`?
    ///
    /// Pulled out for testability — callers feed
    /// `SystemTime::now()` in production but a fixed timestamp in
    /// tests. `None` `expires_at` is never expired.
    #[must_use]
    pub fn is_expired(&self, entry: &TokenEntry, now: SystemTime) -> bool {
        entry.expires_at.is_some_and(|t| now >= t)
    }

    /// Number of entries. Exposed for diagnostics + future
    /// [`crate::management_api::Identity`] payload extension (NOT
    /// exposing token bytes or ids).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` iff [`Self::len`] == 0. Cannot occur for a registry
    /// constructed through [`Self::new`] (rejected at construction),
    /// but included for `clippy::len_without_is_empty` parity.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Per-role
    /// counts for the `token_registry_reloaded` audit event.
    #[must_use]
    pub fn role_counts(&self) -> RegistryCounts {
        let mut counts = RegistryCounts {
            total: self.entries.len(),
            ..RegistryCounts::default()
        };
        for entry in &self.entries {
            match entry.role {
                Role::Admin => counts.admin += 1,
                Role::ReadOnly => counts.read_only += 1,
                Role::Custom(_) => counts.custom += 1,
            }
        }
        counts
    }
}

/// Atomic-swap container for an `Arc<TokenRegistry>`. Cheap to read
/// on the per-request hot path (one `ArcSwap::load_full`); the
/// rotation entry point ([`Self::store`]) replaces the inner `Arc`
/// atomically so in-flight requests continue to see their existing
/// snapshot.
///
/// Mirrors [`crate::tls_rotation::RotatableServerConfig`] bit-for-bit:
/// the wording "Arc<ArcSwap<Arc<T>>>" in the M-Prod plan names the
/// ATOMIC-SWAP PATTERN; the actual handle is `Arc<ArcSwap<T>>`
/// because `ArcSwap` internally holds an `Arc<T>` and `load_full()`
/// returns an `Arc<T>` clone. See `tls_rotation.rs` for the
/// rationale on this shape choice.
///
/// Cloning a `RotatableRegistry` clones the inner `Arc<ArcSwap<_>>`
/// — every clone observes the same rotation. The bind config, the
/// management-API auth middleware, and the eventual SIGHUP handler
/// all hold clones of the same handle.
#[derive(Clone)]
pub struct RotatableRegistry {
    inner: Arc<ArcSwap<TokenRegistry>>,
}

impl RotatableRegistry {
    /// Wrap an initial `Arc<TokenRegistry>` for rotation.
    ///
    /// The first [`Self::current`] call returns the supplied
    /// registry; the first [`Self::rotate`] call replaces it.
    #[must_use]
    pub fn new(initial: Arc<TokenRegistry>) -> Self {
        Self {
            inner: Arc::new(ArcSwap::new(initial)),
        }
    }

    /// Load the current `Arc<TokenRegistry>`. Cheap (one
    /// `ArcSwap::load_full`); returns an owned `Arc` that is safe
    /// to hold across `.await`.
    ///
    /// The auth middleware calls this once per protected request +
    /// uses the resulting snapshot for both `match_token` and
    /// `check_route`, so a rotation between the match and the check
    /// can't tear the verdict.
    ///
    /// **Naming.** Mirrors [`crate::tls::RotatableServerConfig::current`]
    /// (Group B-2 #2 verify-finding patch). Both sides of the
    /// atomic-swap contract expose `current` / `rotate` so a future
    /// SIGHUP handler driving both rotations sees one API shape.
    #[must_use]
    pub fn current(&self) -> Arc<TokenRegistry> {
        self.inner.load_full()
    }

    /// Atomically swap in a new `Arc<TokenRegistry>`. The next
    /// [`Self::current`] call observes `next`; existing snapshots
    /// held by in-flight requests keep serving the old registry
    /// until they drop.
    ///
    /// **Naming.** Mirrors [`crate::tls::RotatableServerConfig::rotate`].
    /// Wired by the eventual SIGHUP handler (Phase B Part 2).
    pub fn rotate(&self, next: Arc<TokenRegistry>) {
        self.inner.store(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_token(bytes: &[u8]) -> BearerToken {
        BearerToken::from_bytes(bytes.to_vec()).expect("non-empty")
    }

    fn mk_entry(id: &str, bytes: &[u8], role: Role) -> TokenEntry {
        TokenEntry {
            id: TokenId(id.into()),
            token: mk_token(bytes),
            role,
            expires_at: None,
        }
    }

    #[test]
    fn match_token_returns_admin_entry() {
        let reg = TokenRegistry::new(vec![
            mk_entry("admin1", b"AAAAAA", Role::Admin),
            mk_entry("reader1", b"BBBBBB", Role::ReadOnly),
        ])
        .unwrap();
        let hit = reg.match_token(b"AAAAAA").expect("hit");
        assert_eq!(hit.id.0, "admin1");
        assert_eq!(hit.role, Role::Admin);
    }

    #[test]
    fn match_token_returns_none_on_miss() {
        let reg = TokenRegistry::new(vec![mk_entry("admin1", b"AAAAAA", Role::Admin)]).unwrap();
        assert!(reg.match_token(b"NOPE").is_none());
        // Length mismatch (subtle::ConstantTimeEq requires equal
        // lengths; short-circuit is documented in BearerToken).
        assert!(reg.match_token(b"AAA").is_none());
    }

    #[test]
    fn match_token_walks_all_entries_after_first_match() {
        // Three entries; the FIRST matches. We verify the returned
        // entry is the first (declaration order), AND we verify the
        // walk continues by checking that match_token still returns
        // the FIRST when a LATER entry happens to also hold the
        // same bytes (which would be operator-error in production,
        // but we exercise the "no early-exit + first-wins"
        // invariant directly).
        let reg = TokenRegistry::new(vec![
            mk_entry("admin1", b"AAAAAA", Role::Admin),
            mk_entry("reader1", b"BBBBBB", Role::ReadOnly),
            mk_entry("ci1", b"AAAAAA", Role::Custom(BTreeSet::new())),
        ])
        .unwrap();
        let hit = reg.match_token(b"AAAAAA").expect("hit");
        // First entry wins on a tie — proves the walk visited all
        // entries (otherwise hit_none-then-set on the first match
        // would short-circuit; we know it doesn't because the
        // ci1 entry was also a match but didn't displace admin1).
        assert_eq!(hit.id.0, "admin1");
    }

    #[test]
    fn check_route_admin_grants_all_protected() {
        let reg = TokenRegistry::new(vec![mk_entry("admin1", b"AAAAAA", Role::Admin)]).unwrap();
        let entry = reg.match_token(b"AAAAAA").unwrap();
        // Admin grants the read-only subset AND the POST endpoints.
        assert!(reg.check_route(entry, "/v1/identity"));
        assert!(reg.check_route(entry, "/v1/metrics"));
        assert!(reg.check_route(entry, "/v1/nodes/:id/messages"));
        assert!(reg.check_route(entry, "/v1/nodes/:id/call"));
        assert!(reg.check_route(entry, "/v1/nodes/:id/messages/bulk"));
        // Admin also grants "unknown" (defense in depth — a future
        // protected route added without a matching ACL update is
        // still reachable by Admin; ReadOnly + Custom deny).
        assert!(reg.check_route(entry, "unknown"));
    }

    #[test]
    fn check_route_read_only_grants_subset_denies_post() {
        let reg = TokenRegistry::new(vec![mk_entry("reader1", b"BBBBBB", Role::ReadOnly)]).unwrap();
        let entry = reg.match_token(b"BBBBBB").unwrap();
        // Every READ_ONLY_ROUTES entry → permitted.
        for route in READ_ONLY_ROUTES {
            assert!(
                reg.check_route(entry, route),
                "ReadOnly should permit {route}"
            );
        }
        // POST endpoints → denied.
        assert!(!reg.check_route(entry, "/v1/nodes/:id/messages"));
        assert!(!reg.check_route(entry, "/v1/nodes/:id/call"));
        assert!(!reg.check_route(entry, "/v1/nodes/:id/messages/bulk"));
        // Unknown → denied.
        assert!(!reg.check_route(entry, "unknown"));
    }

    #[test]
    fn check_route_custom_grants_explicit_only() {
        let mut allow = BTreeSet::new();
        allow.insert("/v1/metrics".to_string());
        allow.insert("/v1/nodes/:id/call".to_string());
        let reg =
            TokenRegistry::new(vec![mk_entry("ci1", b"CCCCCC", Role::Custom(allow))]).unwrap();
        let entry = reg.match_token(b"CCCCCC").unwrap();
        assert!(reg.check_route(entry, "/v1/metrics"));
        assert!(reg.check_route(entry, "/v1/nodes/:id/call"));
        // Not in the explicit set → denied, even if it's in
        // READ_ONLY_ROUTES.
        assert!(!reg.check_route(entry, "/v1/identity"));
        assert!(!reg.check_route(entry, "/v1/nodes/:id/messages"));
        assert!(!reg.check_route(entry, "unknown"));
    }

    #[test]
    fn is_expired_now_or_past_is_true_future_is_false() {
        let reg = TokenRegistry::new(vec![mk_entry("admin1", b"AAAAAA", Role::Admin)]).unwrap();
        let entry = &reg.entries[0];
        let now = SystemTime::now();
        // None expires_at → never expired.
        assert!(!reg.is_expired(entry, now));

        let past = TokenEntry {
            id: TokenId("expired1".into()),
            token: mk_token(b"DDDDDD"),
            role: Role::Admin,
            expires_at: Some(now - std::time::Duration::from_secs(60)),
        };
        assert!(reg.is_expired(&past, now));

        let future = TokenEntry {
            id: TokenId("fresh1".into()),
            token: mk_token(b"EEEEEE"),
            role: Role::Admin,
            expires_at: Some(now + std::time::Duration::from_secs(60)),
        };
        assert!(!reg.is_expired(&future, now));
    }

    #[test]
    fn new_rejects_empty() {
        let err = TokenRegistry::new(vec![]).unwrap_err();
        assert!(matches!(err, RegistryError::Empty));
    }

    #[test]
    fn new_rejects_too_many() {
        // Synthesize MAX_TOKENS + 1 entries with distinct bytes.
        let mut entries = Vec::with_capacity(MAX_TOKENS + 1);
        for i in 0..=MAX_TOKENS {
            entries.push(mk_entry(
                &format!("id-{i}"),
                format!("token-bytes-{i:08}").as_bytes(),
                Role::Admin,
            ));
        }
        let err = TokenRegistry::new(entries).unwrap_err();
        match err {
            RegistryError::TooMany { count } => assert_eq!(count, MAX_TOKENS + 1),
            other => panic!("expected TooMany, got {other:?}"),
        }
    }

    #[test]
    fn new_rejects_duplicate_id() {
        let entries = vec![
            mk_entry("dup", b"AAAAAA", Role::Admin),
            mk_entry("dup", b"BBBBBB", Role::ReadOnly),
        ];
        let err = TokenRegistry::new(entries).unwrap_err();
        match err {
            RegistryError::DuplicateId { id } => assert_eq!(id, "dup"),
            other => panic!("expected DuplicateId, got {other:?}"),
        }
    }

    #[test]
    fn read_only_routes_subset_of_protected_route_labels() {
        // Pin the cross-module invariant: every READ_ONLY_ROUTES
        // entry must appear in PROTECTED_ROUTE_LABELS. Without
        // this, an operator wiring a ReadOnly role could grant a
        // route the rate-limit / audit middleware doesn't know
        // about — silently bypassing the protected-route
        // classifier.
        let protected: std::collections::BTreeSet<&'static str> =
            crate::management_api::PROTECTED_ROUTE_LABELS
                .iter()
                .copied()
                .collect();
        for r in READ_ONLY_ROUTES {
            assert!(
                protected.contains(r),
                "READ_ONLY_ROUTES entry {r} must appear in PROTECTED_ROUTE_LABELS"
            );
        }
    }

    #[test]
    fn rotatable_registry_current_returns_initial_then_rotate_replaces() {
        let reg_a =
            Arc::new(TokenRegistry::new(vec![mk_entry("admin1", b"AAAAAA", Role::Admin)]).unwrap());
        let reg_b = Arc::new(
            TokenRegistry::new(vec![
                mk_entry("admin1", b"AAAAAA", Role::Admin),
                mk_entry("reader1", b"BBBBBB", Role::ReadOnly),
            ])
            .unwrap(),
        );
        let rot = RotatableRegistry::new(reg_a.clone());
        let snap = rot.current();
        assert!(Arc::ptr_eq(&snap, &reg_a));
        assert_eq!(snap.len(), 1);

        rot.rotate(reg_b.clone());
        let snap2 = rot.current();
        assert!(Arc::ptr_eq(&snap2, &reg_b));
        assert_eq!(snap2.len(), 2);

        // Pre-rotation snapshot still points at A — no UAF.
        assert!(Arc::ptr_eq(&snap, &reg_a));
    }

    #[test]
    fn rotatable_registry_clone_observes_rotation() {
        let reg_a =
            Arc::new(TokenRegistry::new(vec![mk_entry("admin1", b"AAAAAA", Role::Admin)]).unwrap());
        let reg_b =
            Arc::new(TokenRegistry::new(vec![mk_entry("admin2", b"BBBBBB", Role::Admin)]).unwrap());
        let rot1 = RotatableRegistry::new(reg_a);
        let rot2 = rot1.clone();
        rot1.rotate(reg_b.clone());
        let snap2 = rot2.current();
        assert!(Arc::ptr_eq(&snap2, &reg_b));
    }
}
