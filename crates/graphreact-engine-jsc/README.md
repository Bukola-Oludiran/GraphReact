# graphreact-engine-jsc

_JSC (JavaScriptCore) engine bridge — wraps `zig/bridge/jsc.zig`, links the macOS system JavaScriptCore framework, ships real bidirectional JS↔Rust callbacks and hook semantics._

## Milestone

Ships across two milestone slices: [M12.3a — JSC bridge SHIPPED](../../docs/backend/m6.6-engine-parity.md) brought the bridge from stub through phase3 hook wiring (every `useState` / `useEffect` / `useMessage` / `useGraphQuery` row in the M6.6 parity matrix lights up under JSC on macOS), and [M12.3b — WebKit vendoring (in progress)](../../docs/backend/m12.3b-webkit-vendoring.md) extends the bridge to Linux/Windows via vendored WebKit. macOS uses the system `JavaScriptCore.framework` at `/System/Library/Frameworks/`; phases 3b-phase1 + 3b-phase1.5 (the extern-C `drainMicrotasks` C++ shim) shipped the real `useEffectAsync` path including genuine async I/O via deferred host-callback resolution.

## Role in the runtime

This crate sits in the **engine tier**: it implements `graphreact_engine_zig::Engine` over Apple's JavaScriptCore C ABI, exposing the same trait surface as `graphreact-engine-cpython` (Python) and `graphreact-engine-null` (no-op). The actual FFI boundary is owned by `zig/bridge/jsc.zig` (built via `cargo xtask zig-build`) and re-exported through `graphreact-engine-zig`'s shared types; this crate is the safe Rust facade — isolate lifecycle, JSON marshalling, a process-global `CALLBACK_REGISTRY` keyed by monotonically-assigned `CallbackId`, and the C trampoline that re-enters Rust from inside a `JSObjectCallAsFunction` call. The reactive runtime (`graphreact-core`) doesn't know JSC exists; the CLI picks an engine at mount time via `SchedulerHost::Jsc(JsRuntime)`.

## Key types

- [`JscEngine`](src/lib.rs#L72) — the engine struct: `initialized` guard, `Mutex<State>` over the isolate table, idempotent `inspector_installed` flag for M16-Backend P9 debugger attach.
- [`impl Engine for JscEngine`](src/lib.rs#L365) — the `graphreact-engine-zig::Engine` trait impl: `initialize`, `create_isolate`, `eval`, `resolve`, `call`, `register_callback`, `invoke_global`, `drain_microtasks`, `shutdown`.
- `drain_microtasks_static` / `invoke_global_static` — free-function variants for callsites that hold an `Rc<RefCell<JscEngine>>` and can't re-borrow without conflicting (load-bearing for the `useEdgeAcceptHandler` re-entrancy path).
- `CALLBACK_REGISTRY` — process-global `RwLock<FxHashMap<u64, SharedCallback>>` mapping Zig-assigned callback ids to `Arc<dyn Fn>` closures; `Arc` (not `Box`) so the JSC trampoline can clone-out under the lock and invoke past release.

## Composes with

| Crate | Direction | Role |
|---|---|---|
| [`graphreact-engine-zig`](../graphreact-engine-zig/) | depends on | Shared `Engine` trait + `HostValue` / `IsolateHandle` / `EngineError` types; the Zig static lib that owns the JSC C ABI surface. |
| [`graphreact-cli`](../graphreact-cli/) | depended on by | `grx run --runtime jsc` and engine-hosted `grx cluster daemon` mount this engine via `SchedulerHost::Jsc`. |

## Running the tests

```sh
cargo test -p graphreact-engine-jsc --release
```

Roughly 20 unit tests inside `src/lib.rs` plus the M12.3a-phase3c 1000-cycle RSS stress acceptance gate (uses `libc` as a dev-dep for RSS measurement). macOS-only on the host side; non-macOS targets compile to a no-op stub until M12.3b-phase4 wires vendored WebKit.

## See also

- [M6.6 — TS feature parity audit, per-engine sign-off](../../docs/backend/m6.6-engine-parity.md) — the JSC column in the hook-by-hook parity matrix.
- [M12.3b — WebKit vendoring](../../docs/backend/m12.3b-webkit-vendoring.md) — Linux/Windows path; xtask orchestration; variant-(b) decision.
- [M16 — JSC debug surface](../../docs/backend/m16-debug-jsc.md) — `JSRemoteInspectorStart` integration + Safari Web Inspector attach.
- [Decision 0001 — V8 engine question](../../docs/decisions/0001-v8-engine-question.md) — why JSC over V8.
- [Workspace onboarding](../../docs/onboarding.md)
- [Architecture overview](../../docs/architecture.md)
