//! WebKit clone + build orchestration. **M12.3b-phase2.**
//!
//! Phase 2 deliverable: scaffolding for the cross-platform vendoring
//! track. Two subcommands:
//!
//! - `xtask webkit-clone` — shallow clone (or fetch+checkout) of
//!   WebKit at the pinned ref into `vendor/WebKit/`. Honors
//!   `--ref <commit>` and `$GRAPHREACT_WEBKIT_REF` overrides; writes
//!   `vendor/WebKit/.graphreact-stamp` recording the actual ref used.
//! - `xtask webkit-build --target <triple>` — runs cmake + ninja
//!   against `vendor/WebKit/` with the JSCOnly port flag matrix,
//!   producing `target/webkit/<triple>/<mode>/lib/libJavaScriptCore.a`
//!   (+ libWTF.a, libbmalloc.a).
//!
//! Both subcommands accept `--dry-run` for orchestration-shape
//! verification without executing the multi-hour build (or the ~2-3 GB
//! clone). The dry-run output is the exact command(s) that would be
//! invoked.
//!
//! **macOS does NOT need this.** macOS continues to use the system
//! framework + the M12.3b-phase1.5 extern-C symbol shim for
//! `JSC::VM::drainMicrotasks`. The vendoring track is the
//! Linux/Windows path. Phase 4 wires the vendored libJSC into
//! `graphreact-engine-jsc` for those targets and rewrites the C++
//! drain shim against the vendored WebKit headers.
//!
//! **Phase 2 scope is the orchestration code itself.** The actual
//! clone (10+ minutes, ~2-3 GB) + first build (multi-hour) happens at
//! phase 3 kickoff on the appropriate host. This file's tests + a
//! smoke `--dry-run` invocation are the phase 2 acceptance gate.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------
// Pinned ref
// ---------------------------------------------------------------------

/// Upstream WebKit repository. We use upstream rather than Bun's fork
/// (oven-sh/WebKit) because we don't need the bun-specific
/// `USE_BUN_JSC_ADDITIONS` / `USE_BUN_EVENT_LOOP` patches; the JSCOnly
/// port from upstream is sufficient.
pub const WEBKIT_REPO: &str = "https://github.com/WebKit/WebKit.git";

/// Default pinned WebKit ref. **Placeholder until M12.3b-phase3
/// kickoff** — phase 3 chooses a specific commit verified to build
/// cleanly across all five target triples and updates this constant.
/// Until then, override at clone time via `--ref <commit>` or the
/// `GRAPHREACT_WEBKIT_REF` environment variable.
///
/// We use `main` as the default (rather than a SHA) so the scaffolding
/// works against current upstream during phase 2 testing, but the
/// stamp written to `vendor/WebKit/.graphreact-stamp` records the
/// actual resolved commit so reproducibility is preserved within a
/// single clone.
pub const WEBKIT_REF_DEFAULT: &str = "main";

// ---------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------

/// Target triples we orchestrate WebKit builds for. macOS targets are
/// included for parity but are NOT used for `grx-jsc` — macOS uses
/// the system framework. They exist so a future workflow can
/// cross-build a `grx-jsc-vendored` artifact for benchmarking against
/// the system-framework variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    DarwinArm64,
    DarwinX86_64,
    LinuxX86_64,
    LinuxArm64,
    WindowsX86_64,
}

