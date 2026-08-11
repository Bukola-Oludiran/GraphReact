//! End-to-end conformance smoke test for graphreact-engine-go.
//!
//! Runs every conformance scenario sequentially in **one process**
//! and calls [`std::process::exit`] at the end, dodging the Go
//! c-shared TSD-cleanup deadlock that fires on macOS when a host
//! thread that called into Go tries to exit normally.
//!
//! `cargo test` runs each integration-test binary in its own
//! process, so a single binary that runs N test scenarios
//! sequentially gives the same coverage as N `#[test]` functions
//! without per-test thread teardown.
//!
//! Skipped automatically when the build.rs probe detected no Go
//! toolchain (the `skip_go_tests` cfg flag is propagated through
//! the engine-go crate; this integration test reads the same
//! `GRX_GO_TEST_DYLIB` env var the conformance suite uses).
//!
//! Run with `cargo test -p graphreact-engine-go --release
//! --test conformance_smoke`.

use std::path::Path;

use graphreact_engine_go::{__test_callback_registry_contains, GoEngine};
use graphreact_engine_zig::{
    BoxCallback, Engine, EngineError, EngineResult, FunctionHandle, HostValue, IsolateHandle,
    IsolateOptions, ModuleHandle,
};

fn fresh_engine() -> GoEngine {
    let path = option_env!("GRX_GO_TEST_DYLIB").unwrap_or("/dev/null");
    GoEngine::with_artifact(Path::new(path))
}

fn run_all() -> Result<(), String> {
    let path = option_env!("GRX_GO_TEST_DYLIB").unwrap_or("/dev/null");
    if path == "/dev/null" {
        eprintln!("skipping: GRX_GO_TEST_DYLIB not set (Go toolchain absent at build time)");
        return Ok(());
    }
    if !Path::new(path).exists() {
        eprintln!("skipping: {path} not present", path = path);
        return Ok(());
    }

    scenario_loads_and_reports_abi_version()?;
    scenario_initialize_and_shutdown_idempotent()?;
    scenario_create_and_dispose_isolate()?;
    scenario_bad_isolate_handle_rejected()?;
    scenario_eval_module_resolve_and_call_add()?;
    scenario_register_callback_round_trips()?;
    scenario_register_duplicate_callback_rejected()?;
    scenario_dispose_drains_callback_registry()?;
    scenario_large_payload_grows_buffer()?;
    scenario_large_payload_cap_exceeded_errors()?;
    scenario_large_callback_grows_trampoline_buffer()?;
    scenario_drain_microtasks_is_soft_no_op()?;

    Ok(())
}

