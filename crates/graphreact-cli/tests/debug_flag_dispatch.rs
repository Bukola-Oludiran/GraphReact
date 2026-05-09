//! **M16-Backend P8 verification gate.** End-to-end CLI dispatch
//! tests for the `--debug` flag — verifies the binary rejects
//! mismatched `--runtime` / `--debug=<engine>` combinations BEFORE
//! mounting any engine.

use std::process::Command as StdCommand;

use assert_cmd::cargo::CommandCargoExt;

#[test]
fn debug_cpython_with_runtime_null_is_rejected() {
    // grx run --debug=cpython:5678 --runtime=null <some_path> →
    // exit nonzero with engine-mismatch error.
    let mut bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let out = bin
        .args([
            "run",
            "--runtime=null",
            "--debug=cpython:5678",
            // Path is irrelevant — validation fires before mount.
            "/tmp/nonexistent.grx",
        ])
        .output()
        .expect("run grx");
    assert!(!out.status.success(), "expected failure exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--debug") && stderr.contains("cpython"),
        "expected mismatch error, got stderr: {stderr}"
    );
}

/// **M16-Backend P9 SHIPPED 2026-05-08.** JSC + `--debug=jsc:9229`
/// validates clean at dispatch — install path is the JSC RemoteInspector
/// via the Zig bridge, lockdownd-discovered by Safari Web Inspector.
/// The binary still fails on the nonexistent path, but with a file-not-
/// found error class — NOT a P8-era "not yet wired (P9)" debug-validation
/// rejection. This test pins the regression: re-introducing a JSC
/// short-circuit at validation would surface here.
#[test]
fn debug_jsc_passes_validation_post_p9() {
    let mut bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let out = bin
        .args([
            "run",
            "--runtime=jsc",
            "--debug=jsc:9229",
            "/tmp/nonexistent.grx",
        ])
        .output()
        .expect("run grx");
    assert!(!out.status.success(), "expected failure exit on bad path");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // P9 dropped the "not yet wired (P9)" short-circuit. Failure now
    // comes from path resolution / file IO, NOT debug validation.
    assert!(
        !stderr.contains("not yet wired"),
        "validation must no longer reject JSC, got stderr: {stderr}"
    );
}

/// **M16-Backend P9.** Still reject JSC + non-jsc runtime at dispatch
/// — the engine-mismatch path is unchanged from P8.
#[test]
fn debug_jsc_with_runtime_cpython_is_rejected() {
    let mut bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let out = bin
        .args([
            "run",
            "--runtime=cpython",
            "--debug=jsc:9229",
            "/tmp/nonexistent.grx",
        ])
        .output()
        .expect("run grx");
    assert!(!out.status.success(), "expected failure exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--debug") && stderr.contains("jsc"),
        "expected mismatch error naming jsc, got stderr: {stderr}"
    );
}

#[test]
fn debug_with_unknown_engine_rejected_at_parse() {
    let mut bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let out = bin
        .args([
            "run",
            "--runtime=cpython",
            "--debug=v8:9229",
            "/tmp/nonexistent.grx",
        ])
        .output()
        .expect("run grx");
    assert!(!out.status.success(), "expected failure exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown debug engine") || stderr.contains("v8"),
        "expected unknown-engine error, got stderr: {stderr}"
    );
}

#[test]
fn debug_no_wait_requires_debug_flag() {
    let mut bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let out = bin
        .args([
            "run",
            "--runtime=cpython",
            "--debug-no-wait",
            "/tmp/nonexistent.grx",
        ])
        .output()
        .expect("run grx");
    assert!(!out.status.success(), "expected failure exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // clap surfaces this as `error: the following required arguments
    // were not provided: --debug=<DEBUG>` (or similar).
    assert!(
        stderr.contains("--debug") && stderr.contains("required"),
        "expected requires-debug clap error, got stderr: {stderr}"
    );
}
