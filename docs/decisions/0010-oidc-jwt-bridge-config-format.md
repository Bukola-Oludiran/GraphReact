# 0010 — OIDC JWT bridge: file-based JWKS + JSON config + per-claim role mapping

**Status:** DECIDED — recorded 2026-06-26. M-Prod sub-track #1 Phase B Part 2 (Group 1c) SHIPPED 2026-06-26.

**Related:** [ADR 0006](0006-audit-chain-substrate.md) (NDJSON precedent + defensive parse), [ADR 0008](0008-multi-token-rbac.md) (Role / TokenRegistry vocabulary the bridge maps onto), [ADR 0009](0009-token-registry-file-format.md) (mode-0o600 file shape this ADR DELIBERATELY DEVIATES from for the JWKS file). Cross-references: [m-prod-auth.md §"Shipped — Phase B Part 2 (Group 1c)"](../backend/m-prod-auth.md), [m-prod.md](../backend/m-prod.md) (cross-track contracts table).

## Why an ADR (and why now)

Group 1c ships the JWT-OIDC bridge — JWT verification + claim-to-Role mapping for external IdPs (Okta / Auth0 / Keycloak / AWS Cognito / Google / GitHub etc.). The bridge consumes TWO operator-supplied files: a JWKS holding the IdP signing keys, and a bridge config holding the issuer / audience allowlists + the role-mapping vocabulary. Inventing the file shapes inside the implementation PR without an ADR would leave operators guessing about forward-compat guarantees and would risk silent breaking changes the first time an alternative envelope or mapping language was considered. This ADR pins the choices + documents the reasoning.

## The decisions (seven)

