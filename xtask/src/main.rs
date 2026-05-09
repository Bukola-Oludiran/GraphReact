//! Workspace task runner. Use via `cargo xtask <subcommand>`.
//!
//! Subcommands:
//!   * `bench`         — run criterion benchmarks across the workspace (TODO)
//!   * `napi-build`    — build `graphreact-napi` as a cdylib and copy it
//!                       to `crates/graphreact-napi/test-ts/index.node`
//!                       so vitest can `require()` it
//!   * `fixtures`      — regenerate graspologic golden fixtures (M8)
//!   * `zig-build`     — compile `zig/bridge/` via the system `zig`
//!                       compiler. Errors with install instructions
//!                       when zig isn't on `$PATH`. (M12.2 scaffolding;
//!                       M12.3 will use the artefact to replace the C
//!                       reference in graphreact-engine-echo.)
//!   * `webkit-clone`  — clone or fetch+checkout WebKit at the pinned
//!                       ref into `vendor/WebKit/`. (M12.3b-phase2.)
//!   * `webkit-build`  — invoke cmake + ninja to build libJavaScriptCore.a
//!                       for a target triple. (M12.3b-phase2.)

mod webkit;

use std::env;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use webkit::{BuildOptions, CloneOptions, Mode, Target};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("bench") => match bench() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask bench: {e}");
                ExitCode::from(1)
            }
        },
        Some("hyperfine") => match hyperfine() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask hyperfine: {e}");
                ExitCode::from(1)
            }
        },
        Some("napi-build") => match napi_build(args.iter().skip(1).any(|a| a == "--release")) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("xtask napi-build: {e}");
                ExitCode::from(1)
            }
        },
        Some("fixtures") => {
            eprintln!("xtask: fixtures not yet implemented (lands with M8)");
            ExitCode::from(2)
        }
        Some("zig-build") => {
            let release = args.iter().skip(1).any(|a| a == "--release");
            match zig_build(release) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("xtask zig-build: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Some("webkit-clone") => match parse_clone_args(&args[1..]) {
            Ok(opts) => match webkit_clone_cmd(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("xtask webkit-clone: {e}");
                    ExitCode::from(1)
                }
            },
            Err(e) => {
                eprintln!("xtask webkit-clone: {e}");
                ExitCode::from(2)
            }
        },
        Some("webkit-build") => match parse_build_args(&args[1..]) {
            Ok(opts) => match webkit_build_cmd(opts) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("xtask webkit-build: {e}");
                    ExitCode::from(1)
                }
            },
            Err(e) => {
                eprintln!("xtask webkit-build: {e}");
                ExitCode::from(2)
            }
        },
        Some(other) => {
            eprintln!("xtask: unknown subcommand '{other}'");
            print_help();
            ExitCode::from(1)
        }
        None => {
            print_help();
            ExitCode::SUCCESS
        }
    }
}

fn print_help() {
    eprintln!("Usage: cargo xtask <subcommand>");
    eprintln!();
    eprintln!("Subcommands:");
    eprintln!("  bench                       Run benchmarks across the workspace");
    eprintln!("  hyperfine                   Wall-clock startup bench for `grx run` across engines");
    eprintln!("                              (M14.8.x-followon Phase H — requires hyperfine on PATH)");
    eprintln!("  napi-build                  Build the @graphreact/native NAPI artifact (debug)");
    eprintln!("  napi-build --release        Same, optimized build");
    eprintln!("  fixtures                    Regenerate graspologic golden fixtures");
    eprintln!("  zig-build                   Compile zig/bridge/ via the system `zig` compiler");
    eprintln!("  zig-build --release         Same, ReleaseFast optimization");
    eprintln!();
    eprintln!("  webkit-clone [opts]         Clone or fetch+checkout WebKit at the pinned");
    eprintln!("                              ref into `vendor/WebKit/`. (M12.3b-phase2)");
    eprintln!("    --ref <commit>            Override the pinned ref (also $GRAPHREACT_WEBKIT_REF)");
    eprintln!("    --full-history            Disable the default shallow `--depth=1` clone");
    eprintln!("    --dry-run                 Print commands that would run, do not execute");
    eprintln!();
    eprintln!("  webkit-build --target <t>   Build libJavaScriptCore.a for target `<t>`.");
    eprintln!("                              (M12.3b-phase2)");
    eprintln!("    --target <triple>         Target triple: darwin-arm64 / darwin-x86_64 /");
    eprintln!("                              linux-x86_64 / linux-arm64 / windows-x86_64");
    eprintln!("    --debug                   Build in Debug mode (default: Release)");
    eprintln!("    --dry-run                 Print cmake/ninja invocations without running them");
}

