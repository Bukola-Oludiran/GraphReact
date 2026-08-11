//! M17 phase 3f acceptance: 100 isolates × 1000 messages, no GC
//! contention deadlock.
//!
//! Drives the existing `grx_go_stub` fixture
//! (`callbacks.InvokeHostDouble` plus a per-isolate `double` host
//! callback) at scale. Each call hits the full FFI round-trip: Rust
//! `GoEngine::call_function` enters `GrxFunctionCall` (per-isolate
//! `is.mu`, then unlocked body); the body switches on fnKey to
//! `"callbacks.InvokeHostDouble"` and invokes
//! `C.grx_invoke_host_callback` into the Rust trampoline; the
//! Rust closure doubles the number and ships a JSON envelope back
//! through the cgo result buffer.
//!
//! So 100 isolates × 1000 messages = 100k FFI round-trips, exercising
//! every process-global Go-side lock the prelude declares
//! (stMu for isoLookup + per-isolate is.mu for the function-table
//! read + the cgo trampoline's host-callback path). A wedged GC
//! contention deadlock would hang at least one of those 100k calls;
//! the libc::_exit(125) watchdog at 120s catches that.
//!
//! Why a separate test file (not folded into `conformance_smoke.rs`):
//! the conformance suite calls `std::process::exit(0)` at the end to
//! dodge the macOS TSD-cleanup deadlock (golang/go#47100); we want
//! both signals — "conformance green" AND "stress green" — visible
//! as independent test results in `cargo test` output. They can't
//! share a process because the Go runtime is one-shot per process.
//!
//! Run with:
//!   cargo test -p graphreact-engine-go --release --test stress_100x1k -- --nocapture
//!
//! Release mode is mandatory: the debug allocator dominates 100k+
//! lock acquisitions and skews the timing budget by 5-10×.

#![allow(unsafe_code)] // libc::_exit watchdog escape is the only safe pattern; see arm_watchdog().

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use graphreact_engine_go::GoEngine;
use graphreact_engine_zig::{
    BoxCallback, Engine, EngineError, EngineResult, HostValue, IsolateOptions,
};

const N_ISOLATES: usize = 100;
const N_ROUNDS: usize = 1000;
const WATCHDOG_SECS: u64 = 120;
// Release-mode budget; debug widens to 5× to absorb the debug
// allocator. Local release run lands at ~407ms; CI runners are
// noisier but the 60s ceiling still gives a 100× safety factor
// for the production-speed signal.
const BUDGET_SECS_RELEASE: u64 = 60;
const BUDGET_SECS_DEBUG: u64 = 60 * 5;

fn artifact_path() -> Option<&'static str> {
    let path = option_env!("GRX_GO_TEST_DYLIB").unwrap_or("/dev/null");
    if path == "/dev/null" || !Path::new(path).exists() {
        eprintln!("skipping stress_100x1k: GRX_GO_TEST_DYLIB unset or {path} missing");
        return None;
    }
    Some(path)
}

/// Spawn a watchdog thread that calls `libc::_exit(125)` after
/// [`WATCHDOG_SECS`]. We use raw `_exit` instead of
/// `std::process::exit` (runs destructors) or `std::process::abort`
/// (raises SIGABRT, may itself deadlock against pthread cleanup):
/// the wedged-Go-runtime case is exactly the
/// destructors-themselves-deadlock hazard from golang/go#47100 that
/// ADR-0005 cites for the test-process-exit dodge. Inside `_exit`
/// the kernel reaps the process without touching userspace cleanup.
fn arm_watchdog() {
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(WATCHDOG_SECS));
        eprintln!(
            "[stress_100x1k] WATCHDOG FIRED after {WATCHDOG_SECS}s — \
             100 isolates × 1k messages did not complete. \
             Suspect GC contention deadlock or cgo-trampoline wedge."
        );
        // SAFETY: `_exit` is a syscall wrapper that bypasses every
        // userspace cleanup path — the only safe way to terminate
        // when a userspace cleanup path is suspected of deadlocking.
        unsafe { libc::_exit(125) };
    });
}

/// Build the per-isolate "double" callback. Each call doubles the
/// first numeric argument and returns it; identical shape to the
/// conformance suite's [`scenario_register_callback_round_trips`]
/// callback so the FFI round-trip semantics match exactly.
fn double_callback() -> BoxCallback {
    Box::new(|args: &[HostValue]| -> EngineResult<HostValue> {
        let n = match args.first() {
            Some(HostValue::Number(n)) => *n,
            _ => return Err(EngineError::Internal("expected number arg".into())),
        };
        Ok(HostValue::Number(n * 2.0))
    })
}

