//! `ctx.secret(name)` — M-Prod sub-track #3 Phase B Part 1 safe-slice
//! integration test.
//!
//! Drives the JSC engine directly (no scheduler) to prove three
//! invariants of the ADR-0007 contract:
//!
//! 1. **Declared name + present store entry** → body observes the
//!    secret value as a JS string.
//! 2. **Declared name + missing store entry** → body observes `null`
//!    (the store backend's "not found" surfaces as the
//!    default-deny shape).
//! 3. **Undeclared name (default-deny)** → body observes `null` even
//!    though the store has the value.
//!
//! The needs_secret manifest allowlist enforcement is exercised
//! INSIDE the test fixture's `TestSecretSource::read` — it stands in
//! for the Phase B Part 2 production wiring where
//! `graphreact-cli`'s `host_secret` BoxCallback factory will read
//! `current_node_id()` + `Scheduler::manifests` and check
//! `manifest.needs_secret`. The safe-slice's job is to lock the
//! trait + JS-prelude + manifest-field surface so the production
//! wiring can land later without code-shape churn.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use graphreact_engine_jsc::JscEngine;
use graphreact_engine_zig::{
    BoxCallback, Engine, HostSecretCallback, HostValue, IsolateHandle, IsolateOptions,
};
use graphreact_secrets::SecretValue;

/// JSC's `JSContextGroup` is process-global; tests must serialise so
/// `engine.initialize() / shutdown()` pairs don't race. Mirrors the
/// `engine_lock()` pattern from `src/lib.rs`'s inline tests.
fn engine_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Test-only secret source. Bundles the manifest-side `needs_secret`
/// allowlist + the store-side values into a single owner so the
/// `HostSecretCallback::read` impl can enforce both gates in one
/// step. The production-side factory will split these (allowlist
/// from `Scheduler::manifests`, values from the registered
/// `Arc<dyn SecretStore>`).
struct TestSecretSource {
    allow: Vec<String>,
    values: std::collections::HashMap<String, Vec<u8>>,
}

impl HostSecretCallback for TestSecretSource {
    fn read(&self, name: &str) -> Option<SecretValue> {
        if !self.allow.iter().any(|n| n == name) {
            // default-deny: undeclared names resolve to None at the
            // host-callback boundary; JSC then marshals None → null.
            return None;
        }
        self.values
            .get(name)
            .and_then(|v| SecretValue::from_bytes(v.clone()).ok())
    }
}

/// Register a `host_secret` BoxCallback on the JSC engine that
/// forwards to the supplied `TestSecretSource`. The closure marshals
/// the SecretValue's bytes as `HostValue::String` for the JS side —
/// matches the ADR-0007 UTF-8-string boundary contract.
fn install_host_secret(
    e: &mut JscEngine,
    iso: IsolateHandle,
    src: Arc<TestSecretSource>,
) {
    let cb: BoxCallback = Box::new(move |args| {
        let name = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Ok(HostValue::Null),
        };
        match src.read(&name) {
            Some(v) => {
                // SAFE-SLICE: marshal as UTF-8 string per ADR-0007.
                // Non-UTF-8 secrets resolve to null + (Phase B Part 2)
                // emit a tracing::warn! on the
                // graphreact::secrets::audit target.
                match std::str::from_utf8(v.expose_bytes()) {
                    Ok(s) => Ok(HostValue::String(s.to_string())),
                    Err(_) => Ok(HostValue::Null),
                }
            }
            None => Ok(HostValue::Null),
        }
    });
    e.register_callback(iso, "host_secret", cb)
        .expect("register host_secret");
}

/// Build the `ctx` HostValue the JSC component body sees on
/// invocation. The body uses `ctx.secret("FOO")` via the prelude's
/// `__augmentCtx`, so the raw `ctx.id` is enough to keep the body
/// well-typed.
fn make_ctx(node_id: &str) -> HostValue {
    HostValue::Object(vec![
        ("id".to_string(), HostValue::String(node_id.to_string())),
        ("props".to_string(), HostValue::Object(vec![])),
    ])
}

