//! **M-Prod sub-track #1 Phase B Part 2 (Group 1c).** JWT-OIDC
//! bridge with file-based JWKS.
//!
//! Bridges external OIDC IdP-issued JWTs (Okta / Auth0 / Keycloak /
//! AWS Cognito / Google / GitHub etc.) into the B-3 multi-token
//! RBAC dispatch by mapping JWT claims onto the existing
//! [`crate::rbac::Role::Admin`] / [`crate::rbac::Role::ReadOnly`] /
//! [`crate::rbac::Role::Custom`] vocabulary. This group ships the
//! IN-PROCESS half of the bridge; HTTP-based JWKS auto-fetch is
//! deferred to γ.next (operator runs a sidecar/cron that writes
//! the JWKS JSON to disk; daemon reads the file on SIGHUP riding
//! the Group 1ab fan-out).
//!
//! ## Atomic-swap rotation
//!
//! [`RotatableJwks`] mirrors [`crate::rbac::RotatableRegistry`]
//! bit-for-bit: an `Arc<ArcSwap<JwksStore>>` newtype with cheap
//! `current()` reads on the per-request JWT-verify hot path and
//! atomic `rotate(...)` writes from the SIGHUP handler. The
//! wording "Arc<ArcSwap<Arc<T>>>" in the M-Prod plan names the
//! ATOMIC-SWAP PATTERN; the actual handle is `Arc<ArcSwap<T>>`
//! because `ArcSwap` internally holds an `Arc<T>` and
//! `load_full()` returns an `Arc<T>` clone.
//!
//! ## Constant-time fast path
//!
//! [`crate::management_api::auth_layer_rbac`] dispatches to
//! [`JwtVerifier::verify`] only when the bearer candidate starts
//! with `eyJ` (the base64url-encoded `{` opener of every
//! JWT header). Static tokens that don't start with `eyJ`
//! short-circuit BEFORE any crypto runs. JWT-shaped bearers go
//! through the jsonwebtoken validation pipeline (constant-time
//! signature verification is the underlying crypto library's
//! contract).
//!
//! ## Threat model & default-deny posture
//!
//! - Algorithm whitelist: only operator-approved algorithms are
//!   accepted (default RS256 + ES256). NEVER include `none` or
//!   any HMAC algorithm — HMAC keys are symmetric secrets and
//!   the JWKS file is operator-public by design.
//! - Required spec claims: `exp`, `iss`, `aud`, `sub` are ALL
//!   required by default (not optional per RFC 7519). A token
//!   missing any of them fails verify with `MissingClaim`.
//! - Empty `sub` is rejected even when the JWT is otherwise
//!   valid — TokenId derivation needs a non-empty principal.
//! - Default-deny on role mapping: a JWT whose role-claim value
//!   is not in `admin_values` / `read_only_values` /
//!   `custom_role_mappings` is denied (no implicit Role fallback).
//!
//! ## What Group 1c explicitly does NOT ship
//!
//! - HTTP-based JWKS auto-fetch (`/.well-known/jwks.json`
//!   polling). Operators sidecar-write the JWKS JSON to disk and
//!   fire SIGHUP. γ.next can layer auto-fetch on top.
//! - JWE (encrypted JWTs). JWS-signed JWTs only.
//! - Per-issuer JWKS files. One JWKS file maps to one daemon;
//!   multi-issuer deploys union the keys into one file.
//! - Dynamic role mapping (e.g. computing a Role from a Lua/CEL
//!   expression). Today's mapping is exact-string match over
//!   the `role_claim` value.
//! - Live config reload. The bridge config is captured at boot;
//!   SIGHUP reloads JWKS but does NOT re-read the config file.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use jsonwebtoken::errors::ErrorKind as JwtErrorKind;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::rbac::Role;

/// Default clock-skew leeway in seconds applied to `exp` / `nbf`.
/// Matches jsonwebtoken's own default; RFC 7519 §4.1.4 recommends
/// "a few minutes" of slack — 60s is conservative and covers
/// NTP-synced fleets without opening a meaningful attack window.
pub const DEFAULT_JWT_LEEWAY_SECONDS: u64 = 60;

/// Decoded + validated JWT principal handed back from
/// [`JwtVerifier::verify`]. Carries the audit-side identity fields
/// (sub / iss / kid / exp) plus the parsed claims map so the
/// role-mapping layer can read operator-configured claim names.
#[derive(Debug, Clone)]
pub struct VerifiedJwt {
    /// `sub` claim — the audit-side principal name; becomes the
    /// [`crate::rbac::TokenId`] inner string. Validated non-empty
    /// at verify time.
    pub sub: String,
    /// `iss` claim — used in the `jwt_auth_success` audit event.
    pub iss: String,
    /// `kid` from the JWT header (always `Some` — Group 1c
    /// requires `kid` per the IdP key-rotation discipline).
    pub kid: String,
    /// `exp` claim as Unix-milliseconds — denormalized for the
    /// audit emit (avoids re-multiplying by 1000 at every call
    /// site).
    pub exp_unix_ms: i64,
    /// Full parsed claims map. The role-mapping layer reads
    /// `claims.get(&cfg.role_claim)` to derive a [`Role`] without
    /// hard-coding the claim name.
    pub claims: BTreeMap<String, serde_json::Value>,
}

