# graphreact-wasm

*WebAssembly bindings (M18 stub) — server-WASM entry point for edge compute, embedded host runtimes, sandboxed plugins (NOT a browser-frontend runtime).*

## Milestone

Ships as part of [M18 — WebAssembly server-WASM bindings](../../docs/backend/README.md) — **UNSHIPPED (stub)**. The current crate exposes a single `wasm_smoke` export so the workspace compiles with `crate-type = ["cdylib", "rlib"]` wired up and the wasm-bindgen attribute path validated; the real binding surface lands at M18. The intentional architectural framing (server-WASM, not browser) is locked in by [decision 0002 — GraphReact is backend-only](../../docs/decisions/0002-graphreact-is-backend-only.md).

## Role in the runtime

`graphreact-wasm` is a **distribution-tier** crate — peer to `graphreact-napi` rather than to the engines. It lives downstream of `graphreact-core` and (eventually) the engine crates, packaging the runtime for a WebAssembly host. Target deployments are **edge compute** (Cloudflare Workers, Fastly Compute@Edge), **embedded host runtimes** (Wasmtime, Wasmer, WAMR), and **sandboxed plugin** scenarios where the host wants the GraphReact scheduler without granting full OS access. Browsers are deliberately out of scope: running the runtime client-side contradicts the N-tier commitment in decision 0002, even though a browser may occasionally appear as a CI-convenient WASM runner. The M18 surface is intentionally smaller than the NAPI binding — no file-watch, no HTTP sources, no multi-thread tokio (async via `spawn_local`).

## Key types

- **`wasm_smoke`** ([`src/lib.rs:20`](src/lib.rs)) — the placeholder `#[wasm_bindgen]` export. Returns `"graphreact-wasm alive"`; exists solely to prove the cdylib + wasm-bindgen toolchain path before the M18 surface is designed.

The real type surface (scheduler handle, mount/unmount, message send, batch driver) lands at M18 and will be mirrored from the NAPI binding's shape minus host-IO capabilities.

## Composes with

| Crate | Role |
|---|---|
| [`graphreact-core`](../graphreact-core/) | The Scheduler + Message + supervisor substrate this crate will package for a WebAssembly host. |
| `wasm-bindgen` (external) | The `#[wasm_bindgen]` attribute machinery used to generate the JS/host glue for the cdylib. |

No other workspace crate currently depends on `graphreact-wasm`; it is a leaf distribution target.

## Running the tests

```sh
cargo test -p graphreact-wasm --release
```

The crate currently has no dedicated test modules — `cargo test` compiles the stub against the host target and verifies `wasm_smoke` builds clean. Real test coverage (likely via `wasm-bindgen-test` against Wasmtime / browser headless) lands with M18.

## See also

- Parent milestone narrative: [`../../docs/backend/README.md`](../../docs/backend/README.md) (M18 section)
- Decision: [`../../docs/decisions/0002-graphreact-is-backend-only.md`](../../docs/decisions/0002-graphreact-is-backend-only.md) — why server-WASM, not browser-frontend
- Decision: [`../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md`](../../docs/decisions/0003-graphreact-bifurcates-into-backend-and-db.md) — backend vs DB product split that locates this crate in the Backend tree
- Onboarding: [`../../docs/onboarding.md`](../../docs/onboarding.md)
- Architecture: [`../../docs/architecture.md`](../../docs/architecture.md)
