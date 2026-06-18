# 0008 — M-Prod multi-token bearer registry + RBAC engine + per-route ACL ride the Phase A audit-chain substrate; token ids are operator-assigned names (NOT hashes); registry rotates via the Group B-2 #2 atomic-swap pattern; constant-time compare walks ALL entries on every match

**Status:** DECIDED — recorded 2026-06-18. M-Prod sub-track #1 Phase B Part 1 SHIPPED 2026-06-18.

**Related:** ADR 0006 ([0006-audit-chain-substrate.md](0006-audit-chain-substrate.md)). This is an addendum that extends 0006's substrate-not-MVP discipline to the multi-token / RBAC / per-route ACL shape. Cross-references: [m-prod-auth.md](../backend/m-prod-auth.md) (milestone narrative, Phase B Part 1 section), [m-prod-tls-rotation.md](../backend/m-prod-tls-rotation.md) (Group B-2 #2 — the atomic-swap pattern this ADR reuses), [m-prod.md](../backend/m-prod.md) (cross-track contracts table).

## The decisions (five)

1. **Token comparison is constant-time across ALL registry entries (no early-exit).** `TokenRegistry::match_token` iterates the full `Vec<TokenEntry>` with `subtle::ConstantTimeEq::ct_eq` per entry (via the existing `BearerToken::matches` primitive). The first matching entry in declaration order wins, but the walk continues across all subsequent entries — an attacker can't time-attack distinguish "token matched entry N=1" from "token matched entry N=K". Per-byte timing is bounded by the existing `BearerToken::matches` length-check (already public surface; inherited Phase A surface that leaks token LENGTH per entry on a miss, NOT contents).
2. **Role enum is closed over `{Admin, ReadOnly, Custom(BTreeSet<String>)}`.** Three roles cover every Phase B Part 1 operational use-case: Admin (operator credentials), ReadOnly (CI scrapers, dashboards, SOC review pipelines), Custom (per-tenant / per-tool allowlist). JWT-OIDC subject claim mappings (deferred to Phase B Part 2) all fold into one of these three roles via a per-issuer config — no `Role` variant explosion downstream.
3. **`READ_ONLY_ROUTES` vocabulary IS the substrate** for the ReadOnly tier — a single static `&[&str]` that subsets `PROTECTED_ROUTE_LABELS`. The 8 entries (`/v1/identity`, `/v1/metrics`, `/v1/nodes`, `/v1/nodes/:id`, `/v1/nodes/:id/messages/recent`, `/v1/supervisor/tree`, `/v1/observer/events`, `/v1/observer/events.sse`) cover GET endpoints + the WS/SSE subscribe routes. POST endpoints mutate scheduler state and stay admin-only by default. Operators can't override per-deploy: a deploy-specific ACL is the `Custom` role.
4. **`RotatableRegistry` rides the Group B-2 #2 atomic-swap pattern** (`Arc<ArcSwap<TokenRegistry>>`) — not a separate primitive. The handle mirrors `RotatableServerConfig` bit-for-bit (the "Arc<ArcSwap<Arc<T>>>" wording in the M-Prod plan names the ATOMIC-SWAP PATTERN; the actual handle is `Arc<ArcSwap<T>>` because `ArcSwap` internally holds an `Arc<T>` and `load_full()` returns the right shape). Future SIGHUP reload (Phase B Part 2) calls `rbac.store(Arc::new(new_registry))` exactly the way `reload_tls_from_disk` calls `tls.rotate(...)`.
5. **New audit events ride the cross-cutting `graphreact::observer::audit` target** + the Phase A AuditSink fan invariant (tracing fires FIRST, sink fans AFTER — see [ADR 0006](0006-audit-chain-substrate.md) §"The decisions (five)" #1). `auth_success` is INFO + `auth_fail` / `authz_deny` are WARN. The `token_id` in the audit record is the operator-assigned `TokenId.0` string (NOT a hash, NOT the raw token). The existing `auth_fail` kind grows a third `reason` value `"expired"` alongside the Phase A `"missing"` / `"wrong"`.

## Why operator-assigned `TokenId` strings (NOT hashes) — superseding ADR 0006's `token_id` paragraph

ADR 0006's Phase A forward-compat note for the `fields` blob proposed that Phase B's `token_id` would be "a constant-time-comparable hash of the token bytes computed at load-time" ([0006 §"Phase B forward-compat for the `fields` blob"](0006-audit-chain-substrate.md#phase-b-forward-compat-for-the-fields-blob)). ADR 0008 deliberately reverses that prediction.

Phase B Part 1's operational threat model has the audit consumer being a SOC pipeline (Splunk / Sumo Logic / Elastic) where the analyst grepping `kind=auth_success` wants to see `token_id="alice-admin"` or `token_id="ci-runner"`, NOT `token_id="a3f8c2..."` that requires reverse-mapping every release against the operator's secret-vault inventory. The operator-assigned string is the audit-side identity Phase B's customers ACTUALLY consume.

The raw bearer credential remains in `BearerToken` (zeroized on drop), never logged. The constant-time-compare path is the `BearerToken::matches` primitive — that primitive doesn't change. ADR 0008 changes ONLY the AUDIT-SIDE label.

This reversal is reversible: a future ADR can re-add hash-derived `token_id_hash` as a SECOND audit field alongside the operator-assigned `token_id` string (e.g. for cross-deploy correlation), but Phase B Part 1 ships only the operator-assigned name. The `fields` blob's open shape (per ADR 0006 §"Why future RBAC events ride the same on-disk schema") means adding `token_id_hash` later doesn't bump the chain schema.

## Why constant-time across ALL entries (no early-exit)

Three options were considered:

- **(a) Early-exit on the first match.** REJECTED — an attacker who already controls one valid token can time the auth path against carefully-crafted near-miss tokens at varying positions in the registry. If `match_token` short-circuits on entry N's match, the response time when the attacker's malformed candidate "almost-matches" entry K reveals whether K is later in the registry than the attacker's own entry. Cross-entry timing-channel.
- **(b) Hash-first lookup table (HashMap<sha256-of-bytes, TokenEntry>).** REJECTED — would deflate the timing surface but adds (i) a SHA-256 compute per request, (ii) a HashMap-clone-or-rehash cost on rotation, (iii) a cache-line residency attack against operator-assigned keys. Constant-time-compare across N≤32 entries is sub-microsecond — under the request-path latency budget.
- **(c) Linear walk with `subtle::ConstantTimeEq` per entry, no early-exit.** CHOSEN. The walker visits every entry every time, the first-match-in-declaration-order returns once the loop completes. Per-entry cost is one `ct_eq` (~16ns for a 32-byte token); 32-entry walk is ~512ns of CPU per protected request. Negligible vs the rest of the request path (Phase A `bind` audit-write fsync is ~10µs, multi-tens-of-thousands of times more expensive). The recommended SOFT cap of `≤32` entries documented in `MAX_TOKENS` is the operator latency knob.

NOTE: The `BearerToken::matches` short-circuit on LENGTH mismatch (existing pre-Phase-B contract documented at `auth.rs:142`) is the inherited per-entry timing surface. It leaks the token LENGTH per registry entry on a miss, NOT contents. This is acceptable because the length is operator-chosen at registry construction time (not a property of the user-supplied candidate) and matches the existing single-token surface that's been in production since M16-Backend Phase 2. A future ADR can flip to a fixed-length-pad scheme if the length-leak becomes load-bearing.

## Why 3-variant `Role` (not a richer policy DSL)

Three options were considered:

- **(a) Richer policy DSL (e.g. expression-tree ACL: `(route == "/v1/identity") || (route =~ "/v1/nodes/.+" && method == "GET")`).** REJECTED — operator-side ACL DSLs become the engineer-side bug surface (policy parse errors at config load, capture-group regex injection, audit-side opacity because the matched POLICY rather than the matched ENTRY is the verdict). Substrate-not-MVP: a half-on policy engine is the kind of thing operators forget about and security audits flag.
- **(b) Single allowlist per token (no Admin / ReadOnly tiers — every token carries its own `BTreeSet<String>`).** REJECTED — operator overhead. Every Admin token would need to enumerate all 11 PROTECTED_ROUTE_LABELS entries; every ReadOnly token would need to enumerate the 8 READ_ONLY_ROUTES entries. The two tier names ARE the ergonomic operator surface.
- **(c) Three closed-over variants `{Admin, ReadOnly, Custom(BTreeSet)}` — Admin/ReadOnly are short-hands, Custom is the escape hatch.** CHOSEN. Covers every Phase B Part 1 use-case + the Custom escape hatch + Rust's pattern-match exhaustiveness checking guarantees every audit-side `role_str` extraction stays current with the Role enum.

Phase B Part 2's JWT-OIDC bridge will fold issuer subject claims into these three variants via a per-issuer config — no Role variant explosion.

## Why `READ_ONLY_ROUTES` is a single static `&[&str]` (not a per-deploy config)

ADR 0006 §"Substrate-not-MVP receipt — what we deliberately did NOT defer" set the precedent: the substrate ships THE complete primitive the application space needs, not a parameterized half. `READ_ONLY_ROUTES` is the canonical read-only surface across every Phase B deploy — every CI scraper that asks for "read-only access" gets the same answer. A deploy that needs ONE extra route in the read-only tier uses `Role::Custom`; a deploy that needs ONE route REMOVED from read-only uses `Role::Custom` with the trimmed allowlist.

The static slice means there's exactly one source line to change when a Phase C / Phase D protected route should be added to the read-only tier. The unit test `read_only_routes_subset_of_protected_route_labels` pins the cross-module invariant so a future PROTECTED_ROUTE_LABELS reshuffle catches mismatches at compile-test rather than at deploy-time RBAC silent-bypass.

## Why `RotatableRegistry` mirrors `RotatableServerConfig` shape (not a separate primitive)

Sub-track #2's `RotatableServerConfig` already solved the atomic-swap problem for `Arc<rustls::ServerConfig>`: an `Arc<ArcSwap<T>>` newtype, cheap `load_full()` reads on the per-connection hot path, atomic `store(...)` writes from the eventual SIGHUP handler. The pattern is locked as a cross-track contract in [m-prod.md](../backend/m-prod.md#cross-track-contracts-locked-here) ("Atomic-swap pattern — `Arc<ArcSwap<Arc<T>>>` newtype").

Reusing the same shape for `TokenRegistry` means:

- One mental model for operators (TLS rotates this way, tokens rotate this way, Phase B Part 2's SIGHUP handler triggers both via the same code path).
- One audit surface for verifying the rotation took (`tls_reload_success` already exists; Phase B Part 2's `token_registry_reloaded` event will ride the same target + the same fan invariant).
- One regression-risk surface (a future tweak to the `ArcSwap` semantics — e.g. tagging `load()` with span metadata — applies to both rotations uniformly).

The wording "`Arc<ArcSwap<Arc<T>>>`" in the M-Prod plan ([m-prod.md](../backend/m-prod.md#cross-track-contracts-locked-here)) names the ATOMIC-SWAP PATTERN; the actual handle is `Arc<ArcSwap<T>>` because `ArcSwap` internally holds an `Arc<T>` and `load_full()` returns the right `Arc<T>` shape. This ADR makes the shape choice load-bearing-explicit so future reviewers don't mistake the type signature for a deviation from the contract.

## Reversibility

| Decision | Reversibility |
|---|---|
| Constant-time across ALL entries | Reversible — a future `match_token_fast` API can be added alongside `match_token` for deploys that accept the timing trade-off. The original walker stays the default. |
| 3-variant Role enum | Reversible — adding a fourth `Role::PolicyDSL(String)` variant later is additive; Rust's pattern-match exhaustiveness check catches every audit-side `role_str` extraction at compile time. |
| `READ_ONLY_ROUTES` static slice | Reversible — adding/removing entries is one source line. The cross-module subset unit test catches drift against `PROTECTED_ROUTE_LABELS`. |
| Operator-assigned `TokenId` (not hash) | Reversible — adding a sibling `token_id_hash` field to the audit record is additive (open `fields` blob per ADR 0006). The current `token_id` string stays the audit principal. |
| `RotatableRegistry = Arc<ArcSwap<TokenRegistry>>` | Reversible — wrapping into a richer handle (e.g. with reload-timestamp metadata) is additive; rotation callers still call `.store(...)`. |
| `state.rbac = None` keeps Phase A bit-for-bit | Locked. Removing this would break the M-Prod sub-track #1 strict-additive contract ([m-prod.md](../backend/m-prod.md#cross-track-contracts-locked-here)). The Phase A single-token path is regression-tested in `audit_chain_e2e.rs::no_sink_default_keeps_legacy_behavior`. |
| `auth_success` / `authz_deny` audit kinds | Reversible — opaque-to-replay (the chain hash covers the WHOLE record per ADR 0006). A future schema bump that renames kinds is one source change at the emit sites; the chain stays verifiable. |

## Cross-track contracts (locked here, executed elsewhere)

Mirrors the ADR 0006 table layout:

| Track | Contract |
|---|---|
| **#2 TLS rotation** (SHIPPED) | Phase B Part 1 reuses the `RotatableServerConfig` atomic-swap shape directly via `RotatableRegistry`. Phase B Part 2's SIGHUP handler runs both rotations off the same signal arm. |
| **#3 Secrets keyring** (Phase B Part 1 SHIPPED) | Phase B Part 2's JWT-OIDC bridge will load signing keys via the `HostSecretCallback` / `SecretStore` surface — operator-assigned `TokenId` maps to the secret-vault keyring entry name. The raw secret NEVER reaches the audit record. |
| **#4 SIGHUP lifecycle** (Phase B Part 1 SHIPPED for TLS) | Phase B Part 2's `RotatableRegistry::store(...)` rides the same SIGHUP handler that already drives TLS reload. New audit kinds (`token_registry_reloaded` / `token_registry_reload_failed`) ride the existing fan invariant. |
| **#6 Constant-time compare + per-token rate limit** (SHIPPED for per-IP) | Phase B Part 2's per-token rate-limit extractor pulls `TokenId.0` from the auth result (matched on the request path) and keys the limiter on the operator-assigned name. The constant-time compare per-entry stays unchanged. |
| **#9 Chaos arm** (DEFERRED) | Phase B Part 2's chaos arms will include credential-stuffing scenarios (low-rate matching against the full 32-entry registry under heavy concurrent load) to falsify the no-early-exit timing claim under production-realistic conditions. |

## Open questions tracked (not blockers)

- **Per-route ACL with method discrimination.** Phase B Part 1's `check_route` is purely path-keyed. A future deploy may want "ReadOnly grants GET /v1/nodes/:id/messages/recent but NOT POST /v1/nodes/:id/messages/recent" (currently moot because the latter isn't a protected route). The `Role` enum reshape is reversible per the table above.
- **Per-tenant / per-namespace partition.** Phase B Part 1 has no tenant scoping — all entries see all PROTECTED_ROUTE_LABELS. Phase D is the catalogued layer (per the [m-prod.md](../backend/m-prod.md#disposition-table-9-sub-tracks) deferred items).
- **Audit-side rate-limit on `auth_fail` floods.** A credential-stuffing flood would generate one chain record per attempt + one `sync_data` per record (Phase A discipline). Phase B Part 2's batched-fsync worker addresses this; ADR 0008 inherits the deferred status.
- **Token expiry granularity.** Phase B Part 1 takes `SystemTime` (millisecond-precise wall clock). A future deploy may want monotonic-clock-based expiries to dodge wall-clock jumps. The change is additive to `TokenEntry` (`monotonic_expires_at: Option<Instant>` field).