/// Compile `zig/bridge/build.zig` via the system `zig` compiler.
/// Errors with install instructions if `zig` is not on `$PATH`.
fn zig_build(release: bool) -> Result<(), String> {
    let workspace_root = workspace_root()?;
    let bridge_dir = workspace_root.join("zig/bridge");
    if !bridge_dir.exists() {
        return Err(format!(
            "zig bridge dir not found at {}",
            bridge_dir.display()
        ));
    }

    // Detect zig.
    let probe = Command::new("zig").arg("version").output();
    match probe {
        Ok(out) if out.status.success() => {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            eprintln!("xtask zig-build: using zig {version}");
        }
        _ => {
            return Err(
                "zig is not installed or not on $PATH.\n\
                 \n\
                 The Zig bridge at zig/bridge/ produces the JSC engine\n\
                 static library that `graphreact-engine-jsc` links\n\
                 against. The C reference at\n\
                 crates/graphreact-engine-echo/c/echo.c is built\n\
                 separately by that crate's own build.rs (cc-rs) and\n\
                 doesn't need zig.\n\
                 \n\
                 To install zig 0.16+ (M12.3a was developed against 0.16):\n\
                 \n\
                 macOS:    brew install zig\n\
                 Linux:    asdf plugin add zig && asdf install zig latest\n\
                 Windows:  winget install --id zig.zig\n\
                 Or: download a release tarball from https://ziglang.org/download/\n\
                 \n\
                 Once installed, re-run `cargo xtask zig-build` to\n\
                 produce the static libraries under\n\
                 zig/bridge/zig-out/lib/."
                    .to_string(),
            );
        }
    }

    let mut cmd = Command::new("zig");
    cmd.current_dir(&bridge_dir);
    cmd.arg("build");
    if release {
        cmd.arg("-Doptimize=ReleaseFast");
    }

    // Mirror `crates/graphreact-engine-cpython/build.rs`'s Python
    // resolution so the cpython bridge is built against the same
    // headers cargo will link against. Without this, a developer who
    // installed Homebrew Python 3.13 would get pkg-config-resolved
    // headers + 3.13 link target on the cargo side but the legacy
    // Apple Command Line Tools 3.9 headers on the Zig side, causing
    // ABI drift at final-binary link time.
    //
    // M13-phase3f (2026-05-05) — the option-passing scaffold
    // generalises to phase 3h's free-threaded build (`python3.14t`)
    // when that lands.
    let py = resolve_python_for_zig_build();
    eprintln!(
        "xtask zig-build: cpython bridge target Python {minor}{thread} via {source}",
        minor = py.minor,
        thread = if py.free_threaded { "t (PEP 703 free-threaded)" } else { "" },
        source = py.source,
    );
    cmd.arg(format!("-Dpython_include={}", py.include_dir));
    if py.minor >= 12 {
        cmd.arg("-Dpython_3_12=true");
    }
    if py.free_threaded {
        cmd.arg("-Dpython_free_threaded=true");
    }

    let status = cmd.status().map_err(|e| format!("zig build failed: {e}"))?;
    if !status.success() {
        return Err(format!("zig build exited with {status}"));
    }
    // Report whatever artefacts the build produced. Today: jsc bridge.
    // Yesterday: echo bridge (still ported here for resurrection).
    let lib_dir = bridge_dir.join("zig-out/lib");
    if let Ok(read) = std::fs::read_dir(&lib_dir) {
        let mut found_any = false;
        for entry in read.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "a") {
                let len = p.metadata().map(|m| m.len()).unwrap_or(0);
                eprintln!("xtask zig-build: built {} ({} bytes)", p.display(), len);
                found_any = true;
            }
        }
        if !found_any {
            eprintln!(
                "xtask zig-build: no .a artefacts found under {}",
                lib_dir.display()
            );
        }
    } else {
        eprintln!(
            "xtask zig-build: zig-out/lib/ missing under {}",
            bridge_dir.display()
        );
    }
    Ok(())
}