impl Target {
    pub fn from_triple(s: &str) -> Result<Self, String> {
        match s {
            "darwin-arm64" | "aarch64-apple-darwin" => Ok(Self::DarwinArm64),
            "darwin-x86_64" | "x86_64-apple-darwin" => Ok(Self::DarwinX86_64),
            "linux-x86_64" | "x86_64-unknown-linux-gnu" => Ok(Self::LinuxX86_64),
            "linux-arm64" | "aarch64-unknown-linux-gnu" => Ok(Self::LinuxArm64),
            "windows-x86_64" | "x86_64-pc-windows-msvc" => Ok(Self::WindowsX86_64),
            other => Err(format!(
                "unknown target triple `{other}`. \
                 Supported: darwin-arm64, darwin-x86_64, linux-x86_64, \
                 linux-arm64, windows-x86_64."
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DarwinArm64 => "darwin-arm64",
            Self::DarwinX86_64 => "darwin-x86_64",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxArm64 => "linux-arm64",
            Self::WindowsX86_64 => "windows-x86_64",
        }
    }

    pub fn is_windows(self) -> bool {
        matches!(self, Self::WindowsX86_64)
    }

    pub fn cmake_system_name(self) -> &'static str {
        match self {
            Self::DarwinArm64 | Self::DarwinX86_64 => "Darwin",
            Self::LinuxX86_64 | Self::LinuxArm64 => "Linux",
            Self::WindowsX86_64 => "Windows",
        }
    }

    pub fn cmake_system_processor(self) -> &'static str {
        match self {
            Self::DarwinArm64 | Self::LinuxArm64 => "aarch64",
            Self::DarwinX86_64 | Self::LinuxX86_64 | Self::WindowsX86_64 => "x86_64",
        }
    }
}

// ---------------------------------------------------------------------
// Build mode
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Release,
    Debug,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Debug => "debug",
        }
    }

    pub fn cmake_build_type(self) -> &'static str {
        match self {
            Self::Release => "Release",
            Self::Debug => "Debug",
        }
    }
}

// ---------------------------------------------------------------------
// Flag matrix
// ---------------------------------------------------------------------

/// JSCOnly port + flag matrix shared across all targets. Mirrors
/// (a subset of) Bun's `scripts/build/deps/webkit.ts` choices: `PORT`,
/// `ENABLE_STATIC_JSC`, `USE_THIN_ARCHIVES=OFF`, FTL JIT on, remote
/// inspector on, media subsystems off (we don't need them for the
/// runtime).
///
/// Returned as `(KEY, VALUE)` pairs; caller turns them into `-DK=V`
/// args at command-build time.
#[must_use]
pub fn cmake_flags(target: Target, mode: Mode) -> Vec<(String, String)> {
    let mut flags: Vec<(String, String)> = vec![
        ("PORT".into(), "JSCOnly".into()),
        ("ENABLE_STATIC_JSC".into(), "ON".into()),
        ("USE_THIN_ARCHIVES".into(), "OFF".into()),
        ("ENABLE_FTL_JIT".into(), "ON".into()),
        ("ENABLE_REMOTE_INSPECTOR".into(), "ON".into()),
        ("ENABLE_API_TESTS".into(), "OFF".into()),
        ("ENABLE_MEDIA_SOURCE".into(), "OFF".into()),
        ("ENABLE_MEDIA_STREAM".into(), "OFF".into()),
        ("ENABLE_WEB_RTC".into(), "OFF".into()),
        ("CMAKE_EXPORT_COMPILE_COMMANDS".into(), "ON".into()),
        (
            "CMAKE_BUILD_TYPE".into(),
            mode.cmake_build_type().into(),
        ),
    ];

    // Cross-compile / target-shape hints. Native builds skip
    // CMAKE_SYSTEM_NAME (cmake auto-detects); cross-builds require it.
    // Phase 2 scaffolding: emits the flags so the cmake invocation
    // has them, but the actual cross-compile sysroot/toolchain lookup
    // is phase 3's problem (target-host pairs that don't match the
    // build host need a sysroot pointer or a cross-compile toolchain
    // file that's host/CI-shaped).
    let host_target = host_target();
    if target != host_target {
        flags.push((
            "CMAKE_SYSTEM_NAME".into(),
            target.cmake_system_name().into(),
        ));
        flags.push((
            "CMAKE_SYSTEM_PROCESSOR".into(),
            target.cmake_system_processor().into(),
        ));
    }

    if target.is_windows() {
        // Match Bun's static-CRT choice — required for static JSC
        // because everything else in the link will use static CRT too.
        flags.push((
            "CMAKE_MSVC_RUNTIME_LIBRARY".into(),
            match mode {
                Mode::Release => "MultiThreaded".into(),
                Mode::Debug => "MultiThreadedDebug".into(),
            },
        ));
    }

    flags
}

