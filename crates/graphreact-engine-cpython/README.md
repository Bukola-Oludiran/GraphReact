# graphreact-engine-cpython

*CPython engine bridge — wraps `zig/bridge/cpython.zig` and links the system Python framework; ships real eval/resolve/call + Python `useState`/`useEffect`/`useEmit`/`useMessage` hook prelude.*

## Milestone

Ships as part of [M13 — CPython engine bridge — SHIPPED 2026-05-06 (M13 closeout)](../../docs/backend/m13-phases.md). M13 closed all ten phase-3 sub-phases plus the four outstanding-work items; the engine surface here is the substrate the CLI's [`pyruntime.rs`](../../crates/graphreact-cli/src/pyruntime.rs) composition layer mounts `lang="python"` definitions on top of.

## Role in the runtime

This crate sits at the **engine tier**, peer to `graphreact-engine-jsc` and `graphreact-engine-null`. It depends on `graphreact-engine-zig` for the FFI substrate (Zig builds + `extern "C"` plumbing) and for the `Engine` trait it implements, but it does NOT depend on the scheduler or any specific distribution surface — the scheduler / CLI composes engines, not the other way around. The Python interpreter is process-singleton (`Py_Initialize` / `Py_Finalize`); per-component isolation is modelled by legacy sub-interpreters created via `Py_NewInterpreter`. CPython is gated to Python 3.12+ for PEP 684's per-interpreter GIL — the C-extension state-pollution issues on 3.9–3.11 forced this floor at the M13 closeout.

## Key types

- [`CPythonEngine`](src/lib.rs#L331) — the engine struct; owns the process-wide main-interpreter lifetime and a Zig-side state pointer.
- [`impl Engine for CPythonEngine`](src/lib.rs#L596) — the trait implementation: `initialize` / `shutdown` / `create_isolate` / `dispose_isolate` / `eval_module` / `resolve_function` / `call_function` / `invoke_global` / `register_host_callback` / `dispose_host_callback`.
- [`CPythonEngine::invoke_global`](src/lib.rs#L351) — symmetric Rust→Python entry point used by `graphreact-cli`'s `pyruntime.rs` to drive `__augment_ctx`, hook prelude install, and direct body re-renders.
- [`invoke_global_static`](src/lib.rs#L386) — free-function variant for callsites that can't hold a `&mut CPythonEngine` borrow (host-callback trampolines that re-enter the engine).

## Composes with

| Crate | Role |
|---|---|
| [`graphreact-engine-zig`](../graphreact-engine-zig/README.md) | Owns `build.rs` plumbing for the Zig bridge + `extern "C"` symbol surface (`gr_cpython_*`) — this crate links the system Python framework on top — and defines the `Engine` trait + `HostValue` JSON-marshalling type both engines converge on. |

Consumed by **[`graphreact-cli`](../graphreact-cli/README.md)** (the `--runtime cpython` selector, `pyruntime.rs` composition layer, and the `grx cluster daemon` engine-hosting path closed in M14.8.x-followon).

## Running the tests

```sh
cargo test -p graphreact-engine-cpython --release
```

The crate ships real-interpreter tests (every test wraps `CPythonEngine::new` + `initialize`), so a working Python 3.12+ framework on the build host is required — see [m13-phases.md](../../docs/backend/m13-phases.md) for the `pkg-config` probe behaviour and the `python_free_threaded` cfg flag.

## See also

- [M13 — CPython engine bridge milestone narrative](../../docs/backend/m13-phases.md) — full sub-phase log (phase 1 through phase 3i), closeout addenda, and the Python 3.12+ floor decision.
- [M6.6 — engine parity](../../docs/backend/m6.6-engine-parity.md) — the trait-surface convergence story across JSC + CPython + null.
- [M16 debug CPython](../../docs/backend/m16-debug-cpython.md) — observer-side debugging hooks specific to this engine.
- [Decision 0002 — GraphReact is backend-only](../../docs/decisions/0002-graphreact-is-backend-only.md) — why an embedded scripting engine sits inside the runtime.
- [Workspace onboarding](../../docs/onboarding.md) — repo layout + crate-tier map.
- [Workspace architecture](../../docs/architecture.md) — tiered overview (substrate / engine / distribution / observer / cli / foreign).
