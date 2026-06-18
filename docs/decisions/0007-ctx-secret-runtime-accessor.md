# 0007 — ctx.secret(name) is a per-call host-callback over an Engine-trait surface; manifest-declared needs_secret allowlist gates reads at the host-callback boundary; default-deny on undeclared names; SecretStore impls are Sync so the engine worker thread can dispatch; CPython + Go wiring + audit-of-secret-reads + SIGHUP re-read defer to Phase B Part 2

**Status:** DECIDED 2026-06-18. M-Prod sub-track #3 Phase B Part 1 SHIPPED 2026-06-18 (JSC slice).
**Related:** ADR-0006 (audit chain substrate, [0006-audit-chain-substrate.md](0006-audit-chain-substrate.md)) — the `graphreact::observer::audit` target convention secret-read auditing will eventually ride; ADR-0004 (substrate-not-MVP discipline, [0004-ingress-as-substrate-primitive.md](0004-ingress-as-substrate-primitive.md)). Cross-references: [m-prod-secrets.md](../backend/m-prod-secrets.md) and [m-prod.md](../backend/m-prod.md).

## The decisions (five)

1. **Per-call host-callback dispatch.** Every `ctx.secret("FOO")` call crosses into Rust and re-dispatches against the registered `SecretStore`. NO caching at the engine layer. SIGHUP-driven rotation (Phase B Part 2) then just swaps the registered `Arc<dyn SecretStore>` — bodies see the new value on the next call, no per-isolate cache invalidation needed.
2. **Manifest-declared `needs_secret = ["NAME1", "NAME2"]` allowlist, default-deny.** Bodies must declare the secrets they read in the permission manifest they return per render. The host-callback consults the **last-committed manifest for the active node** (`Scheduler::manifests`) — the first render therefore has an empty allowlist → default-deny. Non-declared names resolve to `null` (JS) / `None` (Py) / zero-value (Go); calls never throw. Mirrors `accepts_external` default-deny shape.
3. **`SecretStore: Send + Sync` is the thread-safety contract.** The host-callback executes on whichever worker the engine schedules; the existing `graphreact_secrets::SecretStore` trait already pins `Send + Sync` at the trait level. No new lock at the host-callback boundary.
4. **Engine trait surface gains `fn host_secret_callback(&self) -> Option<&dyn HostSecretCallback>` with a `None` default.** Engines that don't yet wire secrets (cpython, go, echo, null, zig) compile unchanged; JSC overrides to return `Some(&...)`. This is the contract surface — the actual wiring on the JS side goes through the existing `register_callback("host_secret", …)` plumbing in `graphreact-cli/src/jsruntime.rs` for the production daemon path; the trait method is the gating predicate the orchestrator queries to decide whether to install the JS-side `ctx.secret` shim.
5. **`SecretValue` traverses the host-callback boundary as a UTF-8 string in `HostValue::String`** for the JSC slice. Non-UTF-8 secrets resolve to `null` and emit a `tracing::warn!(target: "graphreact::secrets::audit")` warning. Phase B Part 2 will widen to a typed sealed-wrapper once CPython + Go cross the same boundary.

## Why per-call (not cached)

Rotation correctness > microbench. Phase B Part 2's SIGHUP path swaps the `Arc<dyn SecretStore>` atomically; a cache would need a separate invalidation channel. The host-callback fast path is one `RwLock::read()` on the store handle + one `SecretStore::get()` (env-var read = O(1); keyring read is the expensive case but rare).

The trio of expected per-event costs:

- **EnvStore::get** — single `std::env::var` lookup; sub-microsecond on the libc hot path.
- **KeyringStore::get** — single platform syscall (Keychain query / Secret Service dbus round-trip / wincred lookup). Tens-of-µs to single-ms range; rare in steady state (bodies read once per render).
- **Caller-side overhead** — JSON serialisation of the `HostValue::String` reply across the JSC bridge dominates the cost at sub-microsecond per call; the lookup itself isn't the bottleneck.