/// JWT verification failure modes. Each variant maps onto a short
/// stable `&'static str` via [`Self::reason_str`] — that tag is
/// the closed-vocabulary `reason` field on the `jwt_auth_fail`
/// audit event, so SOC pipelines can grep failure shapes without
/// regex-matching free-form error strings.
#[derive(Debug, Clone, thiserror::Error)]
pub enum JwtVerifyError {
    /// Bearer didn't start with `eyJ` or otherwise failed the
    /// base64url-three-segments shape (RFC 7519 §7.1).
    #[error("JWT is malformed")]
    MalformedJwt,
    /// JWT header lacked the `kid` field. Group 1c always requires
    /// `kid` — IdPs publishing OLD+NEW keys side-by-side during
    /// rotation MUST set `kid` on the signing key.
    #[error("JWT header missing `kid`")]
    MissingKid,
    /// JWT header carried a `kid` not present in the JWKS file.
    /// Either the IdP rotated keys faster than the operator
    /// sidecar refreshed the file, or the token was minted by
    /// a different issuer than expected.
    #[error("JWT `kid` not in JWKS")]
    UnknownKid,
    /// JWT header `alg` is not in the operator-configured
    /// `allowed_algorithms` whitelist. Defense against
    /// algorithm-substitution attacks (e.g. `none`, `HS256` on
    /// an RSA key).
    #[error("JWT algorithm not allowed")]
    AlgorithmNotAllowed,
    /// Signature verification failed.
    #[error("JWT signature invalid")]
    InvalidSignature,
    /// `exp` claim is in the past (after leeway).
    #[error("JWT expired")]
    ExpiredSignature,
    /// `nbf` (not-before) claim is in the future (after leeway).
    #[error("JWT not yet valid (nbf)")]
    NotYetValid,
    /// `iss` claim is not in the operator-configured
    /// `expected_issuers` list.
    #[error("JWT issuer not allowed")]
    InvalidIssuer,
    /// `aud` claim does not include any of the operator-configured
    /// `expected_audiences`.
    #[error("JWT audience not allowed")]
    InvalidAudience,
    /// A required spec claim (`exp` / `iss` / `aud` / `sub`) was
    /// missing or, in the case of `sub`, empty.
    #[error("JWT missing required claim: {name}")]
    MissingClaim { name: &'static str },
}

impl JwtVerifyError {
    /// Short `&'static str` tag emitted as the `reason` field on
    /// the `jwt_auth_fail` audit event. Closed-vocabulary so SOC
    /// pipelines can grep on stable tags rather than free-form
    /// `Display` strings.
    #[must_use]
    pub fn reason_str(&self) -> &'static str {
        match self {
            Self::MalformedJwt => "malformed_jwt",
            Self::MissingKid => "missing_kid",
            Self::UnknownKid => "unknown_kid",
            Self::AlgorithmNotAllowed => "algorithm_not_allowed",
            Self::InvalidSignature => "invalid_signature",
            Self::ExpiredSignature => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::InvalidIssuer => "invalid_issuer",
            Self::InvalidAudience => "invalid_audience",
            Self::MissingClaim { .. } => "missing_claim",
        }
    }
}

fn map_jwt_err(err: jsonwebtoken::errors::Error) -> JwtVerifyError {
    match err.kind() {
        JwtErrorKind::ExpiredSignature => JwtVerifyError::ExpiredSignature,
        JwtErrorKind::ImmatureSignature => JwtVerifyError::NotYetValid,
        JwtErrorKind::InvalidSignature => JwtVerifyError::InvalidSignature,
        JwtErrorKind::InvalidIssuer => JwtVerifyError::InvalidIssuer,
        JwtErrorKind::InvalidAudience => JwtVerifyError::InvalidAudience,
        JwtErrorKind::InvalidAlgorithm
        | JwtErrorKind::InvalidAlgorithmName
        | JwtErrorKind::MissingAlgorithm => JwtVerifyError::AlgorithmNotAllowed,
        JwtErrorKind::MissingRequiredClaim(name) => {
            // Convert jsonwebtoken's `String` claim name to a
            // `&'static str` over the known closed set so the
            // audit emit stays bounded-cardinality.
            let stable: &'static str = match name.as_str() {
                "exp" => "exp",
                "iss" => "iss",
                "aud" => "aud",
                "sub" => "sub",
                "nbf" => "nbf",
                _ => "other",
            };
            JwtVerifyError::MissingClaim { name: stable }
        }
        _ => JwtVerifyError::MalformedJwt,
    }
}

/// On-disk JWKS load failure modes. Mirrors
/// [`crate::rbac::RegistryLoadError`] shape — typed error enum
/// with `kind_str() -> &'static str` discriminator for the
/// audit emit's `error_kind` field. NO `Mode` variant per
/// ADR-0010: the JWKS file holds PUBLIC keys + is not mode-0o600
/// enforced.
#[derive(Debug, thiserror::Error)]
pub enum JwksLoadError {
    /// File could not be opened / read.
    #[error("JWKS I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// File contents did not parse as a JWKS JSON envelope.
    #[error("JWKS parse at {path}: {detail}")]
    Parse { path: PathBuf, detail: String },
    /// Parsed but contained zero usable keys (after per-key skip-
    /// and-warn for unsupported curves / missing kid / etc.).
    #[error("JWKS at {path} has no usable keys")]
    Empty { path: PathBuf },
    /// Two keys carry the same `kid`. Lookup is by `kid` alone,
    /// so duplicates would create a silent fallback that
    /// operator-facing audit cannot reason about.
    #[error("JWKS at {path} has duplicate kid `{kid}`")]
    DuplicateKid { path: PathBuf, kid: String },
}