fn scenario_loads_and_reports_abi_version() -> Result<(), String> {
    eprintln!("[1/12] loads_artifact_and_reports_abi_version");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

fn scenario_create_and_dispose_isolate() -> Result<(), String> {
    eprintln!("[2/12] create_and_dispose_isolate");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    let iso = e
        .create_isolate(IsolateOptions::default())
        .map_err(|err| format!("create_isolate: {err:?}"))?;
    e.dispose_isolate(iso)
        .map_err(|err| format!("dispose_isolate: {err:?}"))?;
    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

fn scenario_eval_module_resolve_and_call_add() -> Result<(), String> {
    eprintln!("[3/12] eval_module_resolve_and_call_add");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    let iso = e
        .create_isolate(IsolateOptions::default())
        .map_err(|err| format!("create_isolate: {err:?}"))?;
    let module = e
        .eval_module(iso, "calculator", "calculator")
        .map_err(|err| format!("eval_module: {err:?}"))?;
    let func = e
        .resolve_function(iso, module, "Add")
        .map_err(|err| format!("resolve Add: {err:?}"))?;
    let result = e
        .call_function(iso, func, &[HostValue::Number(2.0), HostValue::Number(3.0)])
        .map_err(|err| format!("call Add(2,3): {err:?}"))?;
    let HostValue::Number(n) = result else {
        return Err(format!("expected Number, got {result:?}"));
    };
    if (n - 5.0).abs() >= f64::EPSILON {
        return Err(format!("Add(2,3) = {n}, expected 5"));
    }
    e.dispose_isolate(iso)
        .map_err(|err| format!("dispose_isolate: {err:?}"))?;
    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

fn scenario_register_callback_round_trips() -> Result<(), String> {
    eprintln!("[4/12] register_callback_round_trips");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    let iso = e
        .create_isolate(IsolateOptions::default())
        .map_err(|err| format!("create_isolate: {err:?}"))?;

    let cb: BoxCallback = Box::new(|args: &[HostValue]| -> EngineResult<HostValue> {
        let n = match args.first() {
            Some(HostValue::Number(n)) => *n,
            _ => return Err(EngineError::Internal("expected number arg".into())),
        };
        Ok(HostValue::Number(n * 2.0))
    });
    let id = e
        .register_callback(iso, "double", cb)
        .map_err(|err| format!("register_callback: {err:?}"))?;
    if id.0 == 0 {
        return Err("expected non-zero callback id".to_string());
    }

    let module = e
        .eval_module(iso, "callbacks", "callbacks")
        .map_err(|err| format!("eval_module callbacks: {err:?}"))?;
    let func = e
        .resolve_function(iso, module, "InvokeHostDouble")
        .map_err(|err| format!("resolve InvokeHostDouble: {err:?}"))?;
    let result = e
        .call_function(iso, func, &[HostValue::Number(21.0)])
        .map_err(|err| format!("call InvokeHostDouble(21): {err:?}"))?;
    let HostValue::Number(n) = result else {
        return Err(format!("expected Number, got {result:?}"));
    };
    if (n - 42.0).abs() >= f64::EPSILON {
        return Err(format!(
            "InvokeHostDouble(21) round-tripped to {n}, expected 42"
        ));
    }

    e.dispose_isolate(iso)
        .map_err(|err| format!("dispose_isolate: {err:?}"))?;
    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

fn scenario_initialize_and_shutdown_idempotent() -> Result<(), String> {
    eprintln!("[1b/12] initialize_and_shutdown_idempotent");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize 1: {err:?}"))?;
    e.initialize()
        .map_err(|err| format!("initialize 2 (idempotent): {err:?}"))?;
    e.shutdown().map_err(|err| format!("shutdown 1: {err:?}"))?;
    e.shutdown()
        .map_err(|err| format!("shutdown 2 (idempotent): {err:?}"))?;
    Ok(())
}

fn scenario_bad_isolate_handle_rejected() -> Result<(), String> {
    eprintln!("[2b/12] bad_isolate_handle_rejected");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    let bad = IsolateHandle(999_999);
    let module_result = e.eval_module(bad, "calculator", "calculator");
    if !matches!(module_result, Err(EngineError::InvalidIsolate)) {
        return Err(format!(
            "eval_module with bad iso: expected InvalidIsolate, got {module_result:?}"
        ));
    }
    let resolve_result = e.resolve_function(bad, ModuleHandle(1), "Add");
    if !matches!(resolve_result, Err(EngineError::InvalidIsolate)) {
        return Err(format!(
            "resolve_function with bad iso: expected InvalidIsolate, got {resolve_result:?}"
        ));
    }
    let call_result = e.call_function(bad, FunctionHandle(1), &[]);
    if !matches!(call_result, Err(EngineError::InvalidIsolate)) {
        return Err(format!(
            "call_function with bad iso: expected InvalidIsolate, got {call_result:?}"
        ));
    }
    let dispose_result = e.dispose_isolate(bad);
    if !matches!(dispose_result, Err(EngineError::InvalidIsolate)) {
        return Err(format!(
            "dispose_isolate with bad iso: expected InvalidIsolate, got {dispose_result:?}"
        ));
    }
    let cb: BoxCallback = Box::new(|_| Ok(HostValue::Null));
    let register_result = e.register_callback(bad, "nope", cb);
    if !matches!(register_result, Err(EngineError::InvalidIsolate)) {
        return Err(format!(
            "register_callback with bad iso: expected InvalidIsolate, got {register_result:?}"
        ));
    }
    let drain_result = e.drain_microtasks(bad);
    if !matches!(drain_result, Err(EngineError::InvalidIsolate)) {
        return Err(format!(
            "drain_microtasks with bad iso: expected InvalidIsolate, got {drain_result:?}"
        ));
    }
    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

fn scenario_register_duplicate_callback_rejected() -> Result<(), String> {
    eprintln!("[4b/12] register_duplicate_callback_rejected");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    let iso = e
        .create_isolate(IsolateOptions::default())
        .map_err(|err| format!("create_isolate: {err:?}"))?;

    let cb1: BoxCallback = Box::new(|_| Ok(HostValue::Null));
    e.register_callback(iso, "dup_cb", cb1)
        .map_err(|err| format!("first register: {err:?}"))?;
    let cb2: BoxCallback = Box::new(|_| Ok(HostValue::Null));
    let duplicate = e.register_callback(iso, "dup_cb", cb2);
    if !matches!(
        duplicate,
        Err(EngineError::CallbackAlreadyRegistered { .. })
    ) {
        return Err(format!(
            "expected CallbackAlreadyRegistered, got {duplicate:?}"
        ));
    }

    e.dispose_isolate(iso)
        .map_err(|err| format!("dispose: {err:?}"))?;
    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

fn scenario_dispose_drains_callback_registry() -> Result<(), String> {
    eprintln!("[4c/12] dispose_drains_callback_registry");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    let iso = e
        .create_isolate(IsolateOptions::default())
        .map_err(|err| format!("create_isolate: {err:?}"))?;

    let cb: BoxCallback = Box::new(|_| Ok(HostValue::Null));
    let id = e
        .register_callback(iso, "drain_me", cb)
        .map_err(|err| format!("register: {err:?}"))?;
    if !__test_callback_registry_contains(id.0) {
        return Err(format!(
            "callback id {} not in registry post-register",
            id.0
        ));
    }

    e.dispose_isolate(iso)
        .map_err(|err| format!("dispose: {err:?}"))?;
    if __test_callback_registry_contains(id.0) {
        return Err(format!(
            "callback id {} still in registry after dispose_isolate",
            id.0
        ));
    }

    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

fn scenario_large_payload_grows_buffer() -> Result<(), String> {
    eprintln!("[5b/12] large_payload_grows_buffer");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    let iso = e
        .create_isolate(IsolateOptions::default())
        .map_err(|err| format!("create_isolate: {err:?}"))?;
    let module = e
        .eval_module(iso, "payloads", "payloads")
        .map_err(|err| format!("eval_module: {err:?}"))?;
    let func = e
        .resolve_function(iso, module, "LargeJson")
        .map_err(|err| format!("resolve LargeJson: {err:?}"))?;

    // n=64 → roughly 6.4 KiB payload. Initial buffer is 4 KiB, so
    // the grow-and-retry loop fires at least once.
    let result = e
        .call_function(iso, func, &[HostValue::Number(64.0)])
        .map_err(|err| format!("call LargeJson(64): {err:?}"))?;
    let HostValue::Array(arr) = result else {
        return Err(format!("expected Array, got {result:?}"));
    };
    if arr.len() != 64 {
        return Err(format!("expected 64 strings, got {}", arr.len()));
    }

    e.dispose_isolate(iso)
        .map_err(|err| format!("dispose_isolate: {err:?}"))?;
    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

fn scenario_large_payload_cap_exceeded_errors() -> Result<(), String> {
    eprintln!("[5c/12] large_payload_cap_exceeded_errors");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    let iso = e
        .create_isolate(IsolateOptions::default())
        .map_err(|err| format!("create_isolate: {err:?}"))?;
    let module = e
        .eval_module(iso, "payloads", "payloads")
        .map_err(|err| format!("eval_module: {err:?}"))?;
    let func = e
        .resolve_function(iso, module, "LargeJson")
        .map_err(|err| format!("resolve LargeJson: {err:?}"))?;

    // n=20000 → ~2 MiB payload, exceeds the 1 MiB cap. Must error
    // with an Internal "exceeded 1 MiB" message, NOT spin forever
    // at cap=1 MiB. This is the cap-loop bug-fix coverage.
    let result = e.call_function(iso, func, &[HostValue::Number(20_000.0)]);
    let Err(EngineError::Internal(msg)) = result else {
        return Err(format!(
            "expected Internal('exceeded 1 MiB'), got {result:?}"
        ));
    };
    if !msg.contains("1 MiB") {
        return Err(format!("expected '1 MiB' in error message, got: {msg}"));
    }

    e.dispose_isolate(iso)
        .map_err(|err| format!("dispose_isolate: {err:?}"))?;
    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

fn scenario_large_callback_grows_trampoline_buffer() -> Result<(), String> {
    eprintln!("[5d/12] large_callback_grows_trampoline_buffer");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    let iso = e
        .create_isolate(IsolateOptions::default())
        .map_err(|err| format!("create_isolate: {err:?}"))?;

    // Host callback returns a ~6 KiB string. The Go stub's
    // InvokeLargeEcho pre-allocates a 4 KiB result buffer, so the
    // trampoline's GR_ERR_BUFFER_TOO_SMALL branch fires + the
    // Go-side grow-once handler kicks in. Coverage for the
    // trampoline write-buffer path that the InvokeHostDouble
    // scenario can't exercise (its result is < 10 bytes).
    let cb: BoxCallback = Box::new(|args: &[HostValue]| -> EngineResult<HostValue> {
        let _ = args;
        let big = "y".repeat(6 * 1024);
        Ok(HostValue::String(big))
    });
    e.register_callback(iso, "large_echo", cb)
        .map_err(|err| format!("register large_echo: {err:?}"))?;

    let module = e
        .eval_module(iso, "callbacks", "callbacks")
        .map_err(|err| format!("eval_module callbacks: {err:?}"))?;
    let func = e
        .resolve_function(iso, module, "InvokeLargeEcho")
        .map_err(|err| format!("resolve InvokeLargeEcho: {err:?}"))?;
    let result = e
        .call_function(iso, func, &[])
        .map_err(|err| format!("call InvokeLargeEcho: {err:?}"))?;
    let HostValue::String(s) = result else {
        return Err(format!("expected String, got {result:?}"));
    };
    if s.len() != 6 * 1024 {
        return Err(format!("expected 6 KiB echo, got {} bytes", s.len()));
    }

    e.dispose_isolate(iso)
        .map_err(|err| format!("dispose_isolate: {err:?}"))?;
    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

fn scenario_drain_microtasks_is_soft_no_op() -> Result<(), String> {
    eprintln!("[12/12] drain_microtasks_is_soft_no_op");
    let mut e = fresh_engine();
    e.initialize()
        .map_err(|err| format!("initialize: {err:?}"))?;
    let iso = e
        .create_isolate(IsolateOptions::default())
        .map_err(|err| format!("create_isolate: {err:?}"))?;
    e.drain_microtasks(iso)
        .map_err(|err| format!("drain_microtasks: {err:?}"))?;
    e.dispose_isolate(iso)
        .map_err(|err| format!("dispose_isolate: {err:?}"))?;
    e.shutdown().map_err(|err| format!("shutdown: {err:?}"))?;
    Ok(())
}

#[test]
fn engine_go_conformance_smoke() {
    match run_all() {
        Ok(()) => {
            eprintln!("conformance: ALL SCENARIOS PASSED");
            // Exit fast to dodge the Go runtime TSD destructor that
            // blocks pthread_exit on macOS. The test framework
            // never gets to do a normal join; the process is
            // already terminating cleanly.
            std::process::exit(0);
        }
        Err(msg) => {
            eprintln!("conformance: FAILED — {msg}");
            std::process::exit(1);
        }
    }
}
