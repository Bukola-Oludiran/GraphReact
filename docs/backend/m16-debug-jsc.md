# M16-Backend Phase 9 — JSC per-body source debugging

**Status:** SHIPPED 2026-05-08. Axis B per-body debugger for the JSC engine, mirroring P8's CPython debugpy story.

## TL;DR

`grx run --runtime=jsc --debug=jsc:9229 fixture.grx` (or the same flags on `grx cluster daemon` and `grx run --watch`) marks every JSC `JSGlobalContext` GraphReact mounts as remotely inspectable, starts the system RemoteInspector listener, and the JSContext shows up in **Safari → Develop → [Hostname] → JSContext**. Open Web Inspector and step through the `.grx` JS body source.

Symmetric with P8:

| Engine | Native debug protocol | Native IDE consumer |
|---|---|---|
| CPython (P8) | `debugpy` listener over its own wire | VS Code Python extension / PyCharm |
| **JSC (P9)** | **WebKit `RemoteInspector` (lockdownd discovery on macOS)** | **Safari Web Inspector** |
| JVM (M17 future) | JDWP over TCP | IntelliJ / Eclipse |

No browser is involved in the *protocol path*. Safari is the JSC-native IDE on macOS the way PyCharm is one of the Python-native IDEs. See [`feedback_axis_b_no_browser.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/feedback_axis_b_no_browser.md) for the symmetric framing.

## Platform support

| Platform | Status | Mechanism |
|---|---|---|
| macOS (12+) | **Supported** | Public C SPI (`JSGlobalContextSetRemoteInspectionEnabled` + `JSRemoteInspectorStart`) + Apple lockdownd discovery |
| Linux | Not supported (yet) | Vendored WebKit + RemoteInspector substrate is the M12.3b-phase4 deliverable; until then `--debug=jsc:<port>` returns `EngineError::InvalidIsolate`-class errors at install time on Linux |
| Windows | Not supported | Same as Linux |
| iOS / tvOS | Not targeted | The JSC engine binary is built for macOS targets only |

The macOS-only ceiling matches M12.3a/M12.3b-phase1.5's existing platform stance: the JSC engine itself is macOS-only today via `JavaScriptCore.framework` link; the inspector ceiling moves with it when M12.3b-phase4 lands vendored WebKit.

## Attach procedure (Safari Web Inspector)

1. **Enable Develop menu in Safari.** Safari → Settings → Advanced → check *"Show features for web developers"*. (Older Safari: *"Show Develop menu in menu bar"*.)
2. **Start a `grx` daemon (or `grx run`) with `--debug=jsc:9229`.**
   ```
   grx cluster daemon --runtime=jsc --debug=jsc:9229 --path fixture.grx
   ```
   Stderr prints the standard daemon-ready line; no extra inspector-bound message — lockdownd discovery is silent.
3. **Open Develop menu.** Safari → Develop → *[your hostname]* → *JSContext*. (May appear as `__JSC` or the mount banner's component name; varies by macOS version.)
4. **Set a breakpoint.** Web Inspector opens to the loaded sources. Find your `.grx` JS body source, set a breakpoint inside `def init(ctx)` or any hook handler. Trigger the body (a message arrives, a render happens, a timer fires); the breakpoint hits.
5. **Step / inspect / continue.** Standard Web Inspector controls.

### Why no `--debug-no-wait` analogue for JSC

P8's CPython arm has `--debug-no-wait` to flip `debugpy.wait_for_client()` off for "attach later" workflows. **JSC has no equivalent semantic** — Apple's RemoteInspector doesn't expose a wait-for-client primitive in the public C SPI. Lockdownd discovery is silent and asynchronous: the daemon proceeds past mount immediately whether or not Safari has attached.

If the operator attaches Safari mid-session, breakpoints take effect on the next body invocation. For first-mount breakpoints (`def init(ctx)` analogues), attach Safari before triggering the first message / first render. The cluster daemon's `start_daemon` ordering helps: `mount_with_scheduler` registers definitions but defers the first batch until after `Cluster::start`, so the operator has the boot window between daemon start and first peer-connect to attach.

The CLI accepts `--debug-no-wait` alongside `--debug=jsc:<port>` for syntactic uniformity with the CPython arm; on the JSC code path the flag is a no-op. Documented in `commands.rs::run_run` JSC arm.

### Cross-host attach

debugpy + Safari both live on the same kernel as the daemon. For cross-host debugging the operator opens an SSH tunnel:

```
ssh -L 9229:localhost:9229 user@daemon-host
```

…then attaches a *local* Safari/IDE to `localhost:9229`. Same shape as P8's CPython story; documented as the M-Prod #4 "production debug attach" track.

## What about VS Code's JS debugger / Chrome DevTools / WebStorm?

**Deferred along with V8** ([decision 0001](../decisions/0001-v8-engine-question.md), 2026-05-03 soft-demote).

These IDEs all speak **CDP** (Chrome DevTools Protocol) — V8's native debug protocol that Node.js / Bun standardised on. JSC's native protocol is WebKit's **RemoteInspector**, which is CDP-shaped but not byte-identical. Attaching CDP-shaped clients to JSC requires:

1. A vendored copy of WebKit's `RemoteInspectorServer` (the C++ class that binds a TCP listener and speaks WebKit's wire format).
2. A CDP shim that translates JSC's RemoteInspector envelope ↔ Chrome DevTools Protocol.

That's roughly 600+ lines of WebKit-internal C++ glue (Bun's reference: `bun/src/bun.js/Debugger.zig` + `bun/src/bun.js/bindings/BunDebugger.cpp`). It's real engineering, not gratuitous — but it's also coupled to vendored WebKit (M12.3b-phase4) and to the V8 engine's eventual return (M17 if a paid user asks per [decision 0001]; M-Prod for the CDP-relay infrastructure regardless).

**Forward-compat:** the `--debug=jsc:<port>` parser already accepts a port argument plumbed through the FFI to `gr_jsc_isolate_enable_debugger(handle, port)`. The Zig bridge ignores the port today (lockdownd doesn't bind a TCP listener), but a future M-Prod pass that adds the vendored RemoteInspectorServer can wire the port without re-shaping the CLI surface or the FFI ABI.

## Implementation reference

| Layer | File | Surface |
|---|---|---|
| Zig bridge | [`zig/bridge/jsc.zig`](../../zig/bridge/jsc.zig) | `gr_jsc_isolate_enable_debugger(handle, port)` + `gr_jsc_isolate_inspection_enabled(handle)` exports. `extern "c"` decls for `JSGlobalContextSetRemoteInspectionEnabled` + `JSGlobalContextGetRemoteInspectionEnabled` + `JSRemoteInspectorStart` (symbols exported by macOS framework binary; not in public SDK headers; linker resolves at final link via `-framework JavaScriptCore`). |
| Rust FFI | [`crates/graphreact-engine-jsc/src/ffi.rs`](../../crates/graphreact-engine-jsc/src/ffi.rs) | Mirrors the Zig exports under `extern "C"` block. |
| Rust JscEngine | [`crates/graphreact-engine-jsc/src/lib.rs`](../../crates/graphreact-engine-jsc/src/lib.rs) | `JscEngine::enable_debugger(&mut self, iso, port) -> EngineResult<()>` with `AtomicBool` idempotent guard; `JscEngine::is_inspector_enabled(&self, iso) -> EngineResult<bool>` round-trip helper for the test gate. |
| EngineError | [`crates/graphreact-engine-zig/src/error.rs`](../../crates/graphreact-engine-zig/src/error.rs) | `DebuggerUnsupported { engine, reason }` variant added (P9; reserved for future Linux arm + future engines). |
| CLI install helper | [`crates/graphreact-cli/src/debug/jsc_inspector.rs`](../../crates/graphreact-cli/src/debug/jsc_inspector.rs) | `install_jsc_inspector(engine, iso, port)` thin wrapper over `JscEngine::enable_debugger`; mirrors P8's `install_debugpy` shape. |
| CLI wiring | [`crates/graphreact-cli/src/commands.rs`](../../crates/graphreact-cli/src/commands.rs) | `validate_debug_against_runtime` accepts `DebugEngine::Jsc` (P9 dropped the "not yet wired (P9)" short-circuit); `build_scheduler_host` JSC arm + `run_run` JSC arm install the inspector after mount, before first batch. |
| CLI watch mode | [`crates/graphreact-cli/src/watch_mode.rs`](../../crates/graphreact-cli/src/watch_mode.rs) | `run_jsc_watch_blocking(path, debug)` installs the inspector before first `process_batch_async`. Reload cycles are safe via the `AtomicBool` guard. |

## Test gate

| Layer | Test | Pin |
|---|---|---|
| Engine round-trip | `graphreact-engine-jsc::tests::p9_enable_debugger_flips_inspection_flag` | Fresh JSGlobalContext defaults to `false`; post-install reads back `true` via `JSGlobalContextGetRemoteInspectionEnabled`. |
| Idempotent guard | `graphreact-engine-jsc::tests::p9_enable_debugger_idempotent_via_atomic_bool_guard` | Repeat installs short-circuit; per-context flag stays `true`. |
| Invalid handle | `graphreact-engine-jsc::tests::p9_enable_debugger_invalid_isolate_returns_error` | Garbage handle surfaces `EngineError::InvalidIsolate`. |
| CLI dispatch (positive) | `graphreact-cli::tests::debug_flag_dispatch::debug_jsc_passes_validation_post_p9` | `--debug=jsc:9229 --runtime=jsc` no longer rejected at validation; failure mode on a missing path is now file-IO-class. |
| CLI dispatch (mismatch) | `graphreact-cli::tests::debug_flag_dispatch::debug_jsc_with_runtime_cpython_is_rejected` | Engine-vs-runtime mismatch path unchanged from P8. |
| CLI signature | `graphreact-cli::debug::jsc_inspector::tests::install_jsc_inspector_signature_pinned` | Function pointer coercion pins `(&mut JscEngine, IsolateHandle, u16) -> Result<(), EngineError>`. |

**Manual gate:** Safari Web Inspector attach to a `grx` daemon launched with `--debug=jsc:9229`; breakpoint inside a `.grx` JS body hits on first render. Documented above.

## Footguns

- **Lockdownd discovery is async + silent.** No "inspector listening on PORT" log line, no wait-for-client analogue. Operator must know to look in Safari → Develop. Consider stderr-printing a one-line discovery hint in M-Prod (`grx-jsc-inspector-ready: open Safari → Develop → [hostname] → JSContext`).
- **Process-global RemoteInspector.** `JSRemoteInspectorStart` is singleton-init at the framework level; once started, all JSContexts in the process see Safari Web Inspector targets. The per-context `JSGlobalContextSetRemoteInspectionEnabled` flag controls visibility per-isolate, but the process-wide listener is shared. If a future feature spawns multiple `JscEngine` instances per process, they all share one inspector.
- **JSC has no wait-for-client.** First-mount breakpoints require attaching Safari before the first body fires. The cluster daemon's `start_daemon` ordering (mount → `Cluster::start` → first `process_batch`) gives the operator a window; for `grx run`, the window is between mount and the first `process_batch_async` call (sub-millisecond on most fixtures — open Safari first, then run `grx`).
- **`port` is informational on macOS.** Lockdownd doesn't bind a TCP listener; Safari finds the JSContext by process traversal. The `port` parameter is plumbed for forward-compat with M-Prod's vendored RemoteInspectorServer (TCP-bound) and for symmetry with `--debug=cpython:<port>` (which IS port-bound via debugpy).
- **`--debug-no-wait` is accepted but no-op on JSC.** Documented above; flagged in `commands.rs::run_run` JSC arm with `let _ = debug_wait;`.
- **No CDP wire support on JSC bodies.** VS Code's JS debugger / WebStorm / Chrome DevTools defer with V8 + M-Prod's CDP relay work. See [decision 0001](../decisions/0001-v8-engine-question.md).
- **macOS-only.** Linux + Windows return errors at install time. The JSC engine itself is macOS-only via `JavaScriptCore.framework` link (M12.3b-phase4 will lift via vendored WebKit); the inspector ceiling moves with it.

## Cross-references

- [`m16-debug-cpython.md`](m16-debug-cpython.md) — P8's CPython debugpy story; symmetric.
- [decision 0001](../decisions/0001-v8-engine-question.md) — V8 soft-demote; explains why CDP attach (Chrome DevTools / VS Code JS debugger / WebStorm) defers.
- [decision 0002](../decisions/0002-graphreact-is-backend-only.md) — backend-only architectural commitment; ratatui in P6 is the corollary corrective for Axis A.
- [`m12.3b-webkit-vendoring.md`](m12.3b-webkit-vendoring.md) — vendored WebKit roadmap; unlocks Linux JSC + (eventually) M-Prod's CDP relay infrastructure.
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/feedback_axis_b_no_browser.md` — symmetric Axis B framing rule.
- Bun reference: `bun/src/bun.js/Debugger.zig` + `bun/src/bun.js/bindings/BunDebugger.cpp` — Bun's full vendored-WebKit + CDP relay path (~600+ LOC of WebKit-internal C++ glue). M-Prod scope, NOT M16.
