# graphreact-engine-echo

_Reference echo engine — validates the Rust↔native FFI surface end-to-end via a C/Zig bridge without requiring a real JS engine linked._

## Milestone

Ships as part of the **M12 engine-substrate** trio that pins the Rust↔native ABI: the trait + handle types in [`graphreact-engine-zig`](../graphreact-engine-zig/), this echo bridge, and the JSC bridge whose vendoring story is tracked in [M12.3b — WebKit vendoring](../../docs/backend/m12.3b-webkit-vendoring.md). Engine-parity acceptance (Null / Echo / JSC / CPython behaving identically under the same scheduler) is documented in [M6.6 — engine parity](../../docs/backend/m6.6-engine-parity.md).

## Role in the runtime

`graphreact-engine-echo` sits in the **engine tier** alongside `graphreact-engine-null`, `graphreact-engine-jsc`, and `graphreact-engine-cpython`. It implements the `Engine` trait from [`graphreact-engine-zig`](../graphreact-engine-zig/) against a hand-written C implementation (`c/echo.c`, compiled by `build.rs`); a parity Zig source lives at `zig/bridge/echo.zig`. Its job is **not** to execute JavaScript — its job is to prove that the FFI marshalling round-trip (handle allocation, JSON arg ser/de, callback trampolines, length-cap retry on `GR_ERR_BUFFER_TOO_SMALL`, process-global init/shutdown) is correct independently of whichever real engine is linked. When M12.3 lights up JSC and M17 brings V8, both inherit this ABI; if the echo engine's stress suite passes but a real engine fails, the bug is in the engine adapter, not the boundary contract.

## Key types

- **`EchoEngine`** (`src/lib.rs`) — owns Rust-side callback storage (`FxHashMap<u64, BoxCallback>` behind a `parking_lot::Mutex`) plus an `AtomicBool` init flag; the native side carries no Rust closures.
- **`impl Engine for EchoEngine`** (`src/lib.rs`) — full implementation of `initialize` / `shutdown` / `create_isolate` / `dispose_isolate` / `eval_module` / `resolve_function` / `register_callback` / `call_function` / `drain_microtasks` / `engine_name` / `engine_version` against the `gr_*` C ABI.
- **`echo_trampoline`** (`src/lib.rs`) — `extern "C"` trampoline matching the `gr_host_callback_fn` C signature; writes `null` back through the caller's buffer so guest-initiated callbacks complete without re-entering Rust closures.
- **`invoke_callback`** (`src/lib.rs`) — direct Rust-side callback invocation path used by tests to verify the round-trip when consumers need real closure execution (the trampoline path is the fallback used by `call_function`'s wrap-and-return semantics).
- **`call_with_buffer`** (`src/lib.rs`) — shared helper for ABI calls that write into a caller-provided buffer; retries on `GR_ERR_BUFFER_TOO_SMALL` up to a 1 MiB cap (`RESULT_BUFFER_MAX`).

## Composes with

| Crate | Direction | Role |
|---|---|---|
| [`graphreact-engine-zig`](../graphreact-engine-zig/) | depends on | Provides the `Engine` trait + `HostValue` / `IsolateHandle` / `ModuleHandle` / `FunctionHandle` / `CallbackId` / `BoxCallback` / `EngineError` types this crate implements against. |
| [`graphreact-cli`](../graphreact-cli/) | depended on by | The `grx` binary can mount the echo engine as a runtime option for FFI smoke-testing without dragging in JSC or CPython. |

## Running the tests

```sh
cargo test -p graphreact-engine-echo --release
```

13 in-crate tests covering init/shutdown, isolate create/dispose (including a 500-iteration leak-check via `live_isolates()`), module registration + function resolution, the `call_function` round-trip (including a 200-element `long_args` payload), duplicate-callback rejection, unknown-callback errors, microtask drain, invalid-isolate dispose, name/version surfacing, and the process-global serialisation via `engine_lock()` (the same `JSContextGroup`-style one-VM-per-process shape M12.3 inherits).

## See also

- [M12.3b — WebKit vendoring](../../docs/backend/m12.3b-webkit-vendoring.md) — JSC sister bridge built on top of this same ABI.
- [M6.6 — engine parity](../../docs/backend/m6.6-engine-parity.md) — Null/Echo/JSC/CPython behaving identically under one scheduler.
- [Decision 0001 — V8 engine question](../../docs/decisions/0001-v8-engine-question.md) — why the engine layer is trait-based with multiple swappable backends.
- [Workspace onboarding](../../docs/onboarding.md)
- [Architecture overview](../../docs/architecture.md)