/// Returns the target this build host is currently running on.
/// Drives the native-vs-cross detection in `cmake_flags`.
#[must_use]
pub fn host_target() -> Target {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Target::DarwinArm64
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Target::DarwinX86_64
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Target::LinuxX86_64
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Target::LinuxArm64
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Target::WindowsX86_64
    } else {
        // Unsupported hosts get DarwinArm64 as a default — `cmake_flags`'s
        // cross-compile branch will then engage for any explicit target,
        // which is the conservative (more flags emitted) choice.
        Target::DarwinArm64
    }
}

// ---------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------

/// Source dir relative to the workspace root. Bun also uses
/// `vendor/WebKit/` so this matches their convention; our vendor/ is
/// .gitignored so the ~2-3 GB clone doesn't enter version control.
#[must_use]
pub fn webkit_source_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join("vendor/WebKit")
}

/// Per-target, per-mode build dir. Lives outside `vendor/WebKit/` so
/// a single source tree can be reused for multiple targets +
/// `rm -rf target/webkit/` is a clean reset that doesn't touch the
/// clone.
#[must_use]
pub fn webkit_build_dir(workspace_root: &Path, target: Target, mode: Mode) -> PathBuf {
    workspace_root
        .join("target/webkit")
        .join(target.as_str())
        .join(mode.as_str())
}

/// Where the linker finds `libJavaScriptCore.a` after a successful
/// build. Filename suffix is `.a` on posix, `.lib` on Windows. The
/// directory is `<build_dir>/lib/` (cmake's default install layout
/// under JSCOnly).
#[must_use]
pub fn libjsc_path(workspace_root: &Path, target: Target, mode: Mode) -> PathBuf {
    let suffix = if target.is_windows() { "lib" } else { "a" };
    let prefix = if target.is_windows() { "" } else { "lib" };
    webkit_build_dir(workspace_root, target, mode)
        .join("lib")
        .join(format!("{prefix}JavaScriptCore.{suffix}"))
}

// ---------------------------------------------------------------------
// Clone / build commands
// ---------------------------------------------------------------------

/// Pure ref-precedence rule. Order: explicit `--ref` flag → env-var
/// value → the compile-time default. Splitting the env-var read out
/// of the resolution rule keeps the function pure + testable without
/// touching the process env (`std::env::set_var` is `unsafe` since
/// 1.85 because it races with `getenv()` from libc on other threads —
/// and cargo test runs tests in parallel).
#[must_use]
pub fn resolve_ref_with_env(cli_ref: Option<&str>, env_ref: Option<&str>) -> String {
    if let Some(r) = cli_ref {
        return r.to_string();
    }
    if let Some(e) = env_ref {
        if !e.is_empty() {
            return e.to_string();
        }
    }
    WEBKIT_REF_DEFAULT.to_string()
}

/// Resolve which ref to clone. Reads `$GRAPHREACT_WEBKIT_REF` from the
/// process env and delegates to [`resolve_ref_with_env`].
#[must_use]
pub fn resolve_ref(cli_ref: Option<&str>) -> String {
    let env_ref = env::var("GRAPHREACT_WEBKIT_REF").ok();
    resolve_ref_with_env(cli_ref, env_ref.as_deref())
}

/// Build the git-clone command for a fresh clone. Used when the
/// `vendor/WebKit/` dir doesn't exist. Shallow (`--depth=1`) is the
/// default — full history isn't needed because we pin to a single
/// commit; phase 3 can override with `--full-history` if patch-level
/// development becomes a use case.
///
/// Returns the argv as a `Vec<String>` for both execution and
/// dry-run printing.
#[must_use]
pub fn clone_argv(repo: &str, refname: &str, dest: &Path, shallow: bool) -> Vec<String> {
    let mut v: Vec<String> = vec!["git".into(), "clone".into()];
    if shallow {
        v.push("--depth=1".into());
        v.push(format!("--branch={refname}"));
    }
    v.push(repo.to_string());
    v.push(dest.display().to_string());
    v
}

