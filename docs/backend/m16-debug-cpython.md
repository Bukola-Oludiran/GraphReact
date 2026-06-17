# M16-Backend Phase 8 — CPython per-body source debugging via debugpy

**Status:** SHIPPED 2026-05-08 (P11 closeout doc; debugpy substrate shipped P8 2026-05-08).

## TL;DR

`grx run --runtime=cpython --debug=cpython:5678 fixture.grx` (or the same flags on `grx cluster daemon` and `grx run --watch`) installs `debugpy` on the freshly-mounted CPython isolate via the existing `engine.eval_module(iso, "__grx_debugpy__", &prelude)` FFI surface. Default `wait_for_client=true` blocks until your IDE attaches, so breakpoints inside `def init(ctx)` actually fire on first mount. Then attach **VS Code Python extension** (`Python: Remote Attach` configuration pointing at `localhost:5678`) or **PyCharm** (`Run → Attach to Process → debugpy`) and step through the `.grx` Python body source.

Symmetric Axis B framing — engine-native protocol consumed by the engine-native IDE; **no browsers**:

| Engine | Native debug protocol | Native IDE consumer |
|---|---|---|
| **CPython (P8)** | **debugpy listener** | **VS Code Python extension / PyCharm** |
| JSC (P9) | WebKit RemoteInspector (lockdownd) | Safari Web Inspector |
| JVM (M17 future) | JDWP over TCP | IntelliJ / Eclipse |

