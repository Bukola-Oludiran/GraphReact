//! NAPI bindings for GraphReact — `@graphreact/native`.
//!
//! Exposes the core runtime, algorithms, and graspologic analytics to
//! Node.js consumers. Stages:
//!
//! * Stage 1 — module scaffold + serde bridge (this file).
//! * Stage 2 — `JsCommunicationGraph` and algorithm wrappers.
//! * Stage 3 — `JsScheduler`, `defineNode`, and the hooks bridge.
//!
//! The hot-path constraint from the rewrite plan is preserved: user
//! component evaluation runs on the V8 main thread (the thread the JS
//! call entered on), so JS-side hooks can call back into Rust without
//! `ThreadsafeFunction` ceremony.

#![allow(clippy::missing_safety_doc)]

#[macro_use]
extern crate napi_derive;

pub mod algorithms;
pub mod conversion;
pub mod graph;
pub mod hooks;
pub mod scheduler;
pub mod sources;

#[napi]
pub fn napi_smoke() -> &'static str {
    "graphreact-napi alive"
}

/// Returns the crate version string. Consumers use this to verify the
/// loaded native binary matches the JS package version.
#[napi]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
