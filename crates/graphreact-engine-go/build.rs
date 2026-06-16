//! Build script for graphreact-engine-go's Engine-conformance suite.
//!
//! Compiles `testdata/grx_go_test.go` into `$OUT_DIR/libgrx_go_test.{so,dylib,dll}`
//! via `go build -buildmode=c-shared` and exports the resulting
//! absolute path as `GRX_GO_TEST_DYLIB` so the conformance tests can
//! load it via `env!("GRX_GO_TEST_DYLIB")`.
//!
//! If the `go` toolchain is absent, emits `cargo:warning=...` and sets
//! `cargo:rustc-cfg=skip_go_tests` so the inline `#[cfg(all(test, not(skip_go_tests)))]`
//! conformance suite compiles out — CI without Go still passes.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Allow `#[cfg(skip_go_tests)]` to compile cleanly whether or not
    // we end up emitting that cfg flag.
    println!("cargo::rustc-check-cfg=cfg(skip_go_tests)");

    // Re-run if the test Go source or this build script changes.
    println!("cargo:rerun-if-changed=testdata/grx_go_stub.go");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GO");

    let go_bin = env::var("GO").unwrap_or_else(|_| "go".to_string());

    // Probe for the Go toolchain.
    let probe = Command::new(&go_bin).arg("version").output();
    let probe_ok = matches!(&probe, Ok(out) if out.status.success());
    if !probe_ok {
        println!(
            "cargo:warning=graphreact-engine-go: `{go_bin}` not on PATH \
             (or failed `go version` probe); engine-conformance tests will be skipped. \
             Install Go 1.22+ or set GO=/path/to/go to enable."
        );
        println!("cargo:rustc-cfg=skip_go_tests");
        // Still emit a dummy env var so `env!("GRX_GO_TEST_DYLIB")`
        // doesn't fail to expand at parse time. The inline test
        // module is cfg'd out so it'll never actually be loaded.
        println!("cargo:rustc-env=GRX_GO_TEST_DYLIB=/dev/null");
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo"));
    let testdata = manifest_dir.join("testdata");

    let dll_name = format!(
        "{prefix}grx_go_test{suffix}",
        prefix = env::consts::DLL_PREFIX,
        suffix = env::consts::DLL_SUFFIX,
    );
    let dll_path = out_dir.join(&dll_name);

    let status = Command::new(&go_bin)
        .current_dir(&testdata)
        .arg("build")
        .arg("-buildmode=c-shared")
        .arg("-o")
        .arg(&dll_path)
        .arg("./grx_go_stub.go")
        .status()
        .expect("failed to spawn `go build`");

    if !status.success() {
        println!(
            "cargo:warning=graphreact-engine-go: `go build -buildmode=c-shared` failed \
             (exit {status}); engine-conformance tests will be skipped."
        );
        println!("cargo:rustc-cfg=skip_go_tests");
        println!("cargo:rustc-env=GRX_GO_TEST_DYLIB=/dev/null");
        return;
    }

    // macOS Gatekeeper rejects dlopen-from-arbitrary-path of an
    // un-codesigned .dylib on Apple Silicon. Ad-hoc sign it; failure
    // is non-fatal (operators on hardened policies may need to
    // codesign manually).
    if cfg!(target_os = "macos") {
        let _ = Command::new("codesign")
            .arg("--sign")
            .arg("-")
            .arg(&dll_path)
            .status();
    }

    println!(
        "cargo:rustc-env=GRX_GO_TEST_DYLIB={}",
        dll_path.display()
    );
}
