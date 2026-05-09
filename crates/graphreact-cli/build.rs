//! Bake an `-rpath` for libpython into the `grx` binary so dyld can
//! resolve `libpython3.X.dylib` (or `@rpath/Python3.framework/.../Python3`)
//! at load time without `DYLD_LIBRARY_PATH` being set in the environment.
//!
//! `engine-cpython/build.rs` emits the same `-rpath` flag for its own
//! tests, but `cargo:rustc-link-arg` directives don't propagate
//! transitively to downstream crates' binaries — the `grx` binary
//! lives in `graphreact-cli`, not `engine-cpython`, so we re-emit
//! the rpath here. Read the resolved rpath from
//! `DEP_GRAPHREACT_CPYTHON_PYTHON_RPATH` (cargo wires this through
//! because engine-cpython's Cargo.toml declares `links =
//! "graphreact_cpython"` and its build.rs emits
//! `cargo:python-rpath=...`).
//!
//! Also forwards `--cfg python_3_12` when the engine-cpython resolution
//! detected Python 3.12+ — `grx run --runtime=cpython` doesn't itself
//! branch on the cfg, but tests in this crate's runtime layer
//! (`pyruntime`) may want to gate behavior under the same flag in
//! future slices.
//!
//! Python 3.12+ floor: engine-cpython's `build.rs` refuses to link
//! Python <3.12, so the rpath always points at a Homebrew/Apt/etc.
//! libpython directory. The Apple Command Line Tools 3.9 framework
//! fallback was removed at the 3.12+ closeout.

fn main() {
    println!("cargo:rerun-if-env-changed=DEP_GRAPHREACT_CPYTHON_PYTHON_RPATH");
    println!("cargo:rerun-if-env-changed=DEP_GRAPHREACT_CPYTHON_PYTHON_3_12");
    println!("cargo:rerun-if-env-changed=DEP_GRAPHREACT_CPYTHON_PYTHON_FREE_THREADED");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(python_3_12)");
    println!("cargo::rustc-check-cfg=cfg(python_free_threaded)");

    // engine-cpython's build.rs runs first (we depend on it) and
    // emits `cargo:python-rpath=<path>`. Cargo surfaces it as
    // `DEP_GRAPHREACT_CPYTHON_PYTHON_RPATH`. Skip the rpath bake when
    // the engine-cpython build script didn't run (which can happen on
    // non-macOS or when the dep tree is lopped) — that's a no-op
    // rather than a hard error.
    if let Ok(rpath) = std::env::var("DEP_GRAPHREACT_CPYTHON_PYTHON_RPATH") {
        if !rpath.is_empty() {
            // `cargo:rustc-link-arg` covers ALL linkable targets in
            // this crate (bins, tests, benches, examples).
            println!("cargo:rustc-link-arg=-Wl,-rpath,{rpath}");
        }
    }

    if let Ok(flag) = std::env::var("DEP_GRAPHREACT_CPYTHON_PYTHON_3_12") {
        if flag == "1" {
            println!("cargo:rustc-cfg=python_3_12");
        }
    }

    if let Ok(flag) = std::env::var("DEP_GRAPHREACT_CPYTHON_PYTHON_FREE_THREADED") {
        if flag == "1" {
            println!("cargo:rustc-cfg=python_free_threaded");
        }
    }
}
