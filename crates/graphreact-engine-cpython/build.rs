//! Link the Zig-built CPython bridge static lib + a libpython shared
//! library. The Python target is selected at build time by the
//! `resolve_python` helper below (pkg-config-first; env-var fallback).
//!
//! **Minimum supported Python: 3.12.** PEP 684 (per-interpreter GIL,
//! 3.12+) is load-bearing for the engine — the legacy single-GIL
//! sub-interpreter model on 3.9–3.11 has known C-extension state-
//! pollution issues (`_signal`, `_asyncio`) that crash at low
//! sub-interpreter cycle counts. 3.12+ lifts those, so the build
//! refuses to link an older Python rather than silently shipping a
//! crash-prone artifact.
//!
//! Production builds — Linux, macOS-via-Homebrew, Windows-with-pkg-config —
//! resolve via `pkg-config python-3.X-embed`. Each released artifact
//! bakes a single Python ABI; a Python-3.13 build can't load a 3.12
//! libpython at runtime (CPython is per-minor-version ABI-locked), so
//! the distribution shape is per-minor-version slices: `grx-py-3.12`,
//! `grx-py-3.13`, `grx-py-3.14`, `grx-py-3.14t`. Same shape as numpy
//! wheels.
//!
//! When the resolved minor is ≥3.12, this script emits
//! `cargo:rustc-cfg=python_3_12`. The Rust + Zig bridges branch on it
//! to enable the PEP 684 per-interpreter GIL path
//! (`Py_NewInterpreterFromConfig` with `gil = OWN_GIL` instead of
//! legacy `Py_NewInterpreter`) and a real `Py_FinalizeEx` on shutdown.
//!
//! Downstream consumers (`graphreact-cli/build.rs`) read the resolved
//! rpath / include / minor through `DEP_GRAPHREACT_CPYTHON_*` env
//! vars — `links = "graphreact_cpython"` in this crate's Cargo.toml
//! makes cargo wire the metadata through.

use std::env;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug)]
struct PythonResolution {
    /// Resolved minor version (e.g. 13 for 3.13). Drives the
    /// `python_3_12` cfg flag and informs operators which Python
    /// the binary is locked to. Build refuses to proceed below 12.
    minor: u32,
    /// `Python.h` directory passed to the Zig build via the
    /// `python_include` option so `@cImport(<Python.h>)` resolves.
    include_dir: String,
    /// pkg-config path: dynamic libpython.
    libdir: String,
    /// pkg-config path: e.g. `python3.13` (or `python3.14t` for the
    /// free-threaded build — the trailing `t` is the canonical
    /// distinguishing signal between PEP 703 and GIL'd builds).
    libname: String,
    /// rpath baked into consumer binaries so dyld resolves libpython
    /// at load time without `DYLD_LIBRARY_PATH` / `LD_LIBRARY_PATH`
    /// being set in the environment.
    rpath: String,
    /// Human-readable provenance for the metadata block in the build
    /// log. Helps diagnose distribution-time linkage surprises.
    source: &'static str,
    /// Pkg-config name that resolved (e.g. `python-3.13-embed` or
    /// `python-3.14t-embed`). Surfaced to xtask zig-build so the Zig
    /// side can mirror the choice without re-probing.
    pkg_config_name: String,
    /// True when the resolved Python is a PEP 703 free-threaded build
    /// (`python3.Xt`). Detected via the trailing `t` in either the
    /// pkg-config name (`python-3.14t-embed`) or the libname
    /// (`python3.14t`). Drives the `python_free_threaded` cfg flag.
    free_threaded: bool,
}