/// Build the git-fetch + checkout argv pair for an existing clone.
/// Used when `vendor/WebKit/.git/` already exists; we fetch the new
/// ref and check it out (detached HEAD is fine).
#[must_use]
pub fn fetch_and_checkout_argvs(refname: &str) -> (Vec<String>, Vec<String>) {
    let fetch: Vec<String> = vec![
        "git".into(),
        "fetch".into(),
        "origin".into(),
        refname.to_string(),
    ];
    let checkout: Vec<String> = vec![
        "git".into(),
        "checkout".into(),
        "--detach".into(),
        refname.to_string(),
    ];
    (fetch, checkout)
}

/// Build the cmake configure argv. Generates `build.ninja` under
/// `build_dir`. Caller invokes ninja afterwards to do the actual
/// compile.
#[must_use]
pub fn cmake_configure_argv(
    source_dir: &Path,
    build_dir: &Path,
    target: Target,
    mode: Mode,
) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "cmake".into(),
        "-GNinja".into(),
        "-S".into(),
        source_dir.display().to_string(),
        "-B".into(),
        build_dir.display().to_string(),
    ];
    for (k, val) in cmake_flags(target, mode) {
        v.push(format!("-D{k}={val}"));
    }
    v
}

/// Build the ninja-build argv. Targets the `jsc` cmake target which
/// produces `libJavaScriptCore.a` (+ libWTF.a + libbmalloc.a).
#[must_use]
pub fn ninja_build_argv(build_dir: &Path) -> Vec<String> {
    vec![
        "ninja".into(),
        "-C".into(),
        build_dir.display().to_string(),
        "jsc".into(),
    ]
}

// ---------------------------------------------------------------------
// Subcommand entry points
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CloneOptions {
    /// Override for the pinned ref. `None` falls back to env →
    /// `WEBKIT_REF_DEFAULT`.
    pub r#ref: Option<String>,
    /// Print the commands that would run, do not execute.
    pub dry_run: bool,
    /// Shallow clone (`--depth=1`). Default: true. Phase 3 can flip
    /// this if patch-level WebKit work demands full history.
    pub shallow: bool,
}

