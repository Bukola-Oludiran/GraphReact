# 0009 — Multi-token RBAC registry on-disk file format: NDJSON one-record-per-line, raw bearer bytes, mode 0o600, defensive parse with per-line skip-and-warn

**Status:** DECIDED — recorded 2026-06-26. M-Prod sub-track #1 Phase B Part 2 (Group 1ab) SHIPPED 2026-06-26.

**Related:** [ADR 0006](0006-audit-chain-substrate.md) (NDJSON precedent for the audit chain; same per-line-skip-on-parse-error discipline), [ADR 0008](0008-multi-token-rbac.md) (the `TokenRegistry` / `TokenEntry` / `Role` substrate this file feeds). Cross-references: [m-prod-auth.md §"Shipped — Phase B Part 2 (Group 1ab)"](../backend/m-prod-auth.md#shipped--phase-b-part-2-group-1ab-2026-06-26), [m-prod.md](../backend/m-prod.md) (cross-track contracts table).

## Why an ADR (and why now)

ADR 0008 defined the in-memory `TokenRegistry` / `TokenEntry` / `Role` shapes but left the on-disk file format unspecified — Phase B Part 1 wired RBAC through `SpawnOptions::rbac` programmatically only. Phase B Part 2 Group 1ab introduces the SIGHUP-driven reload path that needs an on-disk file to re-read, so the format becomes operator-visible substrate. Inventing the format inside the implementation PR without an ADR would leave operators guessing about forward-compat guarantees, and would risk a silent breaking change the first time a hash-prefixed or JSON-array shape was considered. This ADR pins the choice + documents the reasoning.

## The decisions (five)

1. **One record per line, NDJSON.** Each line is a single JSON object encoding one `TokenEntry`-equivalent record. Mirrors [ADR 0006](0006-audit-chain-substrate.md)'s choice for the audit chain — same `serde_json::to_vec` shape, same line-oriented defensive parse, same "operators can `tail -f` it" debuggability. Comment lines starting with `#` are skipped. Blank lines are skipped.
2. **The raw bearer credential bytes live in the `token` field as a UTF-8 string.** The file IS the credential — mode 0o600 protects the bytes at the filesystem layer. No bcrypt / scrypt / argon2 prehash. The constant-time compare path (`subtle::ConstantTimeEq` via `BearerToken::matches`) runs against the raw bytes; a prehash would force every protected request to compute SHA-256 per candidate, defeating the latency budget without buying any threat-model protection (the file's mode-0o600 contains the same secret material a bcrypt-protected file would).
3. **Mode 0o600 is enforced on Unix BEFORE any read happens.** Mirrors [`BearerToken::from_file`](../../crates/graphreact-observer/src/auth.rs) and [`FileAuditSink::open`](../../crates/graphreact-observer/src/audit_chain.rs) exactly. A file with group- or world-readable permissions surfaces as `RegistryLoadError::Mode` and refuses to load. Windows file-mode ACLs are deferred ([m-prod-auth.md §"Phase B/C/D scope"](../backend/m-prod-auth.md#phase-bcd-scope)).
4. **Defensive parse: malformed individual lines are LOGGED + SKIPPED, not fatal.** The same precedent ADR 0006 set for the audit chain: an operator hand-editing the file shouldn't blow up the entire reload over one bad row. A `tracing::warn!(target: "graphreact::observer::audit", event = "token_registry_reload_skipped_line", line_index = N, error = "…")` line emits per skip. If EVERY line is malformed (file empty / all lines invalid), the load returns `RegistryLoadError::Parse` and the caller keeps the prior in-memory registry. If at least one valid entry parses, the result feeds `TokenRegistry::new` for the second-pass validation (empty / too-many / duplicate-id).
5. **`expires_at` is `Option<u64>` Unix-milliseconds, NOT ISO8601.** Parsing ISO8601 / RFC3339 requires `chrono` or `time` — neither is a workspace dep, and ADR 0008 + this ADR both inherit the "no new workspace deps" Group 1ab constraint. The trade-off is operator ergonomics: integers are harder to eyeball than `2026-12-31T23:59:59Z`. Reversible — a future ADR can add a sibling `expires_at_iso8601` field once the workspace takes a date-parse dep. The schema's `serde(default)` annotation on the field makes the addition backward-compatible.

## On-disk schema

```json
{"id":"alice-admin","token":"<raw-utf8-bearer-bytes>","role":"Admin"}
{"id":"ci-reader","token":"<raw>","role":"ReadOnly","expires_at_unix_ms":1735689599000}
{"id":"tenant-foo","token":"<raw>","role":{"Custom":["/v1/metrics","/v1/nodes/:id/call"]}}
# Comment lines are ignored.
```

Per-record fields:

| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | `String` | yes | Operator-assigned identity; round-trips to `TokenId(String)`. Audit-side principal name (NOT a hash, NOT the credential). |
| `token` | `String` | yes | Raw bearer credential bytes as UTF-8. Round-trips to `BearerToken::from_bytes`. Empty / invalid bytes cause the line to be skipped with a warning. |
| `role` | `Role` | yes | One of the closed three variants. JSON shape: `"Admin"` (bare string for the unit variant), `"ReadOnly"` (bare string), or `{"Custom": ["/v1/…", "/v1/…"]}` (externally-tagged for the data variant). |
| `expires_at_unix_ms` | `Option<u64>` | no (default `null`) | Unix epoch in milliseconds. Round-trips to `Option<SystemTime>`. `null` / absent = never expires. |

## File-level guarantees

- **Order is preserved.** Entries deserialize into the same `Vec<TokenEntry>` order they appear on disk — `TokenRegistry::match_token` returns the first matching entry in declaration order, so an operator who puts admin tokens at the top of the file gets the same in-memory dispatch ordering.
- **Defensive parse cuts off per-line, not per-record-field.** A line with a missing `role` field fails serde and skips; a line with all required fields but with `role: "Operator"` (which doesn't exist) also skips. Skipping happens BEFORE the registry-level validation (empty / too-many / duplicate-id) so an operator can recover from a partially-broken file by removing the bad lines.
- **No streaming protection.** The whole file is read into memory before parse begins. With `MAX_TOKENS = 256` and recommended ≤ 32 entries per file, total file size is bounded at ~50 KiB even with the longest reasonable token bytes — streaming buys nothing.
- **No backward-compatibility for unknown fields YET.** Today the parser uses `#[derive(Deserialize)]` without `#[serde(deny_unknown_fields)]`, so future fields can be added without breaking older daemons that ignore them. Operators are encouraged NOT to add unknown fields hand-edited — the audit row's emit shape would not capture them.

## Why NDJSON over alternatives

Three options were considered:

- **(a) JSON array** (`[{...}, {...}, ...]`). REJECTED — a syntax error anywhere in the file invalidates the whole array (single closing `]` ten KB away). Defensive per-record skip becomes impossible. The audit chain made the same decision in [ADR 0006](0006-audit-chain-substrate.md).
- **(b) TOML / INI** (sections per token). REJECTED — adds a second parser to the workspace (no current dep on `toml`), syntax-error recovery is even harder than JSON-array, and the readability benefit on small files (`<32` entries) is marginal vs. NDJSON's `jq`-friendliness.
- **(c) NDJSON one record per line.** CHOSEN. Per-line defensive parse, `jq` / `awk` / `grep` friendliness, mirrors the audit chain's on-disk discipline, encoding shape is fully determined by `#[derive(Serialize, Deserialize)]` so no hand-rolled formatter is needed.

## Why raw bearer bytes over bcrypt-prehash

Three options were considered:

- **(a) bcrypt-prehash the token bytes; store the hash in the file.** REJECTED — every protected request would compute `bcrypt_verify(candidate, stored_hash)` per registry entry, an ~80ms-per-attempt CPU cost. With 32 entries and constant-time-across-ALL discipline, the per-request floor would balloon by 2.5 seconds. The file already needs mode 0o600 to keep the secret material safe — bcrypt-prehash buys nothing the filesystem permissions don't already.
- **(b) SHA-256-prehash the token bytes; store the hash + a per-entry salt.** REJECTED — same constant-time argument scales linearly with `len(registry)`; per-entry SHA-256 is ~200ns so a 32-entry walk is ~6.4µs of CPU per request. Marginal but unnecessary — `ct_eq` over raw bytes is already constant-time + ~16ns/entry.
- **(c) Raw UTF-8 bytes in the file; mode 0o600 protects them.** CHOSEN. Mirrors `BearerToken::from_file` (the Phase A single-token surface): operator picks long random bytes (recommended 32+ bytes of base64-url-safe entropy), mode 0o600 ensures only the daemon user can read them.

## Operator workflow

```bash
# 1. Create the registry file with mode 0600 from the start.
install -m 0600 /dev/null /etc/grx/tokens.ndjson

# 2. Append entries one per line. Tokens are raw bytes (not hashed).
cat > /etc/grx/tokens.ndjson <<'EOF'
{"id":"alice-admin","token":"AbCdEf123456...","role":"Admin"}
{"id":"ci-scrape","token":"GhIjKl654321...","role":"ReadOnly"}
EOF

# 3. Boot the daemon with --observer-tokens pointing at the file.
grx cluster daemon \
  --node-id alice@127.0.0.1:9000 \
  --listen 127.0.0.1:9000 \
  --observer-bind 127.0.0.1:9090 \
  --observer-tokens /etc/grx/tokens.ndjson \
  --observer-cert /etc/grx/cert.pem \
  --observer-key /etc/grx/key.pem \
  --observer-audit-log /var/log/grx/audit.ndjson

# 4. Add / remove / rotate tokens by editing the file + firing SIGHUP.
#    The daemon emits `token_registry_reloaded` on success or
#    `token_registry_reload_failed` (keeping the prior registry) on
#    parse / validate failure.
$EDITOR /etc/grx/tokens.ndjson
grx admin reload-tokens --pid $(pgrep -f 'grx cluster daemon')
# OR: kill -HUP $(pgrep -f 'grx cluster daemon')
```

## Reversibility

| Decision | Reversibility |
|---|---|
| NDJSON one record per line | Reversible — a future ADR could add JSON-array support behind a `--observer-tokens-format` CLI flag (or auto-detect by sniffing the first byte). Operators on NDJSON would migrate by `jq -n '[inputs]'`-piping. The NDJSON parser stays the default for back-compat. |
| Raw bearer bytes (no prehash) | Reversible — a future ADR could add a sibling `token_hash` field (with explicit `algo` discriminator) alongside `token`; the parser falls back to constant-time compare against whichever field is present. Mixing raw + hashed tokens in one file would be an operator-side ergonomic problem but technically supported. |
| Mode 0o600 enforced | Locked. Loosening would create a real-world credential leak under the multi-tenant deployment threat model. |
| Defensive per-line parse | Locked. Substrate-not-MVP: a half-on parser that aborts on the first bad line is the kind of thing operators forget about and SREs file post-mortems on. |
| `expires_at` as Unix-millis u64 | Reversible — adding `expires_at_iso8601: Option<String>` as a sibling field is additive. Operators using the new field would set `expires_at_unix_ms` to `null`; the parser picks whichever is `Some`. |

## Cross-track contracts (locked here, executed elsewhere)

| Track | Contract |
|---|---|
| **#1 Audit chain (Phase A + Phase B)** | Every `load_registry_from_file` call emits at least one audit event: `token_registry_reloaded` on success (warn-level), `token_registry_reload_failed` on parse / mode / validate failure (warn-level, with the `error_kind` discriminator from `RegistryLoadError::kind_str`). Per-line skip emits `token_registry_reload_skipped_line` as tracing-only (no sink fan — operators reading the audit chain don't need every malformed line in the tamper-evident ledger). |
| **#2 TLS rotation** (SHIPPED) | SIGHUP arm is the SHARED FAN-OUT POINT. Group 1ab extends the gate from `tls_paths.is_some()` to `tls_paths || tokens_path || audit_log_path`; the `grx-sighup-ready PID` readiness line stays emit-once. |
| **#3 Secrets keyring** (SHIPPED) | Token file is operator-managed at the filesystem layer, NOT through the keyring backend. Future Phase B Part 2 Group 1cd may add a `--observer-tokens-keyring <name>` flag that reads from the OS keyring instead of the filesystem, keeping the file path as the back-compat default. |
| **#4 SIGHUP lifecycle** (SHIPPED for TLS) | Drain-respect: `DaemonHandle::reload_tokens_from_disk` short-circuits when `DrainFlag::is_draining`, emitting `token_registry_reload_skipped reason="draining"` audit row in place of the attempt/success/failure trio. |
| **#5 Observability** | `audit_sink_write_failures_total{kind}` counter rides this load path: `kind="open"` for I/O failures on the file. The audit-side reload events (`token_registry_reload_failed`) carry the same `error_kind` discriminator for correlation. |
| **#6 Constant-time compare** (SHIPPED) | The file format is orthogonal to the constant-time compare — `match_token` still walks all entries with `ct_eq` per byte regardless of how the file loaded. |
| **#9 Chaos arm** (DEFERRED) | Future chaos arms include malformed-line stress (random byte-corruption mid-file) + permission-flip stress (mode bits flipped under live daemon) — Group 1ab's defensive parse + mode-0o600-enforcement are the substrate for those tests. |

## Open questions tracked (not blockers)

- **Operator-side editor lock.** Today the file is read in its entirety on SIGHUP; an editor that does in-place save with a temp file + rename will reliably produce a consistent snapshot, but an editor that writes-then-truncates may cause the load to see a truncated file. The defensive parse keeps the prior registry on total-parse-failure, so the failure mode is benign — but a `RegistryLoadError::Truncated` discriminator would be nice for triage. Deferred.
- **Backward-incompat field addition.** Today's parser does NOT enforce `deny_unknown_fields`. Adding a future required field (e.g. `tenant: String`) would silently no-op on an older file. The mitigation is a `schema_version: u32` field at the top of the file (one-record header line) — deferred until a real field-shape change demands it.
- **Multi-file token registries.** Today the daemon accepts ONE `--observer-tokens <PATH>`. Some deploys (per-tenant credential vaults mounted as separate volumes) may want to concatenate multiple files. A future flag like `--observer-tokens-dir <DIR>` that globs `*.ndjson` and union-loads would be additive; deferred until operator demand surfaces.
- **JWT-OIDC bridge composition.** Phase B Part 2 Group 1cd will add JWT-OIDC validation alongside the static token file. The composition question — "what wins when a JWT token also matches a static entry?" — will be settled in the Group 1cd ADR; today's surface is static-token-file-only.