impl JwksLoadError {
    /// Short closed-vocabulary tag for the
    /// `jwks_reload_failed.error_kind` audit field. Mirrors
    /// [`crate::rbac::RegistryLoadError::kind_str`].
    #[must_use]
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Io { .. } => "io",
            Self::Parse { .. } => "parse",
            Self::Empty { .. } => "empty",
            Self::DuplicateKid { .. } => "duplicate_kid",
        }
    }
}

/// Single key in a [`JwksStore`] — the raw [`Jwk`] plus its
/// pre-decoded [`DecodingKey`] so the per-request verify hot path
/// avoids the per-key ASN.1 decode cost.
pub struct JwksKey {
    /// Raw JWK as loaded from disk. Carries `kid`, `alg`, `kty`,
    /// and the kty-specific parameters (RSA `n`/`e`, EC
    /// `crv`/`x`/`y`, etc.).
    pub jwk: Jwk,
    /// `DecodingKey` produced once at JwksStore load time via
    /// `DecodingKey::from_jwk`. The per-request verify path just
    /// calls `decode::<Claims>(token, &key, &validation)` with
    /// this reference — no per-request key material extraction.
    pub decoding: DecodingKey,
}

impl std::fmt::Debug for JwksKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwksKey")
            .field("kid", &self.jwk.common.key_id)
            .field("alg", &self.jwk.common.key_algorithm)
            .finish()
    }
}

/// Loaded JWKS file, ready for per-request kid lookup.
///
/// Constructed via [`load_jwks_from_file`] or
/// [`Self::from_jwk_set`]. Held behind [`RotatableJwks`] so SIGHUP
/// reloads atomically replace the in-memory store without tearing
/// in-flight verifies.
#[derive(Debug)]
pub struct JwksStore {
    by_kid: HashMap<String, JwksKey>,
}

impl JwksStore {
    /// Build a [`JwksStore`] from an already-parsed [`JwkSet`].
    /// Used by [`load_jwks_from_file`] and by tests that mint
    /// JWKS in-memory.
    ///
    /// Per-key discipline (mirrors RFC 7517 §4.5):
    /// - Keys without `kid` are SKIPPED + warned (no
    ///   silent-first-key fallback).
    /// - Keys whose `DecodingKey::from_jwk` returns Err
    ///   (unsupported curve etc.) are SKIPPED + warned.
    /// - Empty store after the skip pass returns
    ///   [`JwksLoadError::Empty`].
    /// - Two keys with the same `kid` return
    ///   [`JwksLoadError::DuplicateKid`].
    pub fn from_jwk_set(set: JwkSet, source_path: &Path) -> Result<Self, JwksLoadError> {
        let mut by_kid: HashMap<String, JwksKey> = HashMap::new();
        for jwk in set.keys {
            let Some(kid) = jwk.common.key_id.clone() else {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "jwks_skipped_key",
                    reason = "missing_kid",
                    "JWKS key skipped: no kid"
                );
                continue;
            };
            let decoding = match DecodingKey::from_jwk(&jwk) {
                Ok(k) => k,
                Err(err) => {
                    tracing::warn!(
                        target: "graphreact::observer::audit",
                        event = "jwks_skipped_key",
                        reason = "decode",
                        kid = %kid,
                        error = %err,
                        "JWKS key skipped: from_jwk failed"
                    );
                    continue;
                }
            };
            if by_kid.contains_key(&kid) {
                return Err(JwksLoadError::DuplicateKid {
                    path: source_path.to_path_buf(),
                    kid,
                });
            }
            by_kid.insert(kid, JwksKey { jwk, decoding });
        }
        if by_kid.is_empty() {
            return Err(JwksLoadError::Empty {
                path: source_path.to_path_buf(),
            });
        }
        Ok(Self { by_kid })
    }

    /// Build an empty in-memory store. Used by tests + by
    /// [`RotatableJwks`] sentinels in code paths that need a
    /// non-`Option<...>` handle before a real load happens.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            by_kid: HashMap::new(),
        }
    }

    /// Look up a signing key by `kid`. O(1).
    #[must_use]
    pub fn find_key_by_kid(&self, kid: &str) -> Option<&JwksKey> {
        self.by_kid.get(kid)
    }

    /// Number of keys in the store. Emitted as `key_count` on
    /// the `jwks_reloaded` audit event.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.by_kid.len()
    }

    /// Number of distinct `kid` values. By construction equal to
    /// [`Self::key_count`] (duplicate-kid is a load-time error);
    /// kept as a distinct accessor so the audit field stays
    /// self-documenting under future schema evolution.
    #[must_use]
    pub fn kid_count(&self) -> usize {
        self.by_kid.len()
    }
}

