//! **M-Prod sub-track #6 Phase B Part 1 (Group B-4a).** Custom
//! [`tower_governor::key_extractor::KeyExtractor`] impls keyed by the
//! [`crate::rbac::TokenId`] of the authenticated principal +
//! trusted-proxy-aware peer-IP extraction. Plus the
//! `TrustedProxyConfig` CIDR parser + matcher backing the
//! `X-Forwarded-For` opt-in.
//!
//! ## How the extractors get their state
//!
//! `tower_governor 0.5`'s [`tower_governor::key_extractor::KeyExtractor::extract`]
//! takes `&Request<T>` — it has no direct access to `ApiState` (which
//! is the only place the [`crate::rbac::RotatableRegistry`] handle
//! lives). The recipe used by the per-IP extractor of Phase A is to
//! pull state out of REQUEST EXTENSIONS (`ConnectInfo<SocketAddr>` was
//! injected per accept by `accept_loop`). We follow the same shape:
//!
//! 1. A `from_fn_with_state` middleware ([`crate::management_api`]'s
//!    `rate_limit_injector`) sits OUTSIDE both governor layers.
//!    Per request, it snapshots
//!    [`crate::rbac::RotatableRegistry::current`] once, runs the
//!    constant-time [`crate::rbac::TokenRegistry::match_token`], and
//!    inserts a [`PerRequestRateLimitCtx`] extension carrying the
//!    matched (or NoMatch) [`TokenIdKey`] + a bucket discriminator
//!    `&'static str` ∈ `{"per_token", "per_token_no_match"}`.
//! 2. The same injector resolves the effective client IP. If
//!    [`TrustedProxyConfig`] is wired AND the peer IP is in the
//!    trusted set AND `X-Forwarded-For` carries a parseable leftmost
//!    IP, it inserts an [`EffectivePeerIp`] extension. Otherwise no
//!    extension — the per-IP extractor falls back to
//!    `ConnectInfo<SocketAddr>` (= Phase A behavior).
//! 3. Both extractors ([`TokenIdKeyExtractor`] +
//!    [`TrustedProxyAwarePeerIpExtractor`]) read the extensions
//!    SYNCHRONOUSLY from `extract(&req)` — no async, no `&mut`,
//!    matches the trait's purity contract.
//!
//! This sidesteps the design question "how does the extractor access
//! `ApiState`?" without forking `tower_governor`. The injector layer
//! holds the only reference to `ApiState`; the extractors are pure
//! and cheap.
//!
//! ## Why a hand-rolled CIDR parser
//!
//! Phase B Part 1 deliberately avoids adding a new `ipnet` / `cidr`
//! dependency. The trusted-proxies wire format is single-line
//! comma-separated `addr` or `addr/prefix` tokens — IPv4 + IPv6 with
//! prefix in `0..=32` / `0..=128`. ~80 LOC of parser + matcher keeps
//! `cargo deny check` zero-change and the trust footprint local. A
//! future PR can swap to a crate if the surface expands (e.g.
//! Forwarded-header parsing, RFC 7239).

use std::fmt::Debug;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::http::Request;
use tower_governor::errors::GovernorError;
use tower_governor::key_extractor::KeyExtractor;

use crate::rbac::TokenId;

/// **M-Prod sub-track #6 Phase B Part 1.** Bucket-key for the
/// per-token rate-limit governor layer.
///
/// `Matched(id)` hashes by the operator-assigned [`TokenId`] string,
/// so each principal gets an INDEPENDENT token bucket. All
/// no-match attempts collapse to a SINGLE `NoMatch` bucket so the
/// per-request bucket-key vocabulary stays bounded (otherwise an
/// attacker hitting the endpoint with random per-request tokens
/// would inflate the keyed-limiter's internal `DashMap` to
/// `O(distinct_garbage_tokens)`).
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum TokenIdKey {
    /// Bearer token matched a registry entry.
    Matched(TokenId),
    /// No registry entry matched the presented credential — covers
    /// `Authorization` missing, malformed, or carrying garbage bytes.
    /// All such requests share one bucket. Also used when RBAC is
    /// not wired (legacy single-token spawn path) so the per-token
    /// layer is effectively dormant per design.
    NoMatch,
}

/// **M-Prod sub-track #6 Phase B Part 1.** Parsed CIDR ∪ host-route
/// trusted-proxy set. Built once at router-build time, then injected
/// into per-request extensions via `Arc::clone`.
///
/// Wire format: single-line comma-separated tokens, each either
/// `addr` (host route, prefix = 32 for v4 / 128 for v6) or
/// `addr/prefix`. Whitespace around tokens is tolerated. Order is
/// insignificant — [`Self::contains`] scans linearly (operators set
/// ≤ a handful of CIDRs in practice).
#[derive(Clone, Debug)]
pub struct TrustedProxyConfig {
    nets: Vec<TrustedNet>,
}