impl Default for CloneOptions {
    fn default() -> Self {
        Self {
            r#ref: None,
            dry_run: false,
            shallow: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub target: Target,
    pub mode: Mode,
    pub dry_run: bool,
}

/// `xtask webkit-clone` entry. Returns `Ok` if the clone succeeded
/// (or dry-run printed cleanly); `Err` with diagnostic + install hint
/// otherwise.
pub fn webkit_clone(workspace_root: &Path, opts: CloneOptions) -> Result<(), String> {
    let dest = webkit_source_dir(workspace_root);
    let resolved = resolve_ref(opts.r#ref.as_deref());

    let already_cloned = dest.join(".git").exists();
    let argvs: Vec<Vec<String>> = if already_cloned {
        let (fetch, checkout) = fetch_and_checkout_argvs(&resolved);
        vec![fetch, checkout]
    } else {
        vec![clone_argv(WEBKIT_REPO, &resolved, &dest, opts.shallow)]
    };

    if opts.dry_run {
        eprintln!(
            "xtask webkit-clone (dry-run): would {} WebKit at ref `{}` into {}",
            if already_cloned { "fetch + checkout" } else { "clone" },
            resolved,
            dest.display()
        );
        for argv in &argvs {
            eprintln!("  $ {}", argv.join(" "));
        }
        return Ok(());
    }

    // Execute. Tool detection up front so we fail fast with an
    // actionable message rather than mid-pipeline.
    detect_tool("git", "https://git-scm.com/downloads")?;

    if !already_cloned {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create vendor dir {}: {e}", parent.display()))?;
        }
    }

    for argv in &argvs {
        let cwd = if already_cloned { dest.as_path() } else { workspace_root };
        run_command(argv, cwd)?;
    }

    write_stamp(&dest, &resolved)?;
    eprintln!(
        "xtask webkit-clone: WebKit checked out at ref `{resolved}` in {}",
        dest.display()
    );
    Ok(())
}

/// `xtask webkit-build --target <triple>` entry.
pub fn webkit_build(workspace_root: &Path, opts: BuildOptions) -> Result<(), String> {
    let source_dir = webkit_source_dir(workspace_root);
    let build_dir = webkit_build_dir(workspace_root, opts.target, opts.mode);
    let configure = cmake_configure_argv(&source_dir, &build_dir, opts.target, opts.mode);
    let build = ninja_build_argv(&build_dir);

    if opts.dry_run {
        eprintln!(
            "xtask webkit-build (dry-run): target={} mode={} source={} build={}",
            opts.target.as_str(),
            opts.mode.as_str(),
            source_dir.display(),
            build_dir.display()
        );
        eprintln!("  $ {}", configure.join(" "));
        eprintln!("  $ {}", build.join(" "));
        eprintln!(
            "  → expected artifact: {}",
            libjsc_path(workspace_root, opts.target, opts.mode).display()
        );
        return Ok(());
    }

    if !source_dir.exists() {
        return Err(format!(
            "WebKit source not found at {}.\n\
             \n\
             Run `cargo xtask webkit-clone` first to clone\n\
             {} at the pinned ref.",
            source_dir.display(),
            WEBKIT_REPO,
        ));
    }

    detect_tool("cmake", "https://cmake.org/download/")?;
    detect_tool("ninja", "https://github.com/ninja-build/ninja/releases")?;

    std::fs::create_dir_all(&build_dir)
        .map_err(|e| format!("create build dir {}: {e}", build_dir.display()))?;

    run_command(&configure, workspace_root)?;
    run_command(&build, workspace_root)?;

    let lib = libjsc_path(workspace_root, opts.target, opts.mode);
    if !lib.exists() {
        return Err(format!(
            "expected libJavaScriptCore artifact at {} but it does not exist after build.\n\
             Inspect the cmake/ninja log above for build errors.",
            lib.display()
        ));
    }
    let len = lib.metadata().map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "xtask webkit-build: built {} ({} bytes) for target={} mode={}",
        lib.display(),
        len,
        opts.target.as_str(),
        opts.mode.as_str()
    );
    Ok(())
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn detect_tool(name: &str, install_url: &str) -> Result<(), String> {
    let probe = Command::new(name).arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => Ok(()),
        _ => Err(format!(
            "`{name}` is not installed or not on $PATH.\n\
             \n\
             The WebKit vendoring track (M12.3b-phase2/3) needs `{name}` to clone\n\
             and build the JSC sources. Install via your platform's\n\
             package manager (e.g. `brew install {name}` on macOS,\n\
             `apt-get install {name}` on Debian/Ubuntu) or download from\n\
             {install_url}.\n\
             \n\
             Once installed, re-run the failing xtask subcommand."
        )),
    }
}

fn run_command(argv: &[String], cwd: &Path) -> Result<(), String> {
    if argv.is_empty() {
        return Err("empty argv".into());
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
    let status = cmd
        .status()
        .map_err(|e| format!("`{}` spawn failed: {e}", argv[0]))?;
    if !status.success() {
        return Err(format!("`{}` exited with {status}", argv[0]));
    }
    Ok(())
}

fn write_stamp(dest: &Path, resolved_ref: &str) -> Result<(), String> {
    let stamp = dest.join(".graphreact-stamp");
    let body = format!(
        "# graphreact webkit clone stamp\n\
         repo={WEBKIT_REPO}\n\
         ref={resolved_ref}\n"
    );
    std::fs::write(&stamp, body)
        .map_err(|e| format!("write stamp {}: {e}", stamp.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> PathBuf {
        PathBuf::from("/tmp/graphreact-xtask-test")
    }

    #[test]
    fn target_round_trips_through_triple_strings() {
        let cases = [
            ("darwin-arm64", Target::DarwinArm64),
            ("darwin-x86_64", Target::DarwinX86_64),
            ("linux-x86_64", Target::LinuxX86_64),
            ("linux-arm64", Target::LinuxArm64),
            ("windows-x86_64", Target::WindowsX86_64),
        ];
        for (s, expected) in cases {
            assert_eq!(Target::from_triple(s).unwrap(), expected);
            assert_eq!(expected.as_str(), s);
        }
    }

    #[test]
    fn target_accepts_cargo_style_triples() {
        assert_eq!(
            Target::from_triple("aarch64-apple-darwin").unwrap(),
            Target::DarwinArm64
        );
        assert_eq!(
            Target::from_triple("x86_64-unknown-linux-gnu").unwrap(),
            Target::LinuxX86_64
        );
        assert_eq!(
            Target::from_triple("x86_64-pc-windows-msvc").unwrap(),
            Target::WindowsX86_64
        );
    }

    #[test]
    fn target_rejects_unknown_triple() {
        assert!(Target::from_triple("solaris-sparc").is_err());
    }

    #[test]
    fn cmake_flags_include_jsconly_and_static_jsc() {
        let flags = cmake_flags(Target::LinuxX86_64, Mode::Release);
        let by_key: std::collections::HashMap<String, String> = flags.into_iter().collect();
        assert_eq!(by_key.get("PORT").map(String::as_str), Some("JSCOnly"));
        assert_eq!(by_key.get("ENABLE_STATIC_JSC").map(String::as_str), Some("ON"));
        assert_eq!(by_key.get("USE_THIN_ARCHIVES").map(String::as_str), Some("OFF"));
        assert_eq!(by_key.get("ENABLE_FTL_JIT").map(String::as_str), Some("ON"));
        assert_eq!(by_key.get("CMAKE_BUILD_TYPE").map(String::as_str), Some("Release"));
        // Media subsystems off — we don't need them.
        assert_eq!(by_key.get("ENABLE_MEDIA_SOURCE").map(String::as_str), Some("OFF"));
        assert_eq!(by_key.get("ENABLE_WEB_RTC").map(String::as_str), Some("OFF"));
    }

    #[test]
    fn cmake_flags_emit_cross_system_for_non_host_target() {
        // Pick a target that's guaranteed to differ from the test
        // host. CI runs on linux-x86_64 + darwin-arm64; treat
        // windows-x86_64 as the perpetually-cross target.
        let flags = cmake_flags(Target::WindowsX86_64, Mode::Release);
        let by_key: std::collections::HashMap<String, String> = flags.into_iter().collect();
        assert_eq!(
            by_key.get("CMAKE_SYSTEM_NAME").map(String::as_str),
            Some("Windows")
        );
        assert_eq!(
            by_key.get("CMAKE_SYSTEM_PROCESSOR").map(String::as_str),
            Some("x86_64")
        );
        // Windows-specific runtime library flag.
        assert_eq!(
            by_key.get("CMAKE_MSVC_RUNTIME_LIBRARY").map(String::as_str),
            Some("MultiThreaded")
        );
    }

    #[test]
    fn cmake_flags_skip_cross_compile_for_host_target() {
        // host_target() always returns the test-host's triple; flags
        // for that target should NOT include CMAKE_SYSTEM_NAME (cmake
        // auto-detects native).
        let flags = cmake_flags(host_target(), Mode::Release);
        let by_key: std::collections::HashMap<String, String> = flags.into_iter().collect();
        assert!(
            !by_key.contains_key("CMAKE_SYSTEM_NAME"),
            "native build must omit CMAKE_SYSTEM_NAME so cmake auto-detects"
        );
    }

    #[test]
    fn debug_mode_emits_debug_build_type() {
        let flags = cmake_flags(Target::LinuxX86_64, Mode::Debug);
        let by_key: std::collections::HashMap<String, String> = flags.into_iter().collect();
        assert_eq!(by_key.get("CMAKE_BUILD_TYPE").map(String::as_str), Some("Debug"));
    }

    #[test]
    fn webkit_paths_layout_under_vendor_and_target() {
        let workspace = ws();
        assert_eq!(
            webkit_source_dir(&workspace),
            PathBuf::from("/tmp/graphreact-xtask-test/vendor/WebKit")
        );
        assert_eq!(
            webkit_build_dir(&workspace, Target::LinuxX86_64, Mode::Release),
            PathBuf::from("/tmp/graphreact-xtask-test/target/webkit/linux-x86_64/release")
        );
        assert_eq!(
            libjsc_path(&workspace, Target::LinuxX86_64, Mode::Release),
            PathBuf::from(
                "/tmp/graphreact-xtask-test/target/webkit/linux-x86_64/release/lib/libJavaScriptCore.a"
            )
        );
    }

    #[test]
    fn libjsc_path_uses_lib_extension_on_windows() {
        let path = libjsc_path(&ws(), Target::WindowsX86_64, Mode::Release);
        assert!(
            path.to_string_lossy().ends_with("lib/JavaScriptCore.lib"),
            "windows path should end in JavaScriptCore.lib (no `lib` prefix), got {}",
            path.display()
        );
    }

    #[test]
    fn resolve_ref_falls_back_to_default_when_no_cli_or_env() {
        assert_eq!(resolve_ref_with_env(None, None), WEBKIT_REF_DEFAULT);
    }

    #[test]
    fn resolve_ref_uses_env_when_no_cli() {
        assert_eq!(resolve_ref_with_env(None, Some("env-ref-abc")), "env-ref-abc");
    }

    #[test]
    fn resolve_ref_treats_empty_env_as_unset() {
        // Bash sets empty env vars when you `export X=`; treat that
        // as unset so the default still wins.
        assert_eq!(resolve_ref_with_env(None, Some("")), WEBKIT_REF_DEFAULT);
    }

    #[test]
    fn resolve_ref_prefers_explicit_cli_over_env() {
        assert_eq!(
            resolve_ref_with_env(Some("CLI_REF"), Some("ENV_REF")),
            "CLI_REF"
        );
    }

    #[test]
    fn clone_argv_shallow_pins_branch() {
        let argv = clone_argv(
            "https://example.com/WebKit.git",
            "abc123",
            Path::new("/tmp/dest"),
            true,
        );
        assert_eq!(argv[0], "git");
        assert_eq!(argv[1], "clone");
        assert!(argv.contains(&"--depth=1".to_string()));
        assert!(argv.contains(&"--branch=abc123".to_string()));
    }

    #[test]
    fn clone_argv_full_history_drops_depth_flags() {
        let argv = clone_argv(
            "https://example.com/WebKit.git",
            "abc123",
            Path::new("/tmp/dest"),
            false,
        );
        assert!(!argv.iter().any(|a| a.starts_with("--depth")));
        assert!(!argv.iter().any(|a| a.starts_with("--branch")));
    }

    #[test]
    fn fetch_and_checkout_round_trip_use_detached_head() {
        let (fetch, checkout) = fetch_and_checkout_argvs("ref-xyz");
        assert_eq!(fetch, vec!["git", "fetch", "origin", "ref-xyz"]);
        assert_eq!(
            checkout,
            vec!["git", "checkout", "--detach", "ref-xyz"]
        );
    }

    #[test]
    fn cmake_configure_argv_uses_ninja_generator_and_emits_d_flags() {
        let argv = cmake_configure_argv(
            Path::new("/tmp/src"),
            Path::new("/tmp/build"),
            Target::LinuxX86_64,
            Mode::Release,
        );
        assert_eq!(argv[0], "cmake");
        assert!(argv.iter().any(|a| a == "-GNinja"));
        assert!(argv.iter().any(|a| a == "-DPORT=JSCOnly"));
        assert!(argv.iter().any(|a| a == "-DENABLE_STATIC_JSC=ON"));
        assert!(argv.iter().any(|a| a == "-DCMAKE_BUILD_TYPE=Release"));
    }

    #[test]
    fn ninja_build_argv_targets_jsc() {
        let argv = ninja_build_argv(Path::new("/tmp/build"));
        assert_eq!(argv, vec!["ninja", "-C", "/tmp/build", "jsc"]);
    }
}
