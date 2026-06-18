# M-Prod sub-track #3 — Secrets handling (Phase A)

**Status:** SHIPPED 2026-06-17 (Phase A: operator CLI + library substrate). Tracks M-Prod sub-track #3 as defined in the [master plan](../../../../.claude/plans/graphreact-rust-later-milestones.md).

Lives alongside [m-prod-observability.md](m-prod-observability.md) and [m-prod-supply-chain.md](m-prod-supply-chain.md). The Phase B runtime accessor (`ctx.secret(name)` inside `.grx` bodies) is **deferred** — see [Phase B (deferred)](#phase-b-deferred).

## Sub-track #3 scope recap

The M-Prod plan calls out:

1. Operator-managed secrets backed by OS keyring (macOS Keychain / Linux Secret Service / Windows Credential Manager).
2. Env-var passthrough for container deploys (k8s `env:`, Docker `--env-file`, systemd `Environment=`).
3. Bodies fetch secrets via `ctx.secret("DATABASE_URL")` from inside `.grx` definitions — _Phase B_.

**Phase A** (this slice) ships items 1 + 2 + the typed library + the operator CLI surface. **Phase B** ships item 3.

## Shipped — `graphreact-secrets` library crate

New workspace member at [`crates/graphreact-secrets`](../../crates/graphreact-secrets/). Public surface:

- **`SecretValue`** — `Vec<u8>` wrapper deriving `Zeroize + ZeroizeOnDrop`. `Debug` redacts the contents (prints `len` only). Construction validates non-empty + ≤ `MAX_SECRET_BYTES` (4096 bytes — sized to the smallest production keyring backend, Windows Credential Manager).
- **`SecretStore` trait** — object-safe `set / get / list / remove`. Implementations must be `Send + Sync` so the CLI can dispatch through `Box<dyn SecretStore>`.
- **`KeyringStore`** — wraps the [`keyring`](https://crates.io/crates/keyring) crate (v3.x) with platform-pinned features:
  | Platform | Feature | Backend |
  |---|---|---|
  | macOS | `apple-native` | system Keychain via `security-framework` |
  | Linux | `linux-native-sync-persistent` | Secret Service over dbus (gnome-keyring / kwallet) |
  | Windows | `windows-native` | Credential Manager via `windows-sys` |

  Service-name is fixed to `"graphreact"`; the secret name is the account. Entries are discoverable via `security find-generic-password -s graphreact -a DATABASE_URL` (macOS) / `secret-tool search service graphreact account DATABASE_URL` (Linux) without exposing the value.

- **`EnvStore`** — reads `GRX_SECRET_<UPPER_NAME>` env vars. Production path for headless CI + containers (no GUI keyring agent). `set` / `remove` mutate `std::env` (process-local, ephemeral).

- **`validate_secret_name`** — `[A-Z][A-Z0-9_]*` shape enforced at the boundary so names round-trip cleanly between the keyring + env backends.

- **`SecretsError`** — single typed error surface; backend-specific quirks (dbus timeouts, TouchID dismissal, wincred "too large") all funnel into `BackendUnavailable` / `TooLarge` / `NotFound`.

License: MIT (workspace inheritance). No new advisories — `keyring` v3.x and its per-platform feature closures are MIT/Apache-2.0, already covered by `deny.toml`'s allow-list.

## Shipped — `grx secrets` CLI subcommand

| Subcommand | Reads stdin? | Writes stdout? | Idempotent? |
|---|---|---|---|
| `grx secrets set <NAME> [--backend keyring\|env]` | yes (no-echo on TTY; line-buffered on pipe) | no | upsert |
| `grx secrets get <NAME> [--backend keyring\|env]` | no | raw bytes (no trailing newline) | n/a |
| `grx secrets list [--backend keyring\|env]` | no | one name per line, sorted | n/a |
| `grx secrets remove <NAME> [--backend keyring\|env]` | no | no | yes (no-op if absent) |

`--backend keyring` is the default — developer workstations land here. `--backend env` is the documented production path for containerised deploys.

`set` uses [`rpassword`](https://crates.io/crates/rpassword) to disable terminal ECHO on TTY input. Piped input emits a stderr warning that the value may have hit shell history / process listings. The plaintext lives in a single heap allocation that is transferred into `SecretValue::from_string` (which zeroizes the original buffer on success).

`get` writes the raw bytes to stdout **without** a trailing newline so callers can compose with `--token-file <(grx secrets get DATABASE_URL)` or `KEY=$(grx secrets get API_KEY)` without an off-by-one byte.

`list` returns the empty vector on the keyring backend in Phase A (the `keyring` crate's enumeration surface isn't uniform across platforms; tracked upstream as `hwchen/keyring-rs#193`). Operators who need enumeration use `--backend env`.

`remove` is idempotent — removing an absent entry returns exit code 0 so `grx secrets remove FOO` is safe to re-run.

### CLI safety invariants

- The plaintext **never** appears as a CLI argument (no `ps aux` leak). The `set` command reads it from stdin.
- The structured audit log (`graphreact::secrets::audit` target) records the action + backend + name, **never** the value.
- `Debug`-derived logs are safe: `SecretValue` redacts contents.
- Length validation surfaces `TooLarge { len, limit }` at the library boundary instead of letting the backend produce a Windows-specific "credential too large" message.

## Backend selection matrix

| Workload | Recommended backend | Why |
|---|---|---|
| Developer workstation (macOS / Linux desktop / Windows desktop) | `keyring` | OS keyring is unlocked at session start; survives process restarts. |
| CI runners (GitHub-hosted, GitLab, CircleCI) | `env` | No GUI agent; secrets injected via runner `secrets:` block → `GRX_SECRET_*` env vars. |
| Docker / k8s containers | `env` | Mount Docker secrets / k8s `Secret` resources directly as `GRX_SECRET_*` env vars. |
| Headless SSH session into a macOS box | `env` | Keychain prompts a TouchID modal on a non-GUI session; either `launchctl asuser` your way in or fall back to `env`. |

## Audit trail

Every `set` / `remove` operation emits a structured event on the `graphreact::observer::audit`-shaped target `graphreact::secrets::audit`:

```text
graphreact::secrets::audit  backend=keyring  action=set     name=DATABASE_URL
graphreact::secrets::audit  backend=env      action=remove  name=DATABASE_URL
```

`get` and `list` deliberately do NOT emit audit events at this layer — they're read-paths that a sub-track #1 audit-chain sink will own once it ships (so the audit log captures the calling principal, not just the action). Tracking under [Phase B (deferred)](#phase-b-deferred).

## Phase B (deferred)

Phase B ships `ctx.secret("DATABASE_URL")` from inside `.grx` definitions. It is blocked on:

1. **Engine trait surface change.** The Engine trait needs a host-callback for secret-read so JSC + CPython + Go can each marshal `ctx.secret(name)` to the same `Box<dyn SecretStore>`.
2. **Per-engine wiring.** JSC's `__augmentCtx`, CPython's `__augment_ctx`, and Go's generated prelude each need `ctx.secret` plumbed (mirroring how `ctx.spawnChild` was added across all three engines in Cleanup-Pass `m66-method-style-ctx`).
3. **ADR-0005.** A short ADR locking the contract (per-call store dispatch vs cached read; manifest declaration `needs_secret = ["DATABASE_URL"]` for principle-of-least-knowledge; default-deny shape for non-declared secrets).
4. **Audit-of-secret-reads.** Once sub-track #1 (audit-chain sink) ships, Phase B `ctx.secret` reads emit through the same hash-chained log.

## Open work in #3

- **Phase B per-engine plumbing** (above).
- **SIGHUP-triggered re-read** when the operator rotates a secret without restarting the daemon — depends on sub-track #2 atomic-swap pattern shipping first.
- **`KeyringStore::list` enumeration** — depends on `keyring` upstream landing a portable `search` API (`hwchen/keyring-rs#193`).
- **Keychain access-groups** for cross-process sharing — requires code-signing `grx` with `keychain-access-groups` entitlements; lands in M-Prod #8 packaging.
- **Audit-of-secret-reads** — sub-track #1 dependency (above).

## Cross-track contracts locked here

- **Sub-track #1 (AuthN/AuthZ):** The audit-chain sink owns the secret-read audit when it ships. Phase A's `get`/`list` are silent at the audit layer to avoid a half-audit (action without principal).
- **Sub-track #2 (TLS/mTLS lifecycle):** The SIGHUP atomic-swap pattern becomes the secret-rotation reload path. No code shape locked in Phase A.
- **Sub-track #8 (packaging):** Keychain access-groups + code-signing for cross-process sharing belong to the release-pipeline track.
- **`deny.toml`:** Unchanged — the `keyring` transitive closure is MIT/Apache-2.0 (covered by the existing allow-list); `rpassword` is MIT/Apache-2.0; both validated by `cargo deny check` on the shipped branch.

## Cross-references

- [`m-prod-observability.md`](m-prod-observability.md) — neighbouring M-Prod sub-track narrative.
- [`m-prod-supply-chain.md`](m-prod-supply-chain.md) — `cargo deny check` floor that gates both `keyring` and `rpassword` additions.
- [`m16-management-api.md`](m16-management-api.md) — pre-existing `BearerToken` zeroize pattern that `SecretValue` mirrors.
- [`graphreact-secrets/src/lib.rs`](../../crates/graphreact-secrets/src/lib.rs) — Phase A library surface.
- [`graphreact-cli/src/secrets_commands.rs`](../../crates/graphreact-cli/src/secrets_commands.rs) — `grx secrets` dispatcher.
