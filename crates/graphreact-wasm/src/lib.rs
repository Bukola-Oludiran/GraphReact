//! WebAssembly bindings for GraphReact — M18 stub.
//!
//! Server-WASM entry point for the runtime. The intended deployment
//! targets are edge-compute platforms (Cloudflare Workers, Fastly
//! Compute@Edge), embedded host runtimes (Wasmtime, Wasmer, WAMR),
//! and plugin/sandbox scenarios — NOT browser frontends.
//! GraphReact is a backend runtime; running it client-side in a
//! browser would conflict with the N-tier architectural commitment
//! (see `graphreactRust/docs/decisions/0002-graphreact-is-backend-only.md`).
//! Browsers may incidentally appear as CI-convenient WASM runners,
//! but they are not the target.
//!
//! Implementation lands at M18; the surface is intentionally smaller
//! than the NAPI binding (no file-watch, no HTTP sources, no
//! multi-thread tokio — `spawn_local` for async).

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn wasm_smoke() -> String {
    "graphreact-wasm alive".to_string()
}