/// Load a [`JwksStore`] from an on-disk JWKS JSON file (RFC 7517
/// §5). Reads the file, parses as [`JwkSet`], builds the kid →
/// JwksKey map via [`JwksStore::from_jwk_set`].
///
/// NO mode-0o600 enforcement: JWKS holds PUBLIC keys (by RFC 7517
/// convention) and operators may legitimately want world-readable
/// permissions for sidecar-style refresh patterns. Documented in
/// ADR-0010 as a deliberate deviation from the audit-log + token-
/// registry 0o600 rule.
pub fn load_jwks_from_file(path: &Path) -> Result<JwksStore, JwksLoadError> {
    let bytes = std::fs::read(path).map_err(|err| JwksLoadError::Io {
        path: path.to_path_buf(),
        source: err,
    })?;
    let set: JwkSet = serde_json::from_slice(&bytes).map_err(|err| JwksLoadError::Parse {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    JwksStore::from_jwk_set(set, path)
}

/// Atomic-swap container for an `Arc<JwksStore>`. Cheap to read
/// on the per-request JWT-verify hot path (one
/// `ArcSwap::load_full`); the rotation entry point
/// ([`Self::rotate`]) replaces the inner `Arc` atomically so
/// in-flight verifies continue to see their existing snapshot.
///
/// Mirrors [`crate::rbac::RotatableRegistry`] /
/// [`crate::tls_rotation::RotatableServerConfig`] bit-for-bit:
/// the wording "Arc<ArcSwap<Arc<T>>>" in the M-Prod plan names
/// the ATOMIC-SWAP PATTERN; the actual handle is
/// `Arc<ArcSwap<T>>` because `ArcSwap` internally holds an
/// `Arc<T>` and `load_full()` returns an `Arc<T>` clone.
///
/// Cloning a `RotatableJwks` clones the inner `Arc<ArcSwap<_>>`
/// — every clone observes the same rotation. The [`JwtVerifier`]
/// holds a clone; the daemon-side SIGHUP handler holds another
/// clone; both see the same atomic-swap.
#[derive(Clone)]
pub struct RotatableJwks {
    inner: Arc<ArcSwap<JwksStore>>,
}

impl RotatableJwks {
    /// Wrap an initial `Arc<JwksStore>` for rotation. The first
    /// [`Self::current`] call returns the supplied store; the
    /// first [`Self::rotate`] call replaces it.
    #[must_use]
    pub fn new(initial: Arc<JwksStore>) -> Self {
        Self {
            inner: Arc::new(ArcSwap::new(initial)),
        }
    }

    /// Load the current `Arc<JwksStore>`. Cheap (one
    /// `ArcSwap::load_full`); returns an owned `Arc` safe to
    /// hold across `.await`.
    ///
    /// Naming mirrors [`crate::rbac::RotatableRegistry::current`].
    /// [`JwtVerifier::verify`] snapshots ONCE per request and
    /// uses the result for both kid-lookup and signature verify
    /// so a concurrent rotation between lookup and verify can't
    /// tear a verdict.
    #[must_use]
    pub fn current(&self) -> Arc<JwksStore> {
        self.inner.load_full()
    }

    /// Atomically swap in a new `Arc<JwksStore>`. The next
    /// [`Self::current`] call observes `next`; existing snapshots
    /// held by in-flight verifies keep serving the old store
    /// until they drop.
    ///
    /// Naming mirrors [`crate::rbac::RotatableRegistry::rotate`].
    /// Wired by the daemon SIGHUP fan-out.
    pub fn rotate(&self, next: Arc<JwksStore>) {
        self.inner.store(next);
    }
}

/// Operator-configured claim → [`Role`] mapping. The bridge's
/// authorization edge: every JWT that verifies cleanly must map
/// to one of the three [`Role`] variants OR be denied.
///
/// Default-deny posture: a JWT whose `role_claim` value matches
/// none of `admin_values` / `read_only_values` /
/// `custom_role_mappings` returns
/// [`ClaimMappingError::NoRoleClaimMatch`] and the bridge denies
/// the request.
#[derive(Debug, Clone, Deserialize)]
pub struct RoleMapping {
    /// JWT claim name to inspect. Common values: `"role"`,
    /// `"roles"`, `"groups"`, or a namespaced URI like
    /// `"https://example.com/role"` (Auth0 pattern).
    pub role_claim: String,
    /// Claim values that map to [`Role::Admin`]. Exact match
    /// against either a JSON string value OR (when the claim is
    /// a JSON array) any element of the array.
    #[serde(default)]
    pub admin_values: Vec<String>,
    /// Claim values that map to [`Role::ReadOnly`].
    #[serde(default)]
    pub read_only_values: Vec<String>,
    /// Per-claim-value [`Role::Custom`] ACL sets. The mapped
    /// `BTreeSet<String>` becomes the [`Role::Custom`] payload.
    #[serde(default)]
    pub custom_role_mappings: HashMap<String, BTreeSet<String>>,
}

/// Failure modes for [`derive_role`]. Today's bridge uses one
/// variant; surface kept as a typed enum so future mappers can
/// distinguish "claim missing" from "claim wrong type" without
/// schema bumps downstream.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ClaimMappingError {
    /// JWT verified cleanly but no value at `role_claim` matched
    /// `admin_values` / `read_only_values` /
    /// `custom_role_mappings`. Bridge denies the request.
    #[error("no role claim match")]
    NoRoleClaimMatch,
}

impl ClaimMappingError {
    /// Short stable tag for the `jwt_auth_fail.reason` audit
    /// field.
    #[must_use]
    pub fn reason_str(&self) -> &'static str {
        match self {
            Self::NoRoleClaimMatch => "no_role_claim_match",
        }
    }
}