/// One entry in a [`TrustedProxyConfig`]: an `IpAddr` + prefix length.
/// Host routes encode as `(addr, 32)` for v4 / `(addr, 128)` for v6.
#[derive(Clone, Debug)]
pub struct TrustedNet {
    pub addr: IpAddr,
    pub prefix: u8,
}

/// Parser errors surfaced by [`TrustedProxyConfig::parse`].
#[derive(Debug, thiserror::Error)]
pub enum TrustedProxyParseError {
    /// Token had `/` but the prefix didn't parse as an unsigned int.
    #[error("invalid CIDR token (bad prefix): {0}")]
    BadCidr(String),
    /// The `addr` portion didn't parse as an `IpAddr`.
    #[error("invalid IP: {0}")]
    BadIp(String),
    /// The prefix was numeric but out of range (>32 for v4 / >128 for v6).
    #[error("prefix out of range: {0}")]
    BadPrefix(String),
}

impl TrustedProxyConfig {
    /// Parse a comma-separated `addr[/prefix]` list into a
    /// [`TrustedProxyConfig`]. Empty / whitespace-only input
    /// produces an empty list (which never matches anything).
    ///
    /// Errors fail-closed: a parse failure returns
    /// [`TrustedProxyParseError`] so the caller can log + treat as
    /// "no trusted proxies configured" rather than silently honoring
    /// a malformed list (which would amount to honoring
    /// `X-Forwarded-For` from nobody, the safe default).
    pub fn parse(s: &str) -> Result<Self, TrustedProxyParseError> {
        let mut nets = Vec::new();
        for raw in s.split(',') {
            let token = raw.trim();
            if token.is_empty() {
                continue;
            }
            let (addr_part, prefix_part) = match token.split_once('/') {
                Some((a, p)) => (a, Some(p)),
                None => (token, None),
            };
            let addr: IpAddr = addr_part
                .parse()
                .map_err(|_| TrustedProxyParseError::BadIp(addr_part.to_string()))?;
            let default_prefix: u8 = match addr {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            let prefix: u8 = if let Some(p) = prefix_part {
                p.parse::<u8>()
                    .map_err(|_| TrustedProxyParseError::BadCidr(token.to_string()))?
            } else {
                default_prefix
            };
            let max_prefix = default_prefix;
            if prefix > max_prefix {
                return Err(TrustedProxyParseError::BadPrefix(token.to_string()));
            }
            nets.push(TrustedNet { addr, prefix });
        }
        Ok(Self { nets })
    }

    /// `true` iff `ip` is covered by ANY entry in the trusted set.
    /// Linear scan — set sizes are tiny in practice (<= ~16).
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|net| cidr_match(net, ip))
    }

    /// Number of parsed entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nets.len()
    }

    /// `true` iff [`Self::len`] == 0.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }
}

/// Bit-prefix comparison: `ip` is in `net`'s subnet iff the top
/// `net.prefix` bits of `ip` equal those of `net.addr`. Mixed-family
/// pairs (v4 + v6) never match.
fn cidr_match(net: &TrustedNet, ip: IpAddr) -> bool {
    match (net.addr, ip) {
        (IpAddr::V4(a), IpAddr::V4(b)) => bit_prefix_eq(&a.octets(), &b.octets(), net.prefix),
        (IpAddr::V6(a), IpAddr::V6(b)) => bit_prefix_eq(&a.octets(), &b.octets(), net.prefix),
        _ => false,
    }
}

/// Compare the top `prefix` bits of two equal-length byte arrays.
fn bit_prefix_eq(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let prefix = prefix as usize;
    let full_bytes = prefix / 8;
    if a[..full_bytes] != b[..full_bytes] {
        return false;
    }
    let extra_bits = prefix % 8;
    if extra_bits == 0 {
        return true;
    }
    let mask: u8 = 0xFFu8 << (8 - extra_bits);
    (a[full_bytes] & mask) == (b[full_bytes] & mask)
}

/// **M-Prod sub-track #6 Phase B Part 1.** Per-request rate-limit
/// context injected by the `rate_limit_injector` middleware. Carries
/// the precomputed [`TokenIdKey`] (so the extractor doesn't have to
/// re-walk the registry) + a `bucket_hint` discriminator the per-token
/// error_handler uses to stamp the 429's bucket field.
#[derive(Clone, Debug)]
pub struct PerRequestRateLimitCtx {
    pub token_key: TokenIdKey,
    pub bucket_hint: &'static str,
}