fn main() {
    println!("cargo:rerun-if-changed=../../zig/bridge/cpython.zig");
    println!("cargo:rerun-if-changed=../../zig/bridge/build.zig");
    println!("cargo:rerun-if-env-changed=GRAPHREACT_PYTHON_VERSION");
    println!("cargo:rerun-if-env-changed=GRAPHREACT_PYTHON_PKGCONFIG");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    // Declare the cfg flags so rustc accepts `#[cfg(python_3_12)]` /
    // `#[cfg(python_free_threaded)]` without an `unexpected_cfgs`
    // lint. Each flag's value is set (or not) below, depending on
    // the resolved Python minor + ABI.
    println!("cargo::rustc-check-cfg=cfg(python_3_12)");
    println!("cargo::rustc-check-cfg=cfg(python_free_threaded)");

    // Walk up from CARGO_MANIFEST_DIR to the workspace root, then into
    // zig/bridge/zig-out/lib/.
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("manifest dir has parent")
        .to_path_buf();
    let lib_dir = workspace_root.join("zig/bridge/zig-out/lib");
    let lib_path = lib_dir.join("libgraphreact_cpython_zig.a");

    assert!(
        lib_path.exists(),
        "graphreact-engine-cpython: missing static lib at {}.\n\
         \n\
         The CPython bridge is produced by zig from `zig/bridge/cpython.zig`.\n\
         Build it via:\n\
         \n\
         \tcargo xtask zig-build --release\n\
         \n\
         Requires zig 0.16+ on $PATH. Install with `brew install zig` (macOS),\n\
         or see https://ziglang.org/download/.\n",
        lib_path.display()
    );

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=graphreact_cpython_zig");

    let res = resolve_python();
    eprintln!(
        "graphreact-engine-cpython: linking against Python 3.{minor}{thread} via {source} (rpath={rpath}, pkg_config={pkg})",
        minor = res.minor,
        thread = if res.free_threaded { "t (PEP 703 free-threaded)" } else { "" },
        source = res.source,
        rpath = res.rpath,
        pkg = res.pkg_config_name.as_str(),
    );

    println!("cargo:rustc-link-search=native={}", res.libdir);
    println!("cargo:rustc-link-lib=dylib={}", res.libname);
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", res.rpath);

    if res.minor >= 12 {
        println!("cargo:rustc-cfg=python_3_12");
    }
    if res.free_threaded {
        println!("cargo:rustc-cfg=python_free_threaded");
    }

    // Surface the resolution to downstream crates' build scripts via
    // `DEP_GRAPHREACT_CPYTHON_*` (cargo wires this through because of
    // `links = "graphreact_cpython"` in Cargo.toml). graphreact-cli's
    // build.rs reads these to bake the same rpath into the `grx`
    // binary; xtask zig-build reads them to pass `-Dpython_3_12=...`
    // / `-Dpython_include=...` / `-Dpython_free_threaded=...` to
    // `zig build` so the Zig side picks the right header set +
    // comptime branches.
    println!("cargo:python-rpath={}", res.rpath);
    println!("cargo:python-include={}", res.include_dir);
    println!("cargo:python-minor={}", res.minor);
    println!(
        "cargo:python-3-12={}",
        if res.minor >= 12 { "1" } else { "0" }
    );
    println!(
        "cargo:python-free-threaded={}",
        if res.free_threaded { "1" } else { "0" }
    );
    println!("cargo:python-source={}", res.source);
    println!("cargo:python-pkg-config-name={}", res.pkg_config_name);
}