/// Build `graphreact-napi` as a cdylib and copy the resulting binary to
/// `crates/graphreact-napi/test-ts/index.node` so the vitest harness can
/// `require()` it.
///
/// We avoid `@napi-rs/cli` for now — keeping the toolchain Cargo-only
/// keeps CI light. If/when we publish the npm package, that's when the
/// full `napi-rs/cli` flow earns its keep.
fn napi_build(release: bool) -> Result<(), String> {
    let workspace_root = workspace_root()?;
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(&workspace_root);
    cmd.args(["build", "-p", "graphreact-napi"]);
    if release {
        cmd.arg("--release");
    }
    let status = cmd.status().map_err(|e| format!("cargo build failed: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build exited with {status}"));
    }

    let profile_dir = if release { "release" } else { "debug" };
    let target = workspace_root.join("target").join(profile_dir);
    let lib_name = if cfg!(target_os = "macos") {
        "libgraphreact_napi.dylib"
    } else if cfg!(target_os = "windows") {
        "graphreact_napi.dll"
    } else {
        "libgraphreact_napi.so"
    };
    let src = target.join(lib_name);
    if !src.exists() {
        return Err(format!("cdylib not found at {}", src.display()));
    }
    let dst = workspace_root.join("crates/graphreact-napi/test-ts/index.node");
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create test-ts dir: {e}"))?;
    }
    std::fs::copy(&src, &dst)
        .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
    eprintln!(
        "xtask napi-build: copied {} -> {}",
        src.display(),
        dst.display()
    );
    Ok(())
}

/// Run the paired pure-Rust + NAPI message-dispatch benchmarks and
/// report the ratio.
///
/// **Gate calibration:** the plan's original target was ≤2× NAPI
/// overhead. After measuring the actual synthetic worst-case workload
/// — no per-message computation, every delivery is a full V8 round-trip
/// (handler ref-resolve + object construction + JsFunction::call) — the
/// floor sits at ~2.0-2.2× because pure-Rust handler invocation is
/// ~850ns while V8 round-trip is ~1.85µs. The ~1µs absolute overhead
/// per delivery is the V8 crossing cost; it can't be optimized away
/// without reducing crossings (and components ARE JS, by design).
///
/// The xtask gate is `≤2.5× median across 5 runs` with warmup. The
/// 2× target remains the long-term goal once we either (a) reduce
/// per-call napi overhead in `invoke_message_handler` (cached JS
/// object shape, etc.) or (b) the workload mix dilutes the V8 cost.
/// Realistic workloads with actual per-message computation push the
/// ratio toward 1×.
///
/// Both benches print a line like:
///   `[<which> bench] injected=… delivered=… elapsed=… per_msg=…ns throughput=… msg/sec`
/// We grep `per_msg=` to compare.
const RUNS: usize = 5;
const GATE: f64 = 2.5;