/// Derive a [`Role`] from a [`VerifiedJwt`] under the given
/// mapping. Reads `verified.claims.get(&mapping.role_claim)`:
/// - If the value is a JSON string → exact-match against the
///   allow-lists.
/// - If the value is a JSON array → ANY-match wins (admin >
///   read_only > custom precedence).
/// - If absent or any-other-type → returns
///   [`ClaimMappingError::NoRoleClaimMatch`] (default-deny).
pub fn derive_role(
    verified: &VerifiedJwt,
    mapping: &RoleMapping,
) -> Result<Role, ClaimMappingError> {
    let Some(claim_val) = verified.claims.get(&mapping.role_claim) else {
        return Err(ClaimMappingError::NoRoleClaimMatch);
    };
    // Collect candidate values (one for string, many for array).
    let mut candidates: Vec<&str> = Vec::new();
    match claim_val {
        serde_json::Value::String(s) => candidates.push(s.as_str()),
        serde_json::Value::Array(arr) => {
            for v in arr {
                if let Some(s) = v.as_str() {
                    candidates.push(s);
                }
            }
        }
        _ => return Err(ClaimMappingError::NoRoleClaimMatch),
    }
    if candidates.is_empty() {
        return Err(ClaimMappingError::NoRoleClaimMatch);
    }
    // Precedence: admin > read_only > custom.
    for c in &candidates {
        if mapping.admin_values.iter().any(|v| v == c) {
            return Ok(Role::Admin);
        }
    }
    for c in &candidates {
        if mapping.read_only_values.iter().any(|v| v == c) {
            return Ok(Role::ReadOnly);
        }
    }
    for c in &candidates {
        if let Some(routes) = mapping.custom_role_mappings.get(*c) {
            return Ok(Role::Custom(routes.clone()));
        }
    }
    Err(ClaimMappingError::NoRoleClaimMatch)
}

/// Operator-supplied configuration for the JWT-OIDC bridge.
/// Captured at boot from a JSON file (`--observer-jwt-config
/// <PATH>`) and held inside [`JwtVerifier`]; changes require a
/// daemon restart (SIGHUP reloads JWKS only).
#[derive(Debug, Clone, Deserialize)]
pub struct JwtOidcConfig {
    /// Acceptable issuers — a JWT's `iss` claim must match one of
    /// these exactly (case-sensitive).
    pub expected_issuers: Vec<String>,
    /// Acceptable audiences — a JWT's `aud` claim must contain
    /// at least one of these.
    pub expected_audiences: Vec<String>,
    /// Algorithm whitelist. Default RS256 + ES256. MUST NOT
    /// include `Algorithm::None` or any HMAC variant (HMAC keys
    /// in a public JWKS would be a credential leak).
    #[serde(default = "default_algorithms")]
    pub allowed_algorithms: Vec<Algorithm>,
    /// Clock-skew leeway in seconds applied to `exp` / `nbf`.
    /// Defaults to [`DEFAULT_JWT_LEEWAY_SECONDS`] (60s).
    #[serde(default = "default_leeway")]
    pub leeway_seconds: u64,
    /// Claim → Role mapping.
    pub role_mapping: RoleMapping,
}

fn default_algorithms() -> Vec<Algorithm> {
    vec![Algorithm::RS256, Algorithm::ES256]
}

fn default_leeway() -> u64 {
    DEFAULT_JWT_LEEWAY_SECONDS
}

/// On-disk config load failures.
#[derive(Debug, thiserror::Error)]
pub enum JwtOidcConfigLoadError {
    #[error("JWT OIDC config I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("JWT OIDC config parse at {path}: {detail}")]
    Parse { path: PathBuf, detail: String },
    #[error("JWT OIDC config at {path} has bad file mode {mode:o}, expected 0600")]
    Mode { path: PathBuf, mode: u32 },
}

/// Load a [`JwtOidcConfig`] from a JSON file on disk.
///
/// Enforces mode 0o600 on Unix — UNLIKE the JWKS file, the config
/// is operator-sensitive: it determines who becomes admin. The
/// audit-log + token-registry 0o600 contract applies here too.
pub fn load_jwt_oidc_config_from_file(
    path: &Path,
) -> Result<JwtOidcConfig, JwtOidcConfigLoadError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|err| JwtOidcConfigLoadError::Io {
            path: path.to_path_buf(),
            source: err,
        })?;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(JwtOidcConfigLoadError::Mode {
                path: path.to_path_buf(),
                mode,
            });
        }
    }
    let bytes = std::fs::read(path).map_err(|err| JwtOidcConfigLoadError::Io {
        path: path.to_path_buf(),
        source: err,
    })?;
    serde_json::from_slice(&bytes).map_err(|err| JwtOidcConfigLoadError::Parse {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })
}

/// JWT verifier — held inside `Arc<JwtVerifier>` and shared
/// across requests via [`crate::management_api::ApiState`].
/// Constructed by the daemon at boot from a [`JwtOidcConfig`] +
/// a [`RotatableJwks`]; the per-request hot path calls
/// [`Self::verify`] only when the bearer starts with `eyJ`.
pub struct JwtVerifier {
    jwks: RotatableJwks,
    validation_template: Validation,
    role_mapping: RoleMapping,
    expected_issuers: Vec<String>,
    expected_audiences: Vec<String>,
    allowed_algorithms: Vec<Algorithm>,
}

