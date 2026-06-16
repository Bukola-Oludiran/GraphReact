# 0005 — The third runtime engine is Go (cgo + c-shared), not JVM

**Status:** DECIDED — recorded 2026-06-15. Supersedes [ADR-0001](0001-v8-engine-question.md) "M17 JVM as primary first-party deliverable" framing for the third-engine slot and reshapes [ADR-0003](0003-graphreact-bifurcates-into-backend-and-db.md) §"Mixed-runtime systems (M22) | JS + Python + JVM" wording. M-Ingress (ADR-0004) and the substrate-not-MVP discipline both stay valid; this decision sits inside the M17 engine-bridge milestone scope.
**Related:** [Later-milestone strategic plan](/Users/damioludiran/.claude/plans/graphreact-rust-later-milestones.md) §M17 (full phase decomposition + acceptance gates). [m13-phases.md](../backend/m13-phases.md) is the canonical phase template all engine bridges mirror. [m6.6-engine-parity.md](../backend/m6.6-engine-parity.md) defines the parity matrix M17 must close green.

## The decisions (four)

1. **The M17 third-engine slot is Go, not JVM.** Component bodies tagged `lang="go"` (or `lang="golang"`) compile to a Go c-shared artifact that the daemon loads via `libloading` and dispatches against the same `Engine` trait JSC (M12.3) and CPython (M13) implement.

2. **Embedding strategy is cgo + c-shared.** `go build -buildmode=c-shared` produces a `.so` / `.dylib` + a C header; the daemon links it dynamically and calls exported `//export GrxCallback`-style entry points through the existing `Engine::call_function` surface. Single Go runtime per process; per-component isolates are wrapped goroutines with `sync.Mutex` serialization to honor the trait's `&mut self` borrow shape. Sidecar gRPC, TinyGo + WASM, and hybrid c-shared+sidecar paths are explicitly **not** in M17 scope — they are out-of-scope alternatives, not deferred sub-phases. TinyGo + WASM, if ever needed, lands in M18, not M17.

3. **Authoring surface is build-step codegen, not init-time registration.** Go has no runtime `eval` (Python `exec`/JSC `evaluateScript` equivalents do not exist in Go); component-body authoring cannot mirror M13's HOOK_PRELUDE-at-eval-time pattern. The new `crates/graphreact-codegen-go` crate reads each `.grx` file's Go-tagged bodies and emits a sibling `_grx_prelude.go` carrying typed `UseState` / `UseEffect` / `UseEmit` / `UseMessage` / `UseNeighbors` / `UseGraphQuery` Tier 1/2/3 / `UseSpawnChild` / `UseUnmountSelf` / `UseCompose` / `UseAcceptEdge` / `UseEdgeAcceptHandler` Go funcs that delegate into cgo trampolines. The build flow is `grx build` → emit prelude → `go build -buildmode=c-shared`. Authors write plain Go; `init()` registration APIs, struct-tag macros, and `go:generate` annotations are considered and explicitly rejected.

4. **The Java→Go pivot is recorded as a supersession of ADR-0001's JVM framing, not a deletion of it.** ADR-0001's "JVM as primary first-party deliverable" wording remains as historical record of the 2026-05-03 decision; the M17 implementation work proceeds against Go per this ADR. [ADR-0003 line 52](0003-graphreact-bifurcates-into-backend-and-db.md) (`M22 = JS + Python + JVM`) is similarly preserved historically and amended in-place by ADR-0003's Amendments block (2026-06-15).

## Why Go over JVM

The pivot decision compresses into five reasons, ranked by load-bearing-ness:

### License posture is dramatically simpler

OpenJDK ships under GPL with the Classpath Exception (GPL+CPE); GraalVM Native Image ships dual GPLv2-with-CPE / GFTC; GraalVM Truffle reuses Truffle's runtime license. Each path forces a careful read of [`deny.toml`](../../deny.toml) and an addendum that explains why the runtime distribution is not GPL-by-association. Go's standard library is **BSD-3-Clause** — already in `deny.toml`'s permissive-allowlist line 21 — and the common ecosystem (`google.golang.org/grpc` Apache-2.0, `github.com/golang/protobuf` BSD-3-Clause, cgo as a stdlib feature) does not pull in unexpected licenses. The license track collapses from "ADR + deny.toml exception + product-team legal review" to "no allowlist change needed."

### Cross-platform vendoring is bounded