#[test]
fn stress_100_isolates_1k_messages() {
    let Some(path) = artifact_path() else {
        return;
    };

    arm_watchdog();

    let mut engine = GoEngine::with_artifact(Path::new(path));
    engine.initialize().expect("initialize");
    assert_eq!(
        engine.live_isolates(),
        0,
        "pre-condition: no live isolates after initialize"
    );

    // Pre-create all 100 isolates + their modules / funcs / callbacks
    // serially. This exhaustively exercises the isolate-lifecycle
    // path (GrxIsolateCreate → eval_module → resolve_function →
    // register_callback) under the contention from no other thread
    // — a serial baseline that tells us those paths complete cleanly
    // at 100x repetition before the hot loop starts.
    let mut isos_funcs: Vec<(
        graphreact_engine_zig::IsolateHandle,
        graphreact_engine_zig::FunctionHandle,
    )> = Vec::with_capacity(N_ISOLATES);
    for _ in 0..N_ISOLATES {
        let iso = engine
            .create_isolate(IsolateOptions::default())
            .expect("create_isolate");
        let module = engine
            .eval_module(iso, "callbacks", "callbacks")
            .expect("eval_module callbacks");
        let func = engine
            .resolve_function(iso, module, "InvokeHostDouble")
            .expect("resolve InvokeHostDouble");
        engine
            .register_callback(iso, "double", double_callback())
            .expect("register double");
        isos_funcs.push((iso, func));
    }
    assert_eq!(
        engine.live_isolates(),
        N_ISOLATES as u64,
        "live_isolates should equal N_ISOLATES after pre-create"
    );

    // HOT LOOP — every iteration drives one full FFI round-trip
    // per isolate. Verify each round-trip's return value matches
    // the doubled input so a regression that short-circuits the
    // cgo path (e.g. an accidental fast-path that skips the host
    // callback) is caught alongside the contention assertion.
    let start = Instant::now();
    for round in 0..N_ROUNDS {
        // Loop bound is N_ROUNDS = 1000; round fits in f64 exactly.
        #[allow(clippy::cast_precision_loss)]
        let round_f = round as f64;
        for (iso, func) in &isos_funcs {
            let r = engine
                .call_function(*iso, *func, &[HostValue::Number(round_f)])
                .expect("call_function");
            let HostValue::Number(n) = r else {
                panic!("expected Number, got {r:?}");
            };
            let expected = round_f * 2.0;
            assert!(
                (n - expected).abs() < f64::EPSILON,
                "round={round}: got {n}, expected {expected}"
            );
        }
    }
    let elapsed = start.elapsed();
    let total = N_ISOLATES * N_ROUNDS;
    // total fits in f64 exactly at the small constants we use.
    #[allow(clippy::cast_precision_loss)]
    let throughput = (total as f64) / elapsed.as_secs_f64();
    let budget_secs = if cfg!(debug_assertions) {
        BUDGET_SECS_DEBUG
    } else {
        BUDGET_SECS_RELEASE
    };
    eprintln!(
        "[stress_100x1k] {total} round-trips in {elapsed:?} = {throughput:.0} msgs/sec \
         (budget {budget_secs}s)"
    );
    assert!(
        elapsed < Duration::from_secs(budget_secs),
        "100×1k took {elapsed:?}, exceeded {budget_secs}s budget — \
         possible contention regression (watchdog at {WATCHDOG_SECS}s)"
    );

    // Serial teardown — each dispose hits stMu + scrubs the
    // per-isolate state. Confirms the lifecycle exit path is
    // clean at 100x repetition.
    for (iso, _) in isos_funcs {
        engine.dispose_isolate(iso).expect("dispose_isolate");
    }
    assert_eq!(
        engine.live_isolates(),
        0,
        "post-condition: all isolates disposed"
    );

    engine.shutdown().expect("shutdown");

    eprintln!("[stress_100x1k] PASSED — no contention deadlock");
    // Same exit dodge `conformance_smoke.rs` uses to avoid the
    // macOS TSD-cleanup deadlock that fires on normal pthread_exit
    // after Go has been initialized. The kernel reaps; test
    // framework records the assertion outcome via the test-binary
    // exit code (0 = success).
    std::process::exit(0);
}
