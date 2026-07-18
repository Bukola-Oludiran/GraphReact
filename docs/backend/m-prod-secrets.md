# M-Prod sub-track #3 — Secrets handling

**Status:** SHIPPED 2026-06-17 (Phase A) + SHIPPED 2026-06-18 (Phase B Part 1) + SHIPPED 2026-07-18 (Phase B Part 2: production JSC/CPython/Go access, manifest enforcement, atomic SIGHUP rotation, hash-chain audit fan, and failure metrics).

Lives alongside [m-prod-observability.md](m-prod-observability.md) and [m-prod-supply-chain.md](m-prod-supply-chain.md).

## Sub-track #3 scope recap

The M-Prod plan calls out:

1. Operator-managed secrets backed by OS keyring (macOS Keychain / Linux Secret Service / Windows Credential Manager).
2. Env-var passthrough for container deploys (k8s `env:`, Docker `--env-file`, systemd `Environment=`).
3. Bodies fetch secrets via `ctx.secret("DATABASE_URL")` from inside `.grx` definitions — _Phase B Part 1 (JSC) shipped 2026-06-18; CPython + Go in Part 2_.

**Phase A** (2026-06-17) shipped items 1 + 2 + the typed library + the operator CLI surface. **Phase B Part 1** (2026-06-18) shipped the substrate for item 3: the Engine trait surface, the JSC slice, the manifest-side `needs_secret` allowlist field, ADR-0007, and an integration test. **Phase B Part 2** wires CPython + Go, the production-side host-callback factory, SIGHUP re-read, and audit-of-secret-reads.

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

`get` and `list` deliberately do NOT emit runtime-read events at this operator layer. Guest reads now audit at the production callback boundary, where the active node and committed manifest are known.

## Phase B Part 1 — `ctx.secret(name)` substrate (SHIPPED 2026-06-18)

Phase B splits into two parts. **Part 1** ships the substrate that pins the contract; **Part 2** wires CPython + Go and adds the SIGHUP re-read + audit-of-secret-reads. Part 1 lands the JSC slice on its own so the contract surface is reviewable in isolation.

What shipped on 2026-06-18:

1. **ADR-0007** — [`0007-ctx-secret-runtime-accessor.md`](../decisions/0007-ctx-secret-runtime-accessor.md). Locks the five contract points: per-call host-callback dispatch (no engine-side cache); manifest-declared `needs_secret = ["NAME1", "NAME2"]` allowlist with default-deny; `SecretStore: Send + Sync` thread-safety contract; `Engine::host_secret_callback() -> Option<&dyn HostSecretCallback>` trait surface with `None` default; `SecretValue` rides the boundary as a `HostValue::String` (UTF-8) for the JSC slice with Phase B Part 2 widening planned for typed-bytes.
2. **Engine trait surface change** — `graphreact-engine-zig` gains `HostSecretCallback` trait + `Engine::host_secret_callback()` default-`None` method. All six engine impls (JSC, CPython, Go, Echo, Null, Zig-null) compile unchanged: the `None` default keeps the non-secret-aware engines bit-for-bit identical.
3. **JSC wiring** — `zig/bridge/jsc.zig`'s `__augmentCtx` prelude gains a `rawCtx.secret = function(name) { ... }` shim that forwards to a registered `host_secret` BoxCallback. `JscEngine` overrides `host_secret_callback()` via a process-global `OnceLock<Arc<dyn HostSecretCallback>>` shadow; `set_host_secret_callback` + `host_secret_callback_arc()` setter / reader pair carries the trait-shape predicate the daemon-side orchestrator queries.
4. **Permission manifest field** — `PermissionManifest::needs_secret: Option<Vec<String>>`. `None` matches Phase A pre-existing semantics bit-for-bit (no declared secrets → default-deny → every `ctx.secret(name)` resolves to null). `PermissionManifest::new` initialises to `None`; the `Debug` impl includes the field; `Default` derive carries it. Existing call-sites that use `..Default::default()` keep compiling.
5. **Integration test** — `crates/graphreact-engine-jsc/tests/ctx_secret.rs`. Three cases prove the contract over a real JSC isolate: declared+present returns the value, declared+missing returns `null`, undeclared returns `null` even when the store has the value.

Part 2 now fans `secret_read_allow` / `secret_read_deny` to both tracing and the hash-chained `AuditSink`; values are never included.

## Phase B Part 2 — runtime access and rotation (SHIPPED 2026-07-18)

The cross-track audit-chain and atomic-swap prerequisites are now landed. Part 2 ships:

1. **CPython runtime accessor.** `ctx['secret'](name)` is injected through `__augment_ctx`; a CPython isolate test locks the safe absent-callback behavior.
2. **Go runtime accessor.** Generated `Ctx.Secret(name string) string` dispatches through `Invoke`; the codegen emission test locks the method into the prelude.
3. **Production-side `host_secret` factories.** JSC, CPython, and Go callbacks close over the shared runtime context, read `Scheduler::manifests` by `current_node_id()`, enforce `needs_secret`, then dispatch to the store.
4. **SIGHUP-triggered re-read.** `ArcSwap<Arc<dyn SecretStore>>` reuses the established non-TLS atomic-swap idiom. Existing callbacks retain the outer context and observe the next snapshot without re-registration.
5. **Audit-of-secret-reads.** `secret_read_allow` / `secret_read_deny` events ride the same `graphreact::observer::audit` target + `AuditSink::append` fan as Phase B Part 2's auth events. Logs the name, NEVER the value.
6. **Failure metric.** `grx_secret_read_failures_total{reason}` uses the closed vocabulary `undeclared`, `missing`, `backend_error`, and `non_utf8`.

Runtime selection uses `GRX_RUNTIME_SECRET_BACKEND=env|keyring` and defaults to `env`. The callback stays per-call and uncached. The rotatable handle is `ArcSwap<Arc<dyn SecretStore>>`; SIGHUP order is audit log → audit-forwarder spool → runtime secrets → tokens → JWKS → TLS.

The first render remains default-deny: authorization consults the last committed `PermissionManifest`. A body returns `needs_secret`, then reads on a later render. JSC/Python return null/`None` on deny; Go returns `""`.

## Open work in #3

- **Cached-vs-uncached performance knob** — evidence-gated; ADR-0007 retains per-call dispatch until measurements justify cache invalidation complexity.
- **`KeyringStore::list` enumeration** — depends on `keyring` upstream landing a portable `search` API (`hwchen/keyring-rs#193`).
- **Keychain access-groups** for cross-process sharing — requires code-signing `grx` with `keychain-access-groups` entitlements; lands in M-Prod #8 packaging.

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
- [`0007-ctx-secret-runtime-accessor.md`](../decisions/0007-ctx-secret-runtime-accessor.md) — Phase B Part 1 contract ADR (per-call dispatch + manifest allowlist + trait surface).
- [`graphreact-engine-zig/src/traits.rs`](../../crates/graphreact-engine-zig/src/traits.rs) — `HostSecretCallback` trait + `Engine::host_secret_callback()` default-`None` method.
- [`graphreact-engine-jsc/src/lib.rs`](../../crates/graphreact-engine-jsc/src/lib.rs) — JSC override + process-global `OnceLock<Arc<dyn HostSecretCallback>>` shadow.
- [`graphreact-engine-jsc/tests/ctx_secret.rs`](../../crates/graphreact-engine-jsc/tests/ctx_secret.rs) — Phase B Part 1 JSC integration test (allow + missing + default-deny).