/// Read the `observed` field off a returned manifest HostValue.
fn observed_field(manifest: HostValue) -> HostValue {
    let HostValue::Object(pairs) = manifest else {
        panic!("expected manifest object, got: {manifest:?}");
    };
    pairs
        .into_iter()
        .find(|(k, _)| k == "observed")
        .map(|(_, v)| v)
        .unwrap_or(HostValue::Null)
}

#[test]
fn ctx_secret_declared_present_returns_value() {
    let _g = engine_lock();
    let mut e = JscEngine::new();
    e.initialize().expect("init");
    let iso = e.create_isolate(IsolateOptions::default()).expect("create");

    let src = Arc::new(TestSecretSource {
        allow: vec!["FOO".to_string()],
        values: {
            let mut m = std::collections::HashMap::new();
            m.insert("FOO".to_string(), b"bar".to_vec());
            m
        },
    });
    install_host_secret(&mut e, iso, src);

    let fn_handle = e
        .eval_component(
            iso,
            "SecretReader",
            "return { observed: ctx.secret(\"FOO\"), accepts: [], emits: [] };",
        )
        .expect("eval_component");

    let manifest = e
        .call_function(iso, fn_handle, &[make_ctx("n0")])
        .expect("call");
    let observed = observed_field(manifest);
    assert!(
        matches!(observed, HostValue::String(ref s) if s == "bar"),
        "expected String(\"bar\"), got: {observed:?}"
    );

    e.dispose_isolate(iso).expect("dispose");
    e.shutdown().expect("shutdown");
}

#[test]
fn ctx_secret_declared_missing_returns_null() {
    let _g = engine_lock();
    let mut e = JscEngine::new();
    e.initialize().expect("init");
    let iso = e.create_isolate(IsolateOptions::default()).expect("create");

    let src = Arc::new(TestSecretSource {
        allow: vec!["FOO".to_string()],
        values: std::collections::HashMap::new(),
    });
    install_host_secret(&mut e, iso, src);

    let fn_handle = e
        .eval_component(
            iso,
            "SecretReader",
            "return { observed: ctx.secret(\"FOO\"), accepts: [], emits: [] };",
        )
        .expect("eval_component");

    let manifest = e
        .call_function(iso, fn_handle, &[make_ctx("n0")])
        .expect("call");
    let observed = observed_field(manifest);
    assert!(
        matches!(observed, HostValue::Null),
        "expected Null, got: {observed:?}"
    );

    e.dispose_isolate(iso).expect("dispose");
    e.shutdown().expect("shutdown");
}

#[test]
fn ctx_secret_undeclared_returns_null() {
    let _g = engine_lock();
    let mut e = JscEngine::new();
    e.initialize().expect("init");
    let iso = e.create_isolate(IsolateOptions::default()).expect("create");

    // Allowlist is empty — the value is present in the store but the
    // body's manifest (stand-in: `allow`) hasn't declared `FOO`, so
    // the host-callback returns None → JSC marshals to null.
    let src = Arc::new(TestSecretSource {
        allow: Vec::new(),
        values: {
            let mut m = std::collections::HashMap::new();
            m.insert("FOO".to_string(), b"bar".to_vec());
            m
        },
    });
    install_host_secret(&mut e, iso, src);

    let fn_handle = e
        .eval_component(
            iso,
            "SecretReader",
            "return { observed: ctx.secret(\"FOO\"), accepts: [], emits: [] };",
        )
        .expect("eval_component");

    let manifest = e
        .call_function(iso, fn_handle, &[make_ctx("n0")])
        .expect("call");
    let observed = observed_field(manifest);
    assert!(
        matches!(observed, HostValue::Null),
        "expected Null (default-deny), got: {observed:?}"
    );

    e.dispose_isolate(iso).expect("dispose");
    e.shutdown().expect("shutdown");
}