impl std::fmt::Debug for JwtVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtVerifier")
            .field("expected_issuers", &self.expected_issuers)
            .field("expected_audiences", &self.expected_audiences)
            .field("allowed_algorithms", &self.allowed_algorithms)
            .finish()
    }
}

impl JwtVerifier {
    /// Construct a verifier from a config + a JWKS handle. The
    /// `Validation` template is built ONCE here and reused per
    /// request (cheap-cloned inside `decode`); operators tuning
    /// `leeway_seconds` or issuer / audience sets must restart
    /// the daemon (SIGHUP reloads JWKS only).
    ///
    /// Filters out HMAC algorithms defensively — even if an
    /// operator copy-pastes a bad config, we never accept them
    /// (HMAC keys in a public JWKS would be a credential leak).
    #[must_use]
    pub fn new(cfg: JwtOidcConfig, jwks: RotatableJwks) -> Self {
        let allowed_algorithms: Vec<Algorithm> = cfg
            .allowed_algorithms
            .into_iter()
            .filter(|a| !matches!(a, Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512))
            .collect();
        let allowed_algorithms = if allowed_algorithms.is_empty() {
            default_algorithms()
        } else {
            allowed_algorithms
        };
        let seed = allowed_algorithms
            .first()
            .copied()
            .unwrap_or(Algorithm::RS256);
        let mut v = Validation::new(seed);
        v.algorithms.clone_from(&allowed_algorithms);
        v.set_issuer(&cfg.expected_issuers);
        v.set_audience(&cfg.expected_audiences);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        v.validate_exp = true;
        v.validate_nbf = true;
        v.validate_aud = true;
        v.leeway = cfg.leeway_seconds;
        Self {
            jwks,
            validation_template: v,
            role_mapping: cfg.role_mapping,
            expected_issuers: cfg.expected_issuers,
            expected_audiences: cfg.expected_audiences,
            allowed_algorithms,
        }
    }

    /// Access the inner [`RotatableJwks`] handle. Used by the
    /// daemon-side SIGHUP handler to call
    /// `jwks().rotate(Arc::new(new_store))` from the file-reload
    /// path.
    #[must_use]
    pub fn jwks(&self) -> &RotatableJwks {
        &self.jwks
    }

    /// Access the configured [`RoleMapping`]. Used by
    /// [`crate::management_api::auth_layer_rbac`] to call
    /// [`derive_role`] without threading the mapping through
    /// every call site separately.
    #[must_use]
    pub fn role_mapping(&self) -> &RoleMapping {
        &self.role_mapping
    }

    /// Verify a bearer credential.
    ///
    /// Fast bail when the bearer doesn't start with `eyJ` —
    /// callers SHOULD short-circuit at the dispatch site (see
    /// [`crate::management_api::auth_layer_rbac`]) so static
    /// tokens never reach this path at all; the in-method check
    /// is belt-and-braces.
    pub fn verify(&self, bearer: &str) -> Result<VerifiedJwt, JwtVerifyError> {
        if !bearer.starts_with("eyJ") {
            return Err(JwtVerifyError::MalformedJwt);
        }
        let header = decode_header(bearer).map_err(map_jwt_err)?;
        let kid = header.kid.ok_or(JwtVerifyError::MissingKid)?;
        // Enforce the algorithm whitelist on the JWT header BEFORE
        // looking up the key — otherwise a token with an out-of-list
        // `alg` would be tested against the kid's DecodingKey + leak
        // a different error shape.
        if !self.allowed_algorithms.contains(&header.alg) {
            return Err(JwtVerifyError::AlgorithmNotAllowed);
        }
        let store = self.jwks.current();
        let key = store
            .find_key_by_kid(&kid)
            .ok_or(JwtVerifyError::UnknownKid)?;
        let data = decode::<BTreeMap<String, serde_json::Value>>(
            bearer,
            &key.decoding,
            &self.validation_template,
        )
        .map_err(map_jwt_err)?;
        let claims = data.claims;
        // The `set_required_spec_claims` config means jsonwebtoken
        // has already enforced `exp`, `iss`, `aud`, `sub`. We re-
        // read `sub` here to surface the non-empty invariant
        // separately (jsonwebtoken treats "" as present).
        let sub = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or(JwtVerifyError::MissingClaim { name: "sub" })?
            .to_string();
        let iss = claims
            .get("iss")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let exp_unix_ms = claims
            .get("exp")
            .and_then(serde_json::Value::as_i64)
            .map(|s| s.saturating_mul(1000))
            .unwrap_or(0);
        Ok(VerifiedJwt {
            sub,
            iss,
            kid,
            exp_unix_ms,
            claims,
        })
    }
}