See [`feedback_axis_b_no_browser.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/feedback_axis_b_no_browser.md) for the symmetric framing rule.

## Platform support

| Platform | Status | Mechanism |
|---|---|---|
| macOS (12+) | **Supported** | CPython engine is macOS-only at M13; debugpy attach works via `engine.eval_module` |
| Linux | Not supported (yet) | Same as the CPython engine ceiling; M13-followon track lifts both. |
| Windows | Not supported | Same. |

## Attach procedure (VS Code Python extension)

1. **Install `debugpy` in the engine's Python interpreter.**
   ```sh
   $(grx --runtime=cpython python-path) -m pip install debugpy
   ```
   debugpy is operator-managed because it ships as a PyPI package; the CPython engine doesn't bundle it. ImportError on first eval surfaces through the engine error chain; the CLI maps it to `CommandError::ClusterArg("debug install failed: <ImportError>")`.

2. **Start the daemon (or `grx run`) with `--debug=cpython:5678`.**
   ```sh
   grx run --runtime=cpython --debug=cpython:5678 fixture.grx
   ```
   Stderr prints the standard mount banner; debugpy listens silently (no extra log line — debugpy doesn't print one). If `wait_for_client=true` (default), the daemon blocks at `debugpy.wait_for_client()` until step 4.

3. **Configure VS Code's `launch.json`:**
   ```json
   {
     "version": "0.2.0",
     "configurations": [
       {
         "name": "Python: Remote Attach to grx",
         "type": "python",
         "request": "attach",
         "connect": { "host": "127.0.0.1", "port": 5678 }
       }
     ]
   }
   ```

4. **Press F5 in VS Code** (or click the green play button in the Run panel). debugpy accepts the connection; `wait_for_client()` returns; the daemon proceeds with first `process_batch`. Set a breakpoint inside `def init(ctx)` or any hook handler before pressing F5 — first render hits it.

5. **Step / inspect / continue.** Standard VS Code debugger controls. CPython sub-interpreters show as separate threads in the debugger (per-process model — see below).

### PyCharm

1. **Run → Attach to Process → debugpy** (or use a remote-debug Run Configuration pointing at `localhost:5678`).
2. Same launch sequence as VS Code.

### `--debug-no-wait`

Flips `wait_for_client=false` for "attach later" workflows. Daemon proceeds past mount immediately; the operator runs `debugpy.connect()` mid-session OR attaches and triggers a deliberate body re-render. Useful for production debugging where waiting at mount would block the cluster.

clap rejects `--debug-no-wait` standalone (without `--debug`): `the following required arguments were not provided: --debug=<DEBUG>`.

## Per-process model

`debugpy.listen` binds a TCP socket at the C level — process-global, NOT per-isolate. CPython sub-interpreters all see the same listener once it's installed; subsequent isolates show as separate threads in the debugger.

**Idempotent guard via Python `__main__.__GRX_DEBUGPY_INSTALLED`:**
```python
import sys as _grx_sys
_grx_main = _grx_sys.modules.get('__main__')
if _grx_main is None:
    import __main__ as _grx_main
if not getattr(_grx_main, '__GRX_DEBUGPY_INSTALLED', False):
    import debugpy as _grx_debugpy
    _grx_debugpy.listen(('127.0.0.1', 5678))
    _grx_debugpy.wait_for_client()  # if wait=true
    setattr(_grx_main, '__GRX_DEBUGPY_INSTALLED', True)
```

Repeat installs are no-ops. The CLI calls `install_debugpy` ONCE at first PyRuntime mount.

## Loopback bind only

debugpy traffic carries breakpoint state + variable inspection that's effectively process-internal. P8 ships **loopback-only** by design; the prelude hardcodes `('127.0.0.1', port)`. Cross-host attach uses SSH tunnel:

```sh
ssh -L 5678:localhost:5678 user@daemon-host
```

…then attach a *local* VS Code/PyCharm to `localhost:5678`. Same shape as P9's JSC story; documented as M-Prod #4 "production debug attach" track for any future remote-bind variant with auth/TLS.

## Mount ordering (load-bearing for first-mount breakpoints)

Without `--debug`, `mount_python_definitions` mounts + runs the first `process_batch` synchronously — bodies fire BEFORE the operator can attach.

With `--debug=cpython:<port>`, the CLI switches to `mount_with_scheduler` which registers definitions but defers the first batch:

```
mount_with_scheduler(path)
    → install_debugpy(engine, iso, port, wait=true)
        → debugpy.listen + debugpy.wait_for_client (blocks)
            → operator attaches, debugger sets breakpoints
                → wait_for_client returns
                    → process_batch_async (first render fires; breakpoints hit)
```

In `start_daemon`, the same ordering: `build_scheduler_host` mounts via `mount_with_scheduler` (when debug is set), then `start_daemon` installs debugpy AFTER `Cluster::start` (so the daemon binds its TLS listener + handshakes peers BEFORE blocking on `wait_for_client`). The first `process_batch` fires AFTER the operator attaches.

## Implementation reference

| Layer | File | Surface |
|---|---|---|
| CLI flag parser | [`crates/graphreact-cli/src/debug/flag.rs`](../../crates/graphreact-cli/src/debug/flag.rs) | `DebugFlag { engine: DebugEngine, port: u16 }` + `DebugEngine::{Cpython, Jsc}` enum + `FromStr` for `engine:port` format. Default ports `cpython→5678` (debugpy default), `jsc→9229` (Bun convention). |
| Install helper | [`crates/graphreact-cli/src/debug/cpython_debugpy.rs`](../../crates/graphreact-cli/src/debug/cpython_debugpy.rs) | `build_debugpy_prelude(port, wait) -> String` constructs the idempotent Python prelude. `install_debugpy(engine, iso, port, wait)` calls `engine.eval_module(iso, "__grx_debugpy__", &prelude)`. macOS-only via `#[cfg(target_os = "macos")]` (matches CPython engine ceiling). |
| Module index | [`crates/graphreact-cli/src/debug/mod.rs`](../../crates/graphreact-cli/src/debug/mod.rs) | Re-exports `DebugFlag` + `DebugEngine` + `DebugFlagError`. |
| CLI flag wiring | [`crates/graphreact-cli/src/cli.rs`](../../crates/graphreact-cli/src/cli.rs) | `--debug=<engine>:<port>` + `--debug-no-wait` on `Run` and `cluster daemon`. clap `requires = "debug"` on `--debug-no-wait`. |
| Dispatch validation | [`crates/graphreact-cli/src/commands.rs::validate_debug_against_runtime`](../../crates/graphreact-cli/src/commands.rs) | Rejects engine-vs-runtime mismatches at dispatch (e.g. `--debug=cpython:5678 --runtime=jsc` → CommandError::ClusterArg). |
| Daemon path | [`crates/graphreact-cli/src/commands.rs::start_daemon`](../../crates/graphreact-cli/src/commands.rs) | Installs debugpy AFTER `Cluster::start` + BEFORE first `process_batch`. |
| `grx run` path | [`crates/graphreact-cli/src/commands.rs::run_run`](../../crates/graphreact-cli/src/commands.rs) | Same ordering inside the `LocalSet` block. |
| `grx run --watch` path | [`crates/graphreact-cli/src/watch_mode.rs::run_cpython_watch_blocking`](../../crates/graphreact-cli/src/watch_mode.rs) | Same ordering inside the watch driver's `LocalSet`. Reload cycles re-use the same listener (idempotent guard). |

## Test gate

| Layer | Test | Pin |
|---|---|---|
| Prelude shape | `crates/graphreact-cli/src/debug/cpython_debugpy.rs::tests::*` | 5 unit tests covering wait/no-wait paths + loopback-only bind + idempotent guard + prelude size cap. |
| Flag parser | `crates/graphreact-cli/src/debug/flag.rs::tests::*` | 7 unit tests (valid/invalid/missing-colon/unknown-engine/bad-port/overflow/default-ports/Display). |
| Dispatch validation | `crates/graphreact-cli/src/commands.rs::debug_validation_tests::*` | 5 unit tests for `validate_debug_against_runtime`. |
| CLI dispatch (assert_cmd) | `crates/graphreact-cli/tests/debug_flag_dispatch.rs` | `debug_cpython_with_runtime_null_is_rejected` + `debug_with_unknown_engine_rejected_at_parse` + `debug_no_wait_requires_debug_flag` + (P9-flipped) `debug_jsc_passes_validation_post_p9`. |

**Manual gate:** VS Code Python extension attach → breakpoint inside `def init(ctx)` hits on first mount. Documented above with `launch.json` snippet.

## Footguns

- **debugpy not bundled.** Operators run `pip install debugpy` in the engine's Python interpreter. ImportError surfaces cleanly through the engine chain. M-Prod doc could ship a "first-time setup" troubleshooting section.
- **`wait_for_client` default blocks at mount.** `--debug-no-wait` flips it for production-attach scenarios. Document operationally.
- **Per-process model.** Multiple PyRuntime instances in the same process share one listener. If a future feature spawns multiple PyRuntimes per process (M22 mixed-runtime?), the guard suppresses re-install and the operator can only attach to the first listener.
- **Loopback bind only.** Cross-host attach via SSH tunnel. Documented above. M-Prod could add `--debug-bind=0.0.0.0:<port>` + auth/TLS.
- ~~**Conditional mount-path split.**~~ **SHIPPED via Cleanup-Pass `m16-mount-path-consolidation` (2026-06-17).** `commands.rs::build_scheduler_host` Cpython arm now always uses `mount_with_scheduler` regardless of `--debug` state. The dropped sync first-batch from `mount_python_definitions` was redundant for the dry-mount report path because `process_batch_async` runs anyway. `mount_python_definitions` remains as a test convenience wrapper (mount + sync batch) for the 3 in-crate tests that depend on the inline-batch shape; production code is unified.
- **Overhead test deferred to M-Prod.** "10K messages with `--debug=cpython:5678` AND no debugger attached, within 1% of no-debug baseline" requires debugpy installed in the CI test runner's Python interpreter; that's a heavy CI commit.

## Cross-references

- [`m16-debug-jsc.md`](m16-debug-jsc.md) — P9's JSC RemoteInspector story; symmetric Axis B framing.
- [`m16-architecture.md`](m16-architecture.md) — overall observer architecture (Axis A vs Axis B split).
- [`feedback_axis_b_no_browser.md`](../../../../.claude/projects/-Users-damioludiran-jays-workspace/memory/feedback_axis_b_no_browser.md) — symmetric Axis B framing rule (engine-native protocol + native IDE; never a browser).
- `~/.claude/projects/-Users-damioludiran-jays-workspace/memory/project_graphreact_m16_backend_phase8.md` — P8 closeout (debugpy substrate landed).
- debugpy reference: <https://github.com/microsoft/debugpy> + <https://code.visualstudio.com/docs/python/debugging#_remote-debugging>.