Phase B Part 2 may revisit if `auth_success`-class fan-out moves the read rate above current projections, but the substrate doesn't preclude a cache — it just doesn't ship one in Phase B Part 1.

## Why manifest-declared allowlist (not implicit-allow-all)

Principle of least knowledge + auditability. The same component author who writes `needs_secret = ["DATABASE_URL"]` makes the assertion explicit at the manifest layer; static review of `.grx` files reveals every secret a body might read without grepping bodies. Default-deny matches the `accepts_external` shape Phase A locked.

The alternative — implicit allow-all gated only by the operator-side `SecretStore` namespace — collapses two trust boundaries (the operator says "this secret exists" vs the body says "this secret is mine to read") into one. The manifest layer is the right place to enforce the body-side contract because it's the layer the operator can review at deploy time.

## Why default-deny via `null` (not throw)

Bodies are user code that often run in event handlers (`useMessage`); a thrown exception on a misdeclared secret name would crash the handler. Returning `null` lets bodies short-circuit with a normal `if (!s) return;` — the same shape JS authors expect from `Map.get`. The CPython and Go slices Phase B Part 2 ships use the same logic (`None` / zero-value).

A noisy mode is still available: bodies that prefer fail-loud can wrap their own `ctx.secret(name) ?? throw …` — the substrate doesn't preclude it, but the default sticks with silent-deny so misdeclared bodies degrade gracefully instead of crashing.

## Why JSC first

The Zig prelude's `__augmentCtx` (zig/bridge/jsc.zig:433) is the canonical install point for method-style ctx APIs (`ctx.spawnChild`, `ctx.compose`, `ctx.unmount`, `ctx.acceptEdge` all live there); adding `ctx.secret` rides the same surface in one line of prelude + one `register_callback("host_secret", …)` site. CPython needs the same wiring through `__augment_ctx` (Python source-wrap; see `m13-phases.md:526`) and Go needs a method on the codegen `Ctx` struct — they're each one PR but each PR also needs its own engine-specific test, so they ship Phase B Part 2.

## Why `SecretValue` rides the host-callback boundary as UTF-8 string

The `HostValue` ABI in `graphreact-engine-zig` already routes JSON-serialisable shapes (string, number, bool, null, object, array) across the FFI. The simplest JSC slice (Phase B Part 1) marshals secrets as `HostValue::String` so the JS side sees them as a plain JS `string`. The non-UTF-8 path (binary-only secrets like raw signing keys) resolves to `null` for now + emits a `tracing::warn!(target: "graphreact::secrets::audit")` warning so operators see the gap; Phase B Part 2 widens the boundary to a typed sealed-wrapper after CPython + Go cross the same surface and the right typed-bytes representation is clear (likely `HostValue::Bytes(Vec<u8>)` once the engine ABI carries it).

The `SecretValue` does NOT travel as-is across the FFI — the `Zeroize` + `ZeroizeOnDrop` derive only protects the original `Vec<u8>`; once the bytes are copied into a JS `string`, the JS engine's heap holds an independent copy outside the Rust zeroize regime. This is a known gap; Phase B Part 2 will revisit when widening to typed-bytes (engines that hold the bytes can hand the heap back to Rust on body completion).

## Reversibility

Stop-energy is low:

- The trait method is `None`-default; removing it later is a strict-additive rollback.
- The `needs_secret` manifest field is `Option<Vec<String>>` — `None` matches Phase A pre-existing semantics bit-for-bit. Existing call-sites that don't set it (e.g. `PermissionManifest::new`) keep producing manifests with no declared secrets.
- The `__augmentCtx` line in `jsc.zig` is one line; removing it removes the JS-side surface without touching any other engine wiring.
- The `host_secret` callback name is opt-in via `register_callback` — engines that don't register it return `null` from `ctx.secret(name)` via the existing `if (typeof host_secret === 'undefined') return null;` guard in the prelude. Removing the registration site removes the surface.

## Substrate-not-MVP receipt — what we deliberately did NOT defer

Five Phase B Part 1 scope decisions were load-bearing:

- **ADR.** Without an ADR locking the contract (per-call dispatch, manifest allowlist, default-deny, Sync requirement, UTF-8 string marshalling), CPython + Go can't ride the same shape in Part 2 without re-litigating it.
- **Engine trait surface change.** Without `Engine::host_secret_callback()`, the runtime orchestrator can't query whether an engine supports secret reads (the gating predicate for whether to install the JS-side shim).
- **JSC `__augmentCtx` line + `host_secret` BoxCallback wiring.** Without it, the JS-side `ctx.secret(name)` API doesn't exist; bodies can declare `needs_secret` but no body can read.
- **`PermissionManifest::needs_secret` field.** Without it, the allowlist contract isn't enforceable at the boundary; Phase B Part 2's production-side host-callback factory (reading `Scheduler::manifests` by `current_node_id()`) has nowhere to look.
- **JSC integration test.** Without an executable proof of allowlist-allow + default-deny over a real JSC isolate, the contract shape is undisturbed but unverified.

Explicitly deferred to Phase B Part 2:

- **CPython `__augment_ctx` wiring.** Same shape as JSC but with the Python-side source-wrap pattern; each engine needs its own test.
- **Go codegen prelude `Ctx.Secret(name)` method.** Same shape via the generated Go struct.
- **Audit-of-secret-reads** (`secret_read_allow` / `secret_read_deny` events). Waits on #1's audit-chain sink Phase B landing so the events ride the same hash-chained log instead of just `tracing::info!`.
- **SIGHUP-driven re-read** (atomic swap of the `Arc<dyn SecretStore>` on signal). Waits on #2's atomic-swap pattern shipping for a non-TLS resource.
- **Cached-vs-uncached perf knob.** No measured load yet to justify a cache; revisit when `auth_success`-class fan-out moves the read rate above current projections.
- **Production `host_secret` factory.** The JSC integration test stands in with an inline `TestSecretSource::read` that enforces the allowlist. The production-side wiring (in `graphreact-cli/src/jsruntime.rs`, reading `Scheduler::manifests` by `current_node_id()` + checking `manifest.needs_secret`) ships Phase B Part 2.

## Cross-track contracts (locked here, executed elsewhere)

| Track | Contract |
|---|---|
| #1 audit-chain | Phase B Part 2 secret-read audit events `secret_read_allow` / `secret_read_deny` ride the same `graphreact::observer::audit` target + `AuditSink::append` fan; same precedence-ladder shape as `auth_success`. The `name` is logged; the value NEVER is. |
| #2 TLS rotation | The atomic-swap `Arc<ArcSwap<Arc<T>>>` pattern is the SIGHUP-rotation idiom secrets will reuse Phase B Part 2 (swap `Arc<dyn SecretStore>` on SIGHUP). |
| #5 observability | `secret_read_failures_total` counter joins `MetricsInner` in Phase B Part 2 alongside the audit-chain's `audit_sink_write_failures_total`. |

## Open questions tracked (not blockers)

- **`&self` return type may need widening for CPython + Go.** ADR-0007's `host_secret_callback(&self) -> Option<&dyn HostSecretCallback>` returns a borrow tied to `self`. The JSC slice satisfies this via a process-global `OnceLock<Arc<dyn HostSecretCallback>>` shadow (production model: one `SecretStore` per process, consistent with Phase A's `EnvStore` / `KeyringStore` semantics). CPython / Go may need a different lifetime shape — Phase B Part 2 will revisit; if a widening is needed it's a strict-additive trait change (the JSC default keeps returning the same shape).
- **Binary-only secrets.** Phase B Part 1 resolves non-UTF-8 to `null`. Phase B Part 2 will widen the boundary once the right typed-bytes representation on the engine ABI is clear.
- **Per-isolate vs process-global allowlist scope.** The Phase B Part 1 production factory will read `Scheduler::manifests` by `current_node_id()`. This implicitly scopes the allowlist per-node; future composite-component patterns may need the allowlist to extend down the tree. Track when the use case shows up.