JVM embedding (`libjvm.so` + JNI Invocation API) requires distributing an OpenJDK runtime alongside the daemon: per-arch builds (linux-amd64 / linux-arm64 / macos-amd64 / macos-arm64 / windows-amd64), per-LTS major (JDK 21 LTS floor at minimum), and an Apache 2.0 vs GPL+CPE binary decision (Adoptium/Corretto vs OpenJDK). Go compiles each c-shared artifact at the consumer's build step using the user's own Go toolchain — the daemon ships *no* embedded Go runtime, only the loader. The cross-platform matrix that M12.3b WebKit vendoring took multiple sub-phases to scaffold for JSC reduces to "the user runs `go build -buildmode=c-shared`."

### Debug protocol composes with existing infrastructure

JDWP runs over TCP on a JVM-defined wire; per-engine debug substrate would have grown a third per-language protocol family alongside CDP (JSC) and DAP-via-debugpy (CPython). Delve speaks **DAP** natively on TCP `:2345` — the same Debug Adapter Protocol VS Code uses for the existing JS and Python debug stories. The per-engine debug-flag parser at [crates/graphreact-cli/src/debug/flag.rs](../../crates/graphreact-cli/src/debug/flag.rs) already accepts arbitrary `engine:port` shapes; adding `DebugEngine::Go` with a default port of 2345 is mechanically a one-variant addition + one `commands.rs::validate_debug_against_runtime` arm. No new debugger ecosystem to evaluate.

### Cluster handshake substrate composes verbatim

