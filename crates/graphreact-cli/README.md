# graphreact-cli

*Standalone `grx` CLI — parses and runs `.grx` files; engine selection (JSC / CPython / null) lives behind the engine-zig trait; hosts the M-Ingress daemon + observer TUI.*

## Milestone

Ships across two milestone arcs. The `grx` binary skeleton (parse / check / run / fmt / engine selection) closed with **M12.1**; the M-Ingress daemon, management API, observer TUI, debug-attach surface, and Prometheus/OTel sinks landed with **M16-Backend**. The M14 distribution slice added the `grx cluster daemon` / `grx cluster connect` subcommands ([M14 narrative](../../docs/backend/m14-distribution.md)). M16-Backend phases are tracked across [m16-architecture](../../docs/backend/m16-architecture.md), [m16-management-api](../../docs/backend/m16-management-api.md), [m16-tui](../../docs/backend/m16-tui.md), and [m16.5-ingress](../../docs/backend/m16.5-ingress.md).

## Role in the runtime

`graphreact-cli` sits at the **distribution / observer / cli tier** — one rung above the engine layer, fanning into every shipped engine (`null`, `echo`, `jsc`, `cpython`) via the `graphreact-engine-zig::Engine` trait so engine choice is a CLI flag, not a compile-time fork. It is the only crate in the workspace that knows how to turn a `.grx` path on disk plus a `--runtime` argument into a mounted, scheduled, observable system — every other consumer (subprocess tests, the cluster daemon, the TUI client) drives the runtime through this crate's commands. It composes the parser, the hot-reload pipeline, the distribution layer, and the observer endpoints into the single `grx` binary that ops operates. Crucially it owns NO runtime semantics of its own: it is wiring, dispatch, and process lifecycle around substrate that lives in `graphreact-core` and the engine crates.

## Key types

- **`Cli`** ([`src/cli.rs:22`](src/cli.rs)) — top-level clap `Parser` for the `grx` binary.
- **`Command`** ([`src/cli.rs:28`](src/cli.rs)) — the subcommand enum (`Run`, `Check`, `Parse`, `Fmt`, `Cluster`, `Observer`, `Reload`, …).
- **`EngineKind`** ([`src/engine_select.rs:8`](src/engine_select.rs)) — runtime-selection tag (`Null` / `Echo` / `Jsc` / `Cpython`).
- **`EngineSelectError`** ([`src/engine_select.rs:16`](src/engine_select.rs)) — typed failure for "engine not linked into this build" with platform-specific guidance.

## Composes with

| Crate | Role |
|---|---|
| [`graphreact-core`](../graphreact-core) | Scheduler, message dispatch, mount lifecycle that every subcommand drives. |
| [`graphreact-algorithms`](../graphreact-algorithms) | Graph algorithms surfaced by introspection subcommands. |
| [`graphreact-grx-parser`](../graphreact-grx-parser) | Parses `.grx` source into the IR consumed by `run` / `check` / `parse` / `fmt`. |
| [`graphreact-engine-zig`](../graphreact-engine-zig) | `Engine` trait that `engine_select::select` returns a boxed value for. |
| [`graphreact-engine-null`](../graphreact-engine-null) | Default zero-execution engine for CI + plumbing tests. |
| [`graphreact-engine-echo`](../graphreact-engine-echo) | Deterministic echo engine used by ingress + observer subprocess tests. |
| [`graphreact-engine-jsc`](../graphreact-engine-jsc) | JSC-backed JS body execution (macOS today; vendored WebKit per M12.3b elsewhere). |
| [`graphreact-engine-cpython`](../graphreact-engine-cpython) | CPython body execution via `pkg-config python-3.X-embed`. |
| [`graphreact-dist`](../graphreact-dist) | Cluster start / connect / shutdown for `grx cluster daemon` and `grx cluster connect`. |
| [`graphreact-hotreload`](../graphreact-hotreload) | Watch-mode file watcher + diff classifier + engine adapter driving `grx run --watch`. |
| [`graphreact-observer`](../graphreact-observer) | Management API, Prometheus, OTel, and TUI client that the daemon mounts. |

No graphreact-* crate currently depends on `graphreact-cli` — it is a leaf binary crate. Subprocess tests in [`tests/`](tests) invoke the compiled `grx` binary via `assert_cmd`.

## Running the tests

```sh
cargo test -p graphreact-cli --release
```

Mixes in-process unit tests with subprocess-driven CLI tests (`cluster_cli_subprocess.rs`, `ingress_e2e_subprocess.rs`, `observer_e2e_subprocess.rs`, `observer_ws_reconnect_subprocess.rs`, `observer_tui_smoke.rs`); subprocess tests rebuild the `grx` binary on demand.

## See also

- [M14 distribution narrative](../../docs/backend/m14-distribution.md) — `grx cluster daemon` / `connect` wiring + M14.8 CLI integration.
- [M16-Backend architecture](../../docs/backend/m16-architecture.md) — observer + management API + debug surface that the daemon mounts.
- [M16.5 ingress protocol](../../docs/backend/m16.5-ingress.md) — M-Ingress daemon hosted by `grx`.
- [Decision 0002 — backend-only](../../docs/decisions/0002-graphreact-is-backend-only.md) — why `grx` ships no bundled UI.
- [Workspace onboarding](../../docs/onboarding.md) and [architecture overview](../../docs/architecture.md).