fn resolve_python() -> PythonResolution {
    // 1) Explicit pkg-config name override — highest priority. If it
    //    fails, that's a hard error (the operator told us exactly
    //    which Python to use).
    if let Ok(name) = env::var("GRAPHREACT_PYTHON_PKGCONFIG") {
        return probe_pkg_config(&name).unwrap_or_else(|| {
            panic!(
                "GRAPHREACT_PYTHON_PKGCONFIG={name} did not resolve via pkg-config. \
                 Check that pkg-config is installed and the .pc file is on PKG_CONFIG_PATH."
            );
        });
    }

    // 2) Explicit minor-version override. Same shape: hard error on miss.
    if let Ok(ver) = env::var("GRAPHREACT_PYTHON_VERSION") {
        let name = format!("python-{ver}-embed");
        return probe_pkg_config(&name).unwrap_or_else(|| {
            panic!(
                "GRAPHREACT_PYTHON_VERSION={ver} → pkg-config {name} did not resolve. \
                 Install python@{ver} and pkg-config (e.g. `brew install python@{ver}` on macOS)."
            );
        });
    }

    // 3) Auto-probe in descending version order — prefer the newest
    //    available Python so a developer who installed 3.13 gets the
    //    PEP 684 path automatically. Free-threaded 3.14t comes first
    //    so it wins on machines that have both 3.14t and 3.14.
    //    `python3-embed` (unversioned) is intentionally NOT in this
    //    list: it would resolve on stock macOS to Apple Command Line
    //    Tools 3.9, which does not meet the 3.12+ floor; we'd rather
    //    fail with an actionable install hint.
    for name in &[
        "python-3.14t-embed",
        "python-3.14-embed",
        "python-3.13-embed",
        "python-3.12-embed",
    ] {
        if let Some(r) = probe_pkg_config(name) {
            return r;
        }
    }

    // 4) No supported Python found — fail with an actionable hint.
    //    PEP 684 (per-interpreter GIL) is load-bearing for the engine
    //    and only lifts at 3.12+; older Pythons would silently link
    //    but crash on sub-interpreter cycling.
    panic!(
        "graphreact-engine-cpython: no Python ≥3.12 found via pkg-config.\n\
         \n\
         The engine requires Python 3.12+ (PEP 684 per-interpreter GIL).\n\
         Install one of: python@3.12, python@3.13, python@3.14, or\n\
         python-freethreading (3.14t) and ensure pkg-config sees it.\n\
         \n\
         macOS:   `brew install python@3.13` (then ensure\n\
                  `$(brew --prefix)/opt/python@3.13/lib/pkgconfig` is on PKG_CONFIG_PATH)\n\
         Debian/Ubuntu: `apt-get install python3.13-dev pkg-config`\n\
         Fedora:  `dnf install python3.13-devel pkgconf-pkg-config`\n\
         Windows: install Python from python.org and ensure pkg-config is\n\
                  on PATH; or set GRAPHREACT_PYTHON_PKGCONFIG explicitly.\n\
         \n\
         Override: `GRAPHREACT_PYTHON_PKGCONFIG=python-3.13-embed cargo build`."
    );
}

fn probe_pkg_config(name: &str) -> Option<PythonResolution> {
    // Treat "pkg-config not on PATH" as "no pkg-config Python
    // resolvable" — the caller falls through to the next option
    // rather than panicking.
    let exists = Command::new("pkg-config")
        .args(["--exists", name])
        .status()
        .ok()?;
    if !exists.success() {
        return None;
    }

    let modver = run("pkg-config", &["--modversion", name])?;
    let cflags = run("pkg-config", &["--cflags", name])?;
    let libs = run("pkg-config", &["--libs", name])?;

    let minor: u32 = parse_minor(&modver)?;
    assert!(
        minor >= 12,
        "graphreact-engine-cpython: pkg-config {name} resolved Python 3.{minor}; \
         the engine requires 3.12+ (PEP 684 per-interpreter GIL). \
         Install python@3.12, python@3.13, or python@3.14 and re-run."
    );

    let include_dir = cflags
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("-I"))
        .map(str::to_string)?;

    let libdir = libs
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("-L"))
        .map(str::to_string)?;
    let libname = libs
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("-l"))
        .map(str::to_string)?;

    let rpath = libdir.clone();

    // PEP 703 free-threaded detection. Two complementary signals:
    //   1. The pkg-config name itself: free-threaded variants ship as
    //      `python-3.Xt-embed` (the trailing `t` before `-embed`).
    //   2. The libname pkg-config exposes via `--libs`:
    //      free-threaded → `python3.Xt`; GIL'd → `python3.X`.
    //
    // We accept either signal so the resolution survives small
    // packaging differences across distros (e.g. some Linux distros
    // may emit the free-threaded ABI with a slightly different .pc
    // name like `python3-freethreaded-embed`).
    let pkg_name_is_free_threaded = name
        .strip_suffix("-embed")
        .is_some_and(|stem| stem.ends_with('t'));
    let libname_is_free_threaded = libname.ends_with('t');
    let free_threaded = pkg_name_is_free_threaded || libname_is_free_threaded;

    Some(PythonResolution {
        minor,
        include_dir,
        libdir,
        libname,
        rpath,
        source: "pkg-config",
        pkg_config_name: name.to_string(),
        free_threaded,
    })
}

fn parse_minor(modver: &str) -> Option<u32> {
    // "3.13" or "3.13.13" — pull the second dotted component. CPython
    // version strings sometimes have a "t" suffix on the patch field
    // for free-threaded builds; the minor is always pure-numeric.
    modver.split('.').nth(1)?.parse::<u32>().ok()
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