1. **JWKS file is RFC 7517 §5 JSON** (`{"keys":[<Jwk>, ...]}`). Operators paste the contents of their IdP's `/.well-known/jwks.json` endpoint. Each JWK must carry `kid` (Group 1c rejects keys without a `kid` — see decision #4). The parser uses `jsonwebtoken::jwk::JwkSet` for deserialization + `DecodingKey::from_jwk` for per-key decode; raw RSA `n`/`e` or EC `crv`/`x`/`y` extraction is delegated to the library.
2. **JWKS file mode is NOT enforced.** Mode 0o600 is the contract for the audit-log + token-registry files (per [ADR 0009](0009-token-registry-file-format.md) decision #3), but the JWKS file holds PUBLIC keys (by RFC 7517 convention) and operators may legitimately want world-readable permissions for sidecar-style refresh patterns. The loader does NOT check the mode bits. This is a deliberate deviation from the audit-log + token-registry contract.
3. **JWT-OIDC bridge config is a JSON file (NOT NDJSON).** The config is monolithic, not append-only — there's no per-line stream semantics to preserve. JSON keeps the role-mapping nested HashMap shape readable; NDJSON would force flattening. The config file IS mode-0o600 enforced — the role mapping determines who becomes admin, so the file gets the same filesystem protection as the static-token registry.
4. **Tokens missing `kid` are rejected even when the JWKS has one key.** Uniformity across rotation: production IdPs publish OLD + NEW keys side-by-side during rotation, so a token without `kid` cannot reliably resolve to a specific key. Rejecting at the bridge keeps the operator's mental model simple (`kid` is always required) and prevents the subtle "single-key JWKS works in dev, fails in prod" failure mode.
5. **Algorithm whitelist defaults to RS256 + ES256; HMAC algorithms are stripped defensively even if the operator lists them.** HMAC keys are symmetric secrets — putting them in a public JWKS file would be a credential leak. The `JwtVerifier::new` filter removes `HS256` / `HS384` / `HS512` regardless of config; if the strip leaves the whitelist empty, fall back to RS256 + ES256 rather than accept nothing (which would surface as `AlgorithmNotAllowed` on every request, a confusing failure mode). The `Algorithm::None` variant is rejected by jsonwebtoken's `Validation::new` automatically — the classic JWT vulnerability cannot enter through operator config.
6. **Default-deny on role mapping.** A JWT that verifies cleanly but whose `role_claim` value matches none of `admin_values` / `read_only_values` / `custom_role_mappings` returns `ClaimMappingError::NoRoleClaimMatch` and the bridge denies the request with a `jwt_auth_fail reason=no_role_claim_match` audit row. No implicit Role fallback. Operators wiring a new IdP must explicitly enumerate the role values they expect.
7. **Graceful coexistence with static tokens via the `eyJ` prefix discriminator.** Bearers that start with `eyJ` (the base64url-encoded `{` opener of every JWT header) route through `JwtVerifier::verify` FIRST; non-eyJ bearers short-circuit the JWT path and dispatch through the existing `TokenRegistry::match_token` walk unchanged. Operators can mix JWTs and static tokens during a rollout. EXCEPTION: a `MissingKid` or `UnknownKid` JWT failure falls through silently to the static-token walk — the bearer might be (a) a static token that coincidentally starts with `eyJ`, or (b) a JWT signed by a key the operator hasn't sidecar-refreshed yet (mid-rotation).

## On-disk JWKS schema

```json
{
  "keys": [
    {
      "kty": "RSA",
      "kid": "okta-key-2026-q2",
      "use": "sig",
      "alg": "RS256",
      "n": "<base64urlUInt-modulus>",
      "e": "AQAB"
    },
    {
      "kty": "EC",
      "kid": "okta-key-2026-q3",
      "use": "sig",
      "alg": "ES256",
      "crv": "P-256",
      "x": "<base64url-x>",
      "y": "<base64url-y>"
    }
  ]
}
```

Per-key skip discipline (mirrors RFC 7517 §4.5 advisory):

- Keys without `kid` are SKIPPED + warned (`jwks_skipped_key reason=missing_kid`).
- Keys whose `DecodingKey::from_jwk` returns Err (unsupported curve like ES512, malformed parameters, etc.) are SKIPPED + warned (`jwks_skipped_key reason=decode`).
- Empty store after the skip pass returns `JwksLoadError::Empty` (the SIGHUP reload retains the prior JwksStore).
- Two keys carry the same `kid` returns `JwksLoadError::DuplicateKid` (rejected at load — lookup is by `kid` alone, so duplicates would create silent fallback).

## On-disk bridge config schema

```json
{
  "expected_issuers": ["https://okta.example.com/oauth2/default"],
  "expected_audiences": ["grx-management-api"],
  "allowed_algorithms": ["RS256", "ES256"],
  "leeway_seconds": 60,
  "role_mapping": {
    "role_claim": "https://grx.example.com/role",
    "admin_values": ["grx-admin"],
    "read_only_values": ["grx-reader"],
    "custom_role_mappings": {
      "grx-deployer": ["/v1/nodes/:id/call", "/v1/nodes/:id/messages"]
    }
  }
}
```

Per-field semantics:

| Field | Type | Required | Notes |
|---|---|---|---|
| `expected_issuers` | `Vec<String>` | yes | At least one. JWT `iss` claim must match one of these exactly (case-sensitive). |
| `expected_audiences` | `Vec<String>` | yes | At least one. JWT `aud` claim must contain one of these (jsonwebtoken's set-membership semantics). |
| `allowed_algorithms` | `Vec<Algorithm>` | no (default `["RS256","ES256"]`) | JWT header `alg` must be in this list. HMAC variants are stripped at verifier construction even if listed. |
| `leeway_seconds` | `u64` | no (default 60) | Clock-skew tolerance applied to `exp` and `nbf`. RFC 7519 §4.1.4 recommends "a few minutes". |
| `role_mapping.role_claim` | `String` | yes | JWT claim name to inspect (e.g. `"role"`, `"groups"`, or a namespaced URI per Auth0 convention). |
| `role_mapping.admin_values` | `Vec<String>` | no (default `[]`) | Claim values that map to `Role::Admin`. |
| `role_mapping.read_only_values` | `Vec<String>` | no (default `[]`) | Claim values that map to `Role::ReadOnly`. |
| `role_mapping.custom_role_mappings` | `HashMap<String, BTreeSet<String>>` | no (default `{}`) | Per-claim-value `Role::Custom` ACL sets (route labels). |

## File-level guarantees

- **Required JWT claims:** `exp`, `iss`, `aud`, `sub` are ALL required (`set_required_spec_claims` is invoked). A token missing any of them fails verify with `MissingClaim { name }`. Default RFC 7519 only requires `exp`; we tighten.
- **Non-empty `sub`:** even when jsonwebtoken's `set_required_spec_claims` accepts an empty-string `sub`, the bridge rejects it (`MissingClaim { name: "sub" }`) — TokenId derivation needs a non-empty principal.
- **`nbf` is validated** (`validate_nbf = true`). jsonwebtoken defaults this to `false`; we flip it. Tokens with a future `nbf` (after leeway) fail with `NotYetValid`.
- **`aud` validation is on** (jsonwebtoken's default; explicit here for clarity).
- **Algorithm enforcement is BOTH header-side and decode-side:** the bridge checks `header.alg` against `allowed_algorithms` BEFORE looking up the kid (so a kid lookup against a token with disallowed alg never resolves a `DecodingKey`), AND jsonwebtoken's `decode` enforces `validation.algorithms` again at signature-verify time (defense in depth against any future header-skip).

## Why JSON over alternatives (bridge config)

Three options were considered:

- **(a) NDJSON one record per line.** REJECTED — the config is monolithic (no per-line stream semantics). NDJSON would force flattening the nested `role_mapping` into a sequence of records, hurting readability.
- **(b) TOML.** REJECTED — adds a second parser to the workspace (no current `toml` dep). The role-mapping HashMap-of-Set shape is awkward in TOML (table arrays); JSON's nested object syntax keeps the operator config concise.
- **(c) JSON.** CHOSEN — operators already paste JWKS from their IdP, so JSON tooling (`jq`) is already in their workflow. Nested object shapes are first-class.

## Why pin jsonwebtoken at the v9 major

The substrate plan pinned `"9"` (NOT v10) intentionally:

- v10 changed the default feature surface; auditing the implications against the existing pem 3 / base64 0.22 / serde lock baseline is out of scope for this slice.
- v9.3.1 ships exactly the surface we need (`decode_header`, `decode`, `Validation`, `JwkSet`, `DecodingKey::from_jwk`, `ErrorKind`).
- v9 is MIT-licensed; transitive deps (ring, pem, simple_asn1, base64) are all already in-tree via rustls / serde / other existing workspace pins — `cargo deny check` baseline failures are pre-existing and not perturbed by this addition.

The upgrade to v10 is reversible — a future ADR (when v10 has been audited against the workspace lock) can bump the pin without changing any consumer code (the public API we use is unchanged across the major).

## Why file-based JWKS (and NOT HTTP auto-fetch)

The plan called out HTTP-based JWKS auto-fetch as DEFERRED. The reasoning:

- Adding an HTTP client (reqwest / hyper-util client features) would meaningfully grow the dep tree and the cargo-deny audit surface.
- Production OIDC IdPs typically rotate keys on 24h+ cadences; sidecar-driven file refresh + SIGHUP is sufficient for the realistic operator cadence.
- The file-based path RIDES the existing Group 1ab SIGHUP fan-out — zero net new orchestration code.
- Operators who want HTTP auto-fetch can layer a small sidecar (`curl /.well-known/jwks.json -o /etc/grx/jwks.json && kill -HUP $(cat /var/run/grx.pid)`) without any daemon-side changes.

The cost: operators are responsible for sidecar freshness. IdP key rotations that happen faster than the sidecar's refresh cadence will surface as `UnknownKid` JWT failures + audit rows. Recommend ≤ 1h sidecar polling for production deploys.

## Operator workflow

```bash
# 1. Mint the JWT-OIDC bridge config (mode 0o600 — role mapping
#    determines who becomes admin).
install -m 0600 /dev/null /etc/grx/jwt-config.json
cat > /etc/grx/jwt-config.json <<'EOF'
{
  "expected_issuers": ["https://okta.example.com/oauth2/default"],
  "expected_audiences": ["grx-management-api"],
  "allowed_algorithms": ["RS256", "ES256"],
  "leeway_seconds": 60,
  "role_mapping": {
    "role_claim": "https://grx.example.com/role",
    "admin_values": ["grx-admin"],
    "read_only_values": ["grx-reader"],
    "custom_role_mappings": {}
  }
}
EOF

# 2. Fetch the JWKS from the IdP. Mode is intentionally relaxed —
#    JWKS keys are public by RFC 7517 convention.
curl -sSL https://okta.example.com/oauth2/default/v1/keys \
  > /etc/grx/jwks.json

# 3. Boot the daemon with --observer-jwt-config + --observer-jwks
#    (and an --observer-tokens file — the JWT bridge dispatches
#    INSIDE auth_layer_rbac, so RBAC must also be wired).
grx cluster daemon \
  --node-id alice@127.0.0.1:9000 \
  --listen 127.0.0.1:9000 \
  --observer-bind 127.0.0.1:9090 \
  --observer-cert /etc/grx/cert.pem \
  --observer-key /etc/grx/key.pem \
  --observer-tokens /etc/grx/tokens.ndjson \
  --observer-jwt-config /etc/grx/jwt-config.json \
  --observer-jwks /etc/grx/jwks.json \
  --observer-audit-log /var/log/grx/audit.ndjson

# 4. When the IdP rotates keys (24h+ cadence typical), the
#    sidecar re-fetches + fires SIGHUP. The daemon's
#    Group 1ab + 1c fan-out reloads JWKS atomically (and
#    rotates the audit log + reloads tokens + reloads TLS in
#    the same signal).
curl -sSL https://okta.example.com/oauth2/default/v1/keys \
  > /etc/grx/jwks.json
grx admin reload-tokens --pid $(pgrep -f 'grx cluster daemon')
# OR: kill -HUP $(pgrep -f 'grx cluster daemon')
```

## Reversibility

| Decision | Reversibility |
|---|---|
| JWKS file is RFC 7517 §5 JSON | Locked. The format IS RFC-defined. Operators paste from their IdP. |
| JWKS file mode is NOT enforced | Reversible — a future ADR could add an opt-in `--observer-jwks-mode 0600` flag for operators who want the protection (the keys ARE public but operators may want filesystem-layer auditing of who reads them). Default would stay unenforced. |
| Bridge config is JSON | Reversible — a future ADR could add a sibling format behind a discriminator. JSON stays the default. |
| `kid` always required on JWTs | Reversible — relaxing to "first key wins when JWKS has exactly one key" would silently work but would create the dev-vs-prod mental-model split. Keep locked. |
| HMAC algorithms stripped | Locked. The classic JWT vulnerability — never give operators the option to mis-configure their way into a credential leak via the public JWKS. |
| Default-deny role mapping | Locked. Substrate-not-MVP: silent fallback to a default Role is the kind of thing security audits flag. |
| `eyJ` prefix discriminator | Reversible — a future Group could add a config flag to disable the static-token fallthrough entirely (JWT-only mode for fleets that have fully migrated). Default would stay coexistence. |
| File-based JWKS (no HTTP auto-fetch) | Reversible — γ.next can layer HTTP-based JWKS auto-fetch on top of the file-based substrate as a sidecar-in-daemon shape. The file path remains the back-compat default. |

## Cross-track contracts (locked here, executed elsewhere)

| Track | Contract |
|---|---|
| **#1 Audit chain (Phase A + Phase B)** | Group 1c emits `jwt_auth_success` (WARN), `jwt_auth_fail` (WARN), `jwks_reloaded` (WARN), `jwks_reload_failed` (WARN), `jwks_reload_skipped` (WARN). All use target `graphreact::observer::audit`. The `reason` field on `jwt_auth_fail` is a closed-vocabulary `&'static str` from `JwtVerifyError::reason_str` / `ClaimMappingError::reason_str` plus `"authz_deny"` for the post-verify per-route ACL denial. |
| **#1 Multi-token RBAC (Phase B Part 1)** | The JWT bridge sits INSIDE `auth_layer_rbac` and reuses the per-route ACL (`READ_ONLY_ROUTES` / `Role::Admin`-grants-all / `Role::Custom` set semantics) without modification. `check_route_for_role(&Role, route)` is a helper that operates on a `Role` directly so the JWT bridge can dispatch without synthesizing an ephemeral `TokenEntry`. |
| **#1 Multi-token RBAC (Phase B Part 2 — Group 1ab)** | The JWT bridge requires an RBAC registry to be wired (`auth_layer_rbac` is the JWT dispatch site). Operators wiring `--observer-jwt-config` + `--observer-jwks` without `--observer-tokens` get a boot-time `jwt_config_without_rbac` WARN audit event — the JWT path is dead config without RBAC. |
| **#1 SIGHUP fan-out (Group 1ab)** | SIGHUP gate extends from `(tls_paths || tokens_path || audit_log_path)` to also include `jwks_path`. The `grx-sighup-ready PID` readiness line stays emit-once per process. The fan-out fires `rotate_audit_log → reload_tokens_from_disk → reload_jwks_from_disk → reload_tls_from_disk` — failure in one does NOT short-circuit the rest. |
| **#1 B-4a per-token rate limit (SHIPPED)** | JWT-derived `TokenId(sub)` flows through the existing `TokenIdKeyExtractor` unchanged. Per-token rate limits apply to JWT-derived TokenIds the same way as static tokens. |
| **#2 TLS rotation (SHIPPED)** | `RotatableJwks` mirrors `RotatableServerConfig` bit-for-bit (Arc<ArcSwap<T>> newtype, `current()` / `rotate(Arc<T>)` named identically). A single SIGHUP handler observes one API shape across TLS + RBAC + JWKS. |
| **#3 Secrets keyring (SHIPPED)** | JWKS file is operator-managed at the filesystem layer (keys are public; no keyring needed). Bridge config file is operator-managed too — future Group could add `--observer-jwt-config-keyring <name>` for operators who want the role mapping in the keyring, keeping the file path as the back-compat default. |
| **#4 SIGHUP lifecycle (SHIPPED for TLS + tokens)** | `DaemonHandle::reload_jwks_from_disk` short-circuits when `DrainFlag::is_draining`, emitting `jwks_reload_skipped reason="draining"` in place of the reloaded/failed pair. |
| **#5 Observability** | `audit_sink_write_failures_total` counter rides the JWKS reload audit events too (no per-event subkind cardinality bump). |
| **#6 Constant-time compare (SHIPPED)** | The JWT bridge's `eyJ` prefix discriminator is a 3-byte string-prefix check — no constant-time discipline applies (the bearer's first 3 bytes are not the credential material). Constant-time signature verification IS the jsonwebtoken / ring contract; the bridge inherits it. |
| **#9 Chaos arm (DEFERRED)** | Future chaos arms include malformed-JWKS stress (random byte-corruption mid-file) + IdP-rotation-faster-than-sidecar stress. Group 1c's defensive per-key skip-and-warn is the substrate. |

## Open questions tracked (not blockers)

- **HTTP-based JWKS auto-fetch.** Deferred to γ.next per the substrate plan. Operators run a sidecar that writes the JWKS JSON to disk + fires SIGHUP. Recommended sidecar cadence: ≤ 1h to catch IdP rotations before the audit log fills with `UnknownKid` failures.
- **JWE (encrypted JWTs).** Group 1c handles JWS-signed JWTs only. Operators who need encrypted access tokens can wrap them at the IdP gateway tier.
- **Per-issuer JWKS files.** Group 1c maps ONE daemon to ONE JWKS file. Multi-issuer deploys union the keys into one file (kids must be globally unique across issuers). A future Group could add `--observer-jwks-dir <DIR>` for per-issuer key files; deferred until operator demand surfaces.
- **Dynamic role mapping.** Today's mapping is exact-string match over the `role_claim` value. A future Group could add a Lua/CEL expression mapper (`role_expr: "claims.sub == 'alice' ? Admin : ReadOnly"`); deferred until claim shapes more complex than enum-strings show up.
- **Live config reload.** SIGHUP reloads the JWKS file only; bridge config changes (issuer/audience allowlists, role mapping, leeway) require a daemon restart. A future Group could add a `reload_jwt_config_from_disk` method riding the same SIGHUP fan-out; deferred (config changes are operator-initiated and a restart is cheap with the existing drain orchestration).
- **`AlgorithmMismatchesJwk` distinct error variant.** Today a JWT presenting `alg=RS256` against a JWK declaring `alg=ES256` surfaces as `InvalidSignature` after decode-time validation. Operators triaging this would benefit from a dedicated variant (`alg_mismatches_jwk`) on the `JwtVerifyError::reason_str` audit vocabulary; deferred (low signal-to-noise — operator-misconfigured JWKS files surface during the first verify regardless).