/// **M-Prod sub-track #6 Phase B Part 1.** Effective client IP after
/// `X-Forwarded-For` resolution. Injected by `rate_limit_injector`
/// ONLY when [`TrustedProxyConfig`] is wired AND the peer is in the
/// trusted set AND a parseable leftmost IP was extracted from the
/// `X-Forwarded-For` header. Absence (the common case) tells the
/// per-IP extractor to fall back to `ConnectInfo<SocketAddr>` —
/// bit-for-bit Phase A behavior.
#[derive(Clone, Debug, Copy)]
pub struct EffectivePeerIp(pub IpAddr);

/// **M-Prod sub-track #6 Phase B Part 1.** Response extension stamped
/// by each per-layer governor error_handler so the outer
/// `rate_limit_observer` middleware can disambiguate which bucket
/// fired the 429. The per-layer closures know one of `"per_ip"` or
/// `"per_token"`; the `per_token` discriminator is REFINED to
/// `"per_token_no_match"` by the observer using
/// [`PerTokenBucketHintMarker`] (stamped by the `rate_limit_injector`
/// on the way back through the response path).
#[derive(Clone, Debug, Copy)]
pub struct RateLimitDeniedBucket(pub &'static str);

/// **M-Prod sub-track #6 Phase B Part 1.** Response extension
/// carrying the per-token bucket sub-discriminator, written by the
/// `rate_limit_injector` middleware on the way back through the
/// response path. Closed vocabulary `{"per_token", "per_token_no_match"}`.
///
/// The injector is the only layer that knows whether the request's
/// bearer token MATCHED an RBAC entry. The per-token governor's
/// error_handler closure receives only `GovernorError` — it can't
/// inspect the request. So we use this two-step stamp: the
/// error_handler tags `RateLimitDeniedBucket("per_token")`; the
/// injector tags `PerTokenBucketHintMarker(<matched-or-no-match>)`;
/// the outer observer combines them.
#[derive(Clone, Debug, Copy)]
pub struct PerTokenBucketHintMarker(pub &'static str);

/// **M-Prod sub-track #6 Phase B Part 1.** `KeyExtractor` for the
/// per-token governor layer. Reads the precomputed [`TokenIdKey`]
/// from the [`PerRequestRateLimitCtx`] request extension installed
/// by `rate_limit_injector`.
///
/// Defensive: if the extension is missing (which is impossible under
/// our wiring — injector layer is mandatory whenever the per-token
/// governor is mounted), returns `TokenIdKey::NoMatch` and emits a
/// loud audit-target warning. We never return
/// `GovernorError::UnableToExtractKey` because that would 500 the
/// request; a sentinel `NoMatch` keeps the request rate-limited in
/// the shared no-match bucket (safer fallback).
#[derive(Clone, Debug)]
pub struct TokenIdKeyExtractor;

impl KeyExtractor for TokenIdKeyExtractor {
    type Key = TokenIdKey;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        if let Some(ctx) = req.extensions().get::<PerRequestRateLimitCtx>() {
            return Ok(ctx.token_key.clone());
        }
        // Defensive — should never trigger under correct wiring.
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "rate_limit_token_ctx_missing",
            "per-token KeyExtractor saw a request with no PerRequestRateLimitCtx extension; \
             routing the request through the shared no-match bucket"
        );
        Ok(TokenIdKey::NoMatch)
    }
}

/// **M-Prod sub-track #6 Phase B Part 1.** `KeyExtractor` for the
/// per-IP governor layer. Drop-in replacement for the Phase A
/// [`tower_governor::key_extractor::PeerIpKeyExtractor`]:
///
/// - When an [`EffectivePeerIp`] extension is present (injector
///   resolved a trusted-proxy `X-Forwarded-For` leftmost), use that
///   IP as the bucket key.
/// - Otherwise fall back to `ConnectInfo<SocketAddr>` — bit-for-bit
///   Phase A semantics.
///
/// This means the per-IP layer's behavior is UNCHANGED unless the
/// operator opts in by wiring [`TrustedProxyConfig`].
#[derive(Clone, Debug)]
pub struct TrustedProxyAwarePeerIpExtractor;

impl KeyExtractor for TrustedProxyAwarePeerIpExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        if let Some(eff) = req.extensions().get::<EffectivePeerIp>() {
            return Ok(eff.0);
        }
        req.extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip())
            .ok_or(GovernorError::UnableToExtractKey)
    }
}