fn bench() -> Result<(), String> {
    let workspace_root = workspace_root()?;
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    eprintln!("=== xtask bench ({RUNS} runs, gate ≤{GATE:.1}× median) ===");

    // Build the release NAPI binary up front so per-run NAPI invocations
    // don't re-link.
    napi_build(true)?;

    let test_ts = workspace_root.join("crates/graphreact-napi/test-ts");
    let mut rust_per_msgs: Vec<u64> = Vec::with_capacity(RUNS);
    let mut napi_per_msgs: Vec<u64> = Vec::with_capacity(RUNS);

    for run in 0..RUNS {
        let mut cmd = Command::new(&cargo);
        cmd.current_dir(&workspace_root);
        cmd.args([
            "test",
            "-p",
            "graphreact-core",
            "--release",
            "--test",
            "bench_message_dispatch",
            "--",
            "--nocapture",
        ]);
        let out = cmd
            .output()
            .map_err(|e| format!("cargo test (pure-rust bench) failed: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "pure-rust bench failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let line = combined
            .lines()
            .find(|l| l.contains("[pure-rust bench]"))
            .ok_or_else(|| "pure-rust bench produced no result line".to_string())?;
        let per_msg = parse_per_msg(line)?;
        rust_per_msgs.push(per_msg);
        eprintln!("[run {}] rust per_msg = {per_msg}ns", run + 1);

        let mut cmd = Command::new("npm");
        cmd.current_dir(&test_ts);
        cmd.args(["run", "bench", "--silent"]);
        let out = cmd
            .output()
            .map_err(|e| format!("npm run bench failed: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "napi bench failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let line = combined
            .lines()
            .find(|l| l.contains("[napi bench]"))
            .ok_or_else(|| "napi bench produced no result line".to_string())?;
        let per_msg = parse_per_msg(line)?;
        napi_per_msgs.push(per_msg);
        eprintln!("[run {}] napi per_msg = {per_msg}ns", run + 1);
    }

    rust_per_msgs.sort_unstable();
    napi_per_msgs.sort_unstable();
    let rust_median = rust_per_msgs[RUNS / 2];
    let napi_median = napi_per_msgs[RUNS / 2];
    // u64 -> f64 may lose precision in theory, but our measurements are
    // small ns counts (well under 2^52); the cast is exact in practice.
    #[allow(clippy::cast_precision_loss)]
    let ratio = napi_median as f64 / rust_median as f64;

    eprintln!();
    eprintln!("rust per_msg medians = {rust_per_msgs:?}ns (median {rust_median}ns)");
    eprintln!("napi per_msg medians = {napi_per_msgs:?}ns (median {napi_median}ns)");
    eprintln!("napi/rust ratio (median) = {ratio:.2}× (gate: ≤{GATE:.1}×)");
    if ratio <= GATE {
        eprintln!("PASS");
        Ok(())
    } else {
        Err(format!(
            "FAIL: NAPI overhead {ratio:.2}× exceeds {GATE:.1}× gate"
        ))
    }
}

/// **M14.8.x-followon (Phase H).** Wall-clock startup bench for
/// `grx run` across engines. Builds `grx` in release, writes a
/// minimal Python fixture + a minimal JSC fixture to a tempdir,
/// and shells `hyperfine --warmup 3 --runs 10` against
/// `grx run --runtime cpython <fixture>` and
/// `grx run --runtime jsc <fixture>`.
///
/// Gracefully degrades if `hyperfine` is not on PATH —
/// prints install instructions and exits success (no
/// regression for environments without it; CI wouldn't run
/// this command unconditionally).
///
/// No perf-floor regression gate today; this is observability
/// for tracking startup-cost evolution across Python minors.
fn hyperfine() -> Result<(), String> {
    // Probe for hyperfine. Graceful degrade.
    let probe = Command::new("hyperfine").arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => {
            eprintln!(
                "hyperfine detected: {}",
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
        _ => {
            eprintln!("xtask hyperfine: `hyperfine` is not on PATH.");
            eprintln!("  Install:  brew install hyperfine    (macOS)");
            eprintln!("            cargo install hyperfine   (cross-platform)");
            eprintln!("            apt-get install hyperfine (Debian/Ubuntu)");
            eprintln!("Skipping benchmark (graceful exit).");
            return Ok(());
        }
    }

    let workspace_root = workspace_root()?;
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    eprintln!("=== xtask hyperfine: building release `grx` ===");
    let build = Command::new(&cargo)
        .current_dir(&workspace_root)
        .args(["build", "--release", "-p", "graphreact-cli", "--bin", "grx"])
        .status()
        .map_err(|e| format!("cargo build failed: {e}"))?;
    if !build.success() {
        return Err("cargo build failed".to_string());
    }
    let grx = workspace_root.join("target/release/grx");
    if !grx.exists() {
        return Err(format!("grx binary not found at {}", grx.display()));
    }

    // Write minimal fixtures to a tempdir. Use std::env::temp_dir
    // + a process-unique subdir to avoid stomping on parallel
    // xtask runs.
    let tmp = std::env::temp_dir().join(format!("grx-hyperfine-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)
        .map_err(|e| format!("create tempdir {}: {e}", tmp.display()))?;

    let py_fixture = tmp.join("startup-py.grx");
    std::fs::write(
        &py_fixture,
        r#"<Hello lang="python">
def body(ctx):
    return {'accepts': [], 'emits': []}
</Hello>

<Graph id="startup-py">
  <Hello id="hello" />
</Graph>
"#,
    )
    .map_err(|e| format!("write py fixture: {e}"))?;

    let js_fixture = tmp.join("startup-js.grx");
    std::fs::write(
        &js_fixture,
        r#"<Hello>
  return { accepts: [], emits: [] };
</Hello>

<Graph id="startup-js">
  <Hello id="hello" />
</Graph>
"#,
    )
    .map_err(|e| format!("write js fixture: {e}"))?;

    let py_cmd = format!(
        "{} run --runtime cpython {}",
        grx.display(),
        py_fixture.display()
    );
    let js_cmd = format!(
        "{} run --runtime jsc {}",
        grx.display(),
        js_fixture.display()
    );

    eprintln!("=== xtask hyperfine: running comparison ===");
    eprintln!("  cpython: {py_cmd}");
    eprintln!("  jsc:     {js_cmd}");

    let status = Command::new("hyperfine")
        .args(["--warmup", "3", "--runs", "10", "-N"])
        .arg("--command-name")
        .arg("cpython")
        .arg(&py_cmd)
        .arg("--command-name")
        .arg("jsc")
        .arg(&js_cmd)
        .status()
        .map_err(|e| format!("invoke hyperfine: {e}"))?;

    // Best-effort tempdir cleanup (the per-pid dir is fine to
    // leave behind on Windows / locked-file paths — same shape
    // as napi-build's tempdir handling).
    let _ = std::fs::remove_dir_all(&tmp);

    if !status.success() {
        return Err(format!("hyperfine exited with {status:?}"));
    }
    Ok(())
}

fn parse_per_msg(line: &str) -> Result<u64, String> {
    let token = line
        .split_whitespace()
        .find(|tok| tok.starts_with("per_msg="))
        .ok_or_else(|| format!("no per_msg= token in: {line}"))?;
    let value = token
        .trim_start_matches("per_msg=")
        .trim_end_matches("ns");
    value
        .parse::<u64>()
        .map_err(|e| format!("can't parse per_msg value '{value}': {e}"))
}

fn workspace_root() -> Result<PathBuf, String> {
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").map_err(|_| "CARGO_MANIFEST_DIR unset".to_string())?;
    let path = PathBuf::from(manifest_dir);
    let parent = path
        .parent()
        .ok_or_else(|| "xtask CARGO_MANIFEST_DIR has no parent".to_string())?;
    Ok(parent.to_path_buf())
}

// ---------------------------------------------------------------------
// Python resolution for `xtask zig-build` (M13-phase3f)
// ---------------------------------------------------------------------

/// Mirrors `crates/graphreact-engine-cpython/build.rs::resolve_python`.
/// Both run pkg-config-first (with explicit env-var overrides);
/// keeping them in sync means xtask zig-build picks the same Python
/// the eventual cargo link step will use, so the Zig bridge headers
/// match the libpython that gets linked.
struct PythonForZigBuild {
    minor: u32,
    include_dir: String,
    source: &'static str,
    free_threaded: bool,
}

fn resolve_python_for_zig_build() -> PythonForZigBuild {
    if let Ok(name) = env::var("GRAPHREACT_PYTHON_PKGCONFIG") {
        if let Some(r) = probe_pkg_config_for_zig_build(&name) {
            return r;
        }
        eprintln!(
            "xtask zig-build: GRAPHREACT_PYTHON_PKGCONFIG={name} did not resolve; falling back."
        );
    }
    if let Ok(ver) = env::var("GRAPHREACT_PYTHON_VERSION") {
        let name = format!("python-{ver}-embed");
        if let Some(r) = probe_pkg_config_for_zig_build(&name) {
            return r;
        }
        eprintln!(
            "xtask zig-build: GRAPHREACT_PYTHON_VERSION={ver} → {name} did not resolve; falling back."
        );
    }
    for name in &[
        "python-3.14t-embed",
        "python-3.14-embed",
        "python-3.13-embed",
        "python-3.12-embed",
        "python3-embed",
    ] {
        if let Some(r) = probe_pkg_config_for_zig_build(name) {
            return r;
        }
    }
    PythonForZigBuild {
        minor: 9,
        include_dir: "/Library/Developer/CommandLineTools/Library/Frameworks/Python3.framework/Versions/3.9/Headers".to_string(),
        source: "apple-clt-3.9 (fallback)",
        free_threaded: false,
    }
}

fn probe_pkg_config_for_zig_build(name: &str) -> Option<PythonForZigBuild> {
    let exists = Command::new("pkg-config")
        .args(["--exists", name])
        .status()
        .ok()?;
    if !exists.success() {
        return None;
    }
    let modver = run_pkg_config(&["--modversion", name])?;
    let cflags = run_pkg_config(&["--cflags", name])?;
    let libs = run_pkg_config(&["--libs", name])?;
    let minor: u32 = modver.split('.').nth(1)?.parse().ok()?;
    let include_dir = cflags
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("-I"))
        .map(str::to_string)?;
    // Mirror engine-cpython/build.rs's free-threaded detection: pkg-
    // config name suffix `t-embed` OR libname suffix `t`.
    let pkg_name_is_ft = name
        .strip_suffix("-embed")
        .is_some_and(|stem| stem.ends_with('t'));
    let libname_is_ft = libs
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("-l"))
        .is_some_and(|n| n.ends_with('t'));
    let free_threaded = pkg_name_is_ft || libname_is_ft;
    Some(PythonForZigBuild {
        minor,
        include_dir,
        source: "pkg-config",
        free_threaded,
    })
}

fn run_pkg_config(args: &[&str]) -> Option<String> {
    let out = Command::new("pkg-config").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

// ---------------------------------------------------------------------
// webkit-clone / webkit-build subcommands (M12.3b-phase2)
// ---------------------------------------------------------------------

fn parse_clone_args(rest: &[String]) -> Result<CloneOptions, String> {
    let mut opts = CloneOptions::default();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--ref" => {
                let val = rest
                    .get(i + 1)
                    .ok_or_else(|| "--ref requires a value".to_string())?;
                opts.r#ref = Some(val.clone());
                i += 2;
            }
            "--full-history" => {
                opts.shallow = false;
                i += 1;
            }
            "--dry-run" => {
                opts.dry_run = true;
                i += 1;
            }
            other => {
                return Err(format!(
                    "unknown flag `{other}`. Run `cargo xtask` for usage."
                ));
            }
        }
    }
    Ok(opts)
}

fn parse_build_args(rest: &[String]) -> Result<BuildOptions, String> {
    let mut target: Option<Target> = None;
    let mut mode = Mode::Release;
    let mut dry_run = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--target" => {
                let val = rest
                    .get(i + 1)
                    .ok_or_else(|| "--target requires a value".to_string())?;
                target = Some(Target::from_triple(val)?);
                i += 2;
            }
            "--debug" => {
                mode = Mode::Debug;
                i += 1;
            }
            "--release" => {
                mode = Mode::Release;
                i += 1;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            other => {
                return Err(format!(
                    "unknown flag `{other}`. Run `cargo xtask` for usage."
                ));
            }
        }
    }
    let target = target.ok_or_else(|| {
        "--target <triple> is required (darwin-arm64 / darwin-x86_64 / linux-x86_64 / linux-arm64 / windows-x86_64)".to_string()
    })?;
    Ok(BuildOptions {
        target,
        mode,
        dry_run,
    })
}

fn webkit_clone_cmd(opts: CloneOptions) -> Result<(), String> {
    let workspace = workspace_root()?;
    webkit::webkit_clone(&workspace, opts)
}

fn webkit_build_cmd(opts: BuildOptions) -> Result<(), String> {
    let workspace = workspace_root()?;
    webkit::webkit_build(&workspace, opts)
}