[crates/graphreact-dist/src/handshake.rs](../../crates/graphreact-dist/src/handshake.rs) is engine-agnostic — `NodeId` / `Creation` / capability bitmap, no `engine_kind` on the wire. The M14 distribution layer does not need to learn about Go runtimes because the M:N goroutine scheduler stays inside the daemon process; cross-host distribution remains a per-`NodeId` concern. M14.11 cross-node remote restart (M-Prod sub-track #4) inherits the same indifference. JVM embedding would have inherited the same indifference too, but only after solving the JNI-init thread-affinity dance per process; Go's cgo runtime initialization is process-global and non-interactive.

### Mixed-runtime story compresses

M22 (per locked Q1) ships JS + Python + Go bodies in the same `.grx` system. The JVM path would have required reasoning about three sub-interpreter / isolate strategies (PyO3 sub-interpreters under PEP 684, JavaScriptCore JSContextGroup per body, JVM ClassLoader hierarchy + thread group per body). The Go runtime is itself process-global once initialized — Go isolation reduces to per-component `sync.Mutex` serialization of goroutine entry, which the `Engine` trait's `&mut self` borrow shape already enforces from the Rust side. The mixed-runtime substrate stays a per-component routing problem, not a per-engine isolation problem.

## What this decision does NOT do

- **Does NOT block the V8 soft-demote from ADR-0001.** Anyone can still implement `graphreact-engine-v8` externally against the existing `Engine` trait. The first-party engine list now reads JSC + CPython + Go.
- **Does NOT preempt M18 server-WASM.** TinyGo + WASM is a deployment-target choice that composes with M18's `graphreact-wasm` crate if a host (Cloudflare Workers / Fastly / Wasmtime / Wasmer / WAMR) demands it. M17 ships native Go, not WASM-Go.
- **Does NOT change M-Prod sub-track #4's M14.11 absorption.** Lifecycle work stays in M-Prod sub-track #4 per locked Q9; the Go engine's process-global lifecycle does not require special-case handling there.
- **Does NOT alter the M23 LSP scope cut.** M23 ships outer-shell completions only at MVP per locked Q10. In-body Go completions live in M23.next alongside swc-driven JS in-body completions and rustpython-parser-driven Python in-body completions.
- **Does NOT commit to a build-tool integration for `grx build`.** The codegen step is a Rust binary path; whether it integrates with `cargo`, `make`, `bazel`, or `mage` at the build-tool layer is a per-consumer choice. The substrate ships the codegen + invocation contract.

## Consequences

### Code substrate changes (M17 phase 1 + 3a substrate)

- New workspace member `crates/graphreact-engine-go` implementing the `Engine` trait against a cgo + c-shared Go runtime. Single Go runtime per process; per-component isolates are wrapped via `sync.Mutex` serialization to match the trait's `&mut self` borrow shape.
- New workspace member `crates/graphreact-codegen-go` emitting `_grx_prelude.go` per `.grx` file and orchestrating the `go build -buildmode=c-shared` step.
- New `ComponentLang::Go` variant in [crates/graphreact-grx-parser/src/ast.rs](../../crates/graphreact-grx-parser/src/ast.rs) with `"go" | "golang"` `from_attr_str` aliases.
- New `EngineKind::Go` + `RuntimeArg::Go` variants + a `goruntime.rs` sibling to `jsruntime.rs` / `pyruntime.rs` in [crates/graphreact-cli/src/](../../crates/graphreact-cli/src/).
- New `DebugEngine::Go` variant + default port `2345` Delve DAP in [crates/graphreact-cli/src/debug/flag.rs](../../crates/graphreact-cli/src/debug/flag.rs), plus a per-engine validation arm in `commands.rs::validate_debug_against_runtime`.
- No change to the `Engine` trait at [crates/graphreact-engine-zig/src/traits.rs](../../crates/graphreact-engine-zig/src/traits.rs) — the 8-method surface is engine-agnostic.
- No change to [`deny.toml`](../../deny.toml) — Go stdlib BSD-3-Clause is already in the allowlist.

### Documentation cross-cuts (this commit + follow-ups)

- ADR-0003 Amendments block flags the `JVM → Go` wording supersession (already applied 2026-06-15).
- ADR-0001 Amendment block flags the M17 JVM → Go pivot (already applied 2026-06-15).
- [docs/backend/README.md](../backend/README.md) milestone table M17 / M22 rows already updated 2026-06-15.
- [docs/db/README.md](../db/README.md) M22 row already updated.
- [Later-milestone strategic plan](/Users/damioludiran/.claude/plans/graphreact-rust-later-milestones.md) §M17 + §M22 is the authoritative scope.

### Reversibility

This decision is **reversible at substantial cost** before M17 phase 2.5 (cgo callback trampoline) closes; **irreversible in practice** afterwards. The reversibility boundaries:

- **Before M17 phase 1 ships:** swapping back to JVM costs ADR text + `graphreact-engine-go` skeleton (no behavioral code yet) + parser/CLI variant rename. ~1 day.
- **Between M17 phase 1 and phase 2.5:** swap costs the Engine trait impl (no callbacks yet). ~3 days.
- **After M17 phase 2.5:** swap costs the cgo trampoline + the build-step codegen contract + any deployed `_grx_prelude.go` reference in user systems. Effectively a new milestone.
- **After M17 phase 3b (build-step codegen ships):** user `.grx` systems with `lang="go"` bodies are in the wild; the codegen contract becomes part of the public substrate. A second engine pivot would require either (a) running both engines side-by-side for one major-version cycle or (b) a hard migration with breaking-change notice.

The current plan is to commit M17 to Go without an explicit reversibility window — the substrate-not-MVP discipline expects a complete primitive set to ship behind a stable contract, not a feature-flagged trial.

## Cross-references

- [Later-milestone strategic plan](/Users/damioludiran/.claude/plans/graphreact-rust-later-milestones.md) — §M17 full phase decomposition; §M22 mixed-runtime relaxation; §"Locked decisions" Q1/Q2/Q3 table.
- [ADR-0001 — V8 soft-demote](0001-v8-engine-question.md) — the "M17 JVM as primary first-party deliverable" wording in §Decision and §Reasoning is the historical record this ADR supersedes; the Amendment block at the top of ADR-0001 (2026-06-15) flags this.
- [ADR-0003 — Backend/DB bifurcation](0003-graphreact-bifurcates-into-backend-and-db.md) — line 52 "Mixed-runtime systems (M22) | JS + Python + JVM" is amended by ADR-0003's own Amendments block (2026-06-15).
- [m13-phases.md](../backend/m13-phases.md) — THE template all engine-bridge phase decompositions mirror; M17 phases 1+2 / 2.5 / 3a-h / 3i mirror M13 row-for-row.
- [m6.6-engine-parity.md](../backend/m6.6-engine-parity.md) — the parity matrix M17's Go column closes green at acceptance.
- [m12.3b-webkit-vendoring.md](../backend/m12.3b-webkit-vendoring.md) — the cross-platform Engine-substrate vendoring precedent M17 deliberately does NOT mirror (vendoring lives in the user's Go toolchain, not in `vendor/` here).
- [crates/graphreact-engine-zig/src/traits.rs](../../crates/graphreact-engine-zig/src/traits.rs) — the 8-method `Engine` trait, unchanged by this decision.
- [crates/graphreact-grx-parser/src/ast.rs](../../crates/graphreact-grx-parser/src/ast.rs) — `ComponentLang` enum extended with `Go` variant in M17 phase 3a.
- [crates/graphreact-cli/src/engine_select.rs](../../crates/graphreact-cli/src/engine_select.rs) — `EngineKind` enum extended with `Go` variant in M17 phase 1.
- [crates/graphreact-cli/src/debug/flag.rs](../../crates/graphreact-cli/src/debug/flag.rs) — `DebugEngine` enum extended with `Go` variant + Delve DAP default port `2345`.
- [deny.toml](../../deny.toml) — no allowlist change required; Go stdlib BSD-3-Clause already in line 21.