/// **M-Prod sub-track #6 Phase B Part 1.** Wrap an
/// `Arc<TrustedProxyConfig>` so the injector + `build_router`-style
/// builders can hold the same parsed config without re-parsing per
/// request.
pub type SharedTrustedProxyConfig = Arc<TrustedProxyConfig>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::net::Ipv6Addr;

    #[test]
    fn parse_cidr_v4_happy_path() {
        let cfg = TrustedProxyConfig::parse("10.0.0.0/8,127.0.0.1").expect("parse");
        assert_eq!(cfg.len(), 2);
        assert!(cfg.contains(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(cfg.contains(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!cfg.contains(IpAddr::V4(Ipv4Addr::new(9, 1, 2, 3))));
        assert!(!cfg.contains(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))));
    }

    #[test]
    fn parse_cidr_v6_happy_path() {
        let cfg = TrustedProxyConfig::parse("::1/128,fc00::/7").expect("parse");
        assert_eq!(cfg.len(), 2);
        assert!(cfg.contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(cfg.contains(IpAddr::V6(Ipv6Addr::new(
            0xfc00, 0, 0, 0, 0, 0, 0, 1,
        ))));
        // Outside fc00::/7 → not matched.
        assert!(!cfg.contains(IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1,
        ))));
    }

    #[test]
    fn parse_bare_ip_as_host_route() {
        let cfg = TrustedProxyConfig::parse("8.8.8.8").expect("parse");
        assert!(cfg.contains(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!cfg.contains(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 9))));
    }

    #[test]
    fn parse_rejects_prefix_too_large_v4() {
        let err = TrustedProxyConfig::parse("10.0.0.0/33").unwrap_err();
        assert!(matches!(err, TrustedProxyParseError::BadPrefix(_)));
    }

    #[test]
    fn parse_rejects_prefix_too_large_v6() {
        let err = TrustedProxyConfig::parse("::/129").unwrap_err();
        assert!(matches!(err, TrustedProxyParseError::BadPrefix(_)));
    }

    #[test]
    fn parse_rejects_malformed_ip() {
        let err = TrustedProxyConfig::parse("not-an-ip").unwrap_err();
        assert!(matches!(err, TrustedProxyParseError::BadIp(_)));
    }

    #[test]
    fn parse_rejects_non_numeric_prefix() {
        let err = TrustedProxyConfig::parse("10.0.0.0/abc").unwrap_err();
        assert!(matches!(err, TrustedProxyParseError::BadCidr(_)));
    }

    #[test]
    fn parse_tolerates_whitespace_and_empty() {
        let cfg = TrustedProxyConfig::parse(" 10.0.0.0/8 , , 127.0.0.1 ").expect("parse");
        assert_eq!(cfg.len(), 2);
    }

    #[test]
    fn parse_empty_input_is_empty_config() {
        let cfg = TrustedProxyConfig::parse("").expect("parse");
        assert!(cfg.is_empty());
        assert!(!cfg.contains(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn contains_matches_subnet_boundary() {
        // 10.0.0.0/16 covers 10.0.x.x, NOT 10.1.x.x.
        let cfg = TrustedProxyConfig::parse("10.0.0.0/16").unwrap();
        assert!(cfg.contains(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0))));
        assert!(cfg.contains(IpAddr::V4(Ipv4Addr::new(10, 0, 255, 255))));
        assert!(!cfg.contains(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 0))));
    }

    #[test]
    fn contains_rejects_mixed_families() {
        let cfg = TrustedProxyConfig::parse("10.0.0.0/8").unwrap();
        assert!(!cfg.contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        let cfg6 = TrustedProxyConfig::parse("::/0").unwrap();
        assert!(!cfg6.contains(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0))));
    }

    #[test]
    fn token_id_key_matched_eq_and_hash() {
        use std::collections::HashSet;
        let a1 = TokenIdKey::Matched(TokenId("alice".into()));
        let a2 = TokenIdKey::Matched(TokenId("alice".into()));
        let b = TokenIdKey::Matched(TokenId("bob".into()));
        let nm = TokenIdKey::NoMatch;
        let mut s = HashSet::new();
        s.insert(a1.clone());
        s.insert(a2);
        s.insert(b);
        s.insert(nm.clone());
        s.insert(TokenIdKey::NoMatch);
        // a1 == a2 (deduped), b distinct, all NoMatch dedup.
        assert_eq!(s.len(), 3);
        assert!(s.contains(&a1));
        assert!(s.contains(&nm));
    }
}