/// Per-route ACL check helper that operates on a [`Role`] without
/// requiring a synthesized `TokenEntry`. Used by the JWT bridge
/// so it can reuse the existing
/// [`crate::rbac::TokenRegistry::check_route`] vocabulary without
/// faking a `BearerToken`.
///
/// Mirrors [`crate::rbac::TokenRegistry::check_route`] dispatch:
/// [`Role::Admin`] grants ALL routes (including "unknown" for
/// defense-in-depth); [`Role::ReadOnly`] grants exactly
/// [`crate::rbac::READ_ONLY_ROUTES`]; [`Role::Custom`] grants
/// the explicit per-role set.
#[must_use]
pub fn check_route_for_role(role: &Role, route: &'static str) -> bool {
    match role {
        Role::Admin => true,
        Role::ReadOnly => crate::rbac::READ_ONLY_ROUTES.contains(&route),
        Role::Custom(set) => set.contains(route),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mk_verified(claim_name: &str, claim_value: serde_json::Value) -> VerifiedJwt {
        let mut claims = BTreeMap::new();
        claims.insert("sub".to_string(), json!("alice@example.com"));
        claims.insert("iss".to_string(), json!("https://issuer.example.com"));
        claims.insert(claim_name.to_string(), claim_value);
        VerifiedJwt {
            sub: "alice@example.com".into(),
            iss: "https://issuer.example.com".into(),
            kid: "kid-1".into(),
            exp_unix_ms: 4_102_444_800_000,
            claims,
        }
    }

    fn mk_mapping() -> RoleMapping {
        let mut custom = HashMap::new();
        let mut deployer = BTreeSet::new();
        deployer.insert("/v1/nodes/:id/call".to_string());
        custom.insert("grx-deployer".to_string(), deployer);
        RoleMapping {
            role_claim: "role".into(),
            admin_values: vec!["grx-admin".into()],
            read_only_values: vec!["grx-reader".into()],
            custom_role_mappings: custom,
        }
    }

    fn scratch_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "grx-jwt-oidc-test-{}-{}-{}",
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos(),
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("file")
    }

    #[test]
    fn derive_role_admin_string_match() {
        let vj = mk_verified("role", json!("grx-admin"));
        let role = derive_role(&vj, &mk_mapping()).expect("ok");
        assert_eq!(role, Role::Admin);
    }

    #[test]
    fn derive_role_read_only_string_match() {
        let vj = mk_verified("role", json!("grx-reader"));
        let role = derive_role(&vj, &mk_mapping()).expect("ok");
        assert_eq!(role, Role::ReadOnly);
    }

    #[test]
    fn derive_role_custom_string_match() {
        let vj = mk_verified("role", json!("grx-deployer"));
        let role = derive_role(&vj, &mk_mapping()).expect("ok");
        match role {
            Role::Custom(set) => {
                assert!(set.contains("/v1/nodes/:id/call"));
            }
            other => panic!("expected custom, got {other:?}"),
        }
    }

    #[test]
    fn derive_role_array_admin_precedence() {
        let vj = mk_verified("role", json!(["grx-reader", "grx-admin"]));
        let role = derive_role(&vj, &mk_mapping()).expect("ok");
        assert_eq!(role, Role::Admin);
    }

    #[test]
    fn derive_role_array_read_only_when_no_admin() {
        let vj = mk_verified("role", json!(["grx-reader", "other"]));
        let role = derive_role(&vj, &mk_mapping()).expect("ok");
        assert_eq!(role, Role::ReadOnly);
    }

    #[test]
    fn derive_role_no_match_denies() {
        let vj = mk_verified("role", json!("not-mapped"));
        let err = derive_role(&vj, &mk_mapping()).unwrap_err();
        assert!(matches!(err, ClaimMappingError::NoRoleClaimMatch));
    }

    #[test]
    fn derive_role_missing_claim_denies() {
        let mut vj = mk_verified("other-claim", json!("anything"));
        vj.claims.remove("role");
        let err = derive_role(&vj, &mk_mapping()).unwrap_err();
        assert!(matches!(err, ClaimMappingError::NoRoleClaimMatch));
    }

    #[test]
    fn check_route_for_role_admin_grants_unknown() {
        assert!(check_route_for_role(&Role::Admin, "unknown"));
        assert!(check_route_for_role(&Role::Admin, "/v1/identity"));
        assert!(check_route_for_role(&Role::Admin, "/v1/nodes/:id/messages"));
    }

    #[test]
    fn check_route_for_role_read_only_grants_subset() {
        assert!(check_route_for_role(&Role::ReadOnly, "/v1/identity"));
        assert!(check_route_for_role(&Role::ReadOnly, "/v1/metrics"));
        assert!(!check_route_for_role(
            &Role::ReadOnly,
            "/v1/nodes/:id/messages"
        ));
        assert!(!check_route_for_role(&Role::ReadOnly, "unknown"));
    }

    #[test]
    fn check_route_for_role_custom_only_explicit() {
        let mut set = BTreeSet::new();
        set.insert("/v1/metrics".to_string());
        let role = Role::Custom(set);
        assert!(check_route_for_role(&role, "/v1/metrics"));
        assert!(!check_route_for_role(&role, "/v1/identity"));
        assert!(!check_route_for_role(&role, "unknown"));
    }

    #[test]
    fn reason_str_vocabulary_is_stable() {
        // Pin the closed-vocabulary tags emitted on
        // jwt_auth_fail.reason — these are part of the audit
        // contract and changes break SOC pipelines.
        assert_eq!(JwtVerifyError::MalformedJwt.reason_str(), "malformed_jwt");
        assert_eq!(JwtVerifyError::MissingKid.reason_str(), "missing_kid");
        assert_eq!(JwtVerifyError::UnknownKid.reason_str(), "unknown_kid");
        assert_eq!(
            JwtVerifyError::AlgorithmNotAllowed.reason_str(),
            "algorithm_not_allowed"
        );
        assert_eq!(
            JwtVerifyError::InvalidSignature.reason_str(),
            "invalid_signature"
        );
        assert_eq!(JwtVerifyError::ExpiredSignature.reason_str(), "expired");
        assert_eq!(JwtVerifyError::NotYetValid.reason_str(), "not_yet_valid");
        assert_eq!(JwtVerifyError::InvalidIssuer.reason_str(), "invalid_issuer");
        assert_eq!(
            JwtVerifyError::InvalidAudience.reason_str(),
            "invalid_audience"
        );
        assert_eq!(
            JwtVerifyError::MissingClaim { name: "sub" }.reason_str(),
            "missing_claim"
        );
    }

    #[test]
    fn jwks_load_error_kind_str_stable() {
        let io = JwksLoadError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "x"),
        };
        assert_eq!(io.kind_str(), "io");
        let parse = JwksLoadError::Parse {
            path: PathBuf::from("/tmp/x"),
            detail: "bad".into(),
        };
        assert_eq!(parse.kind_str(), "parse");
        let empty = JwksLoadError::Empty {
            path: PathBuf::from("/tmp/x"),
        };
        assert_eq!(empty.kind_str(), "empty");
        let dup = JwksLoadError::DuplicateKid {
            path: PathBuf::from("/tmp/x"),
            kid: "abc".into(),
        };
        assert_eq!(dup.kind_str(), "duplicate_kid");
    }

    #[test]
    fn rotatable_jwks_current_then_rotate() {
        let store_a = Arc::new(JwksStore::empty());
        let store_b = Arc::new(JwksStore::empty());
        let rot = RotatableJwks::new(store_a.clone());
        let snap = rot.current();
        assert!(Arc::ptr_eq(&snap, &store_a));
        rot.rotate(store_b.clone());
        let snap2 = rot.current();
        assert!(Arc::ptr_eq(&snap2, &store_b));
        // Pre-rotation snapshot still points at A — no UAF.
        assert!(Arc::ptr_eq(&snap, &store_a));
    }

    #[test]
    fn rotatable_jwks_clone_observes_rotation() {
        let store_a = Arc::new(JwksStore::empty());
        let store_b = Arc::new(JwksStore::empty());
        let rot1 = RotatableJwks::new(store_a);
        let rot2 = rot1.clone();
        rot1.rotate(store_b.clone());
        let snap2 = rot2.current();
        assert!(Arc::ptr_eq(&snap2, &store_b));
    }

    #[test]
    fn load_jwks_from_file_missing_returns_io_error() {
        let err = load_jwks_from_file(Path::new("/nonexistent-grx-jwks.json")).unwrap_err();
        assert_eq!(err.kind_str(), "io");
    }

    #[test]
    fn load_jwks_from_file_malformed_returns_parse_error() {
        let path = scratch_path("parse");
        std::fs::write(&path, b"{not json").expect("write");
        let err = load_jwks_from_file(&path).unwrap_err();
        assert_eq!(err.kind_str(), "parse");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_jwks_from_file_empty_keys_returns_empty_error() {
        let path = scratch_path("empty");
        std::fs::write(&path, br#"{"keys": []}"#).expect("write");
        let err = load_jwks_from_file(&path).unwrap_err();
        assert_eq!(err.kind_str(), "empty");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn jwt_verifier_filters_hmac_algorithms() {
        let cfg = JwtOidcConfig {
            expected_issuers: vec!["iss".into()],
            expected_audiences: vec!["aud".into()],
            allowed_algorithms: vec![Algorithm::HS256, Algorithm::RS256],
            leeway_seconds: 30,
            role_mapping: mk_mapping(),
        };
        let jwks = RotatableJwks::new(Arc::new(JwksStore::empty()));
        let v = JwtVerifier::new(cfg, jwks);
        assert!(v.allowed_algorithms.contains(&Algorithm::RS256));
        assert!(!v.allowed_algorithms.contains(&Algorithm::HS256));
    }

    #[test]
    fn jwt_verifier_falls_back_to_default_algorithms_when_only_hmac() {
        let cfg = JwtOidcConfig {
            expected_issuers: vec!["iss".into()],
            expected_audiences: vec!["aud".into()],
            allowed_algorithms: vec![Algorithm::HS256, Algorithm::HS384],
            leeway_seconds: 30,
            role_mapping: mk_mapping(),
        };
        let jwks = RotatableJwks::new(Arc::new(JwksStore::empty()));
        let v = JwtVerifier::new(cfg, jwks);
        assert!(v.allowed_algorithms.contains(&Algorithm::RS256));
        assert!(v.allowed_algorithms.contains(&Algorithm::ES256));
    }

    #[test]
    fn jwt_verifier_short_circuits_non_eyj_bearer() {
        let cfg = JwtOidcConfig {
            expected_issuers: vec!["iss".into()],
            expected_audiences: vec!["aud".into()],
            allowed_algorithms: vec![Algorithm::RS256],
            leeway_seconds: 30,
            role_mapping: mk_mapping(),
        };
        let jwks = RotatableJwks::new(Arc::new(JwksStore::empty()));
        let v = JwtVerifier::new(cfg, jwks);
        let err = v.verify("opaque-static-token-bytes").unwrap_err();
        assert!(matches!(err, JwtVerifyError::MalformedJwt));
    }
}
