//! Subcommand handlers. Take parsed CLI args, return either output
//! text or an exit code.

use std::fmt::Write;
use std::path::Path;

use thiserror::Error;

use graphreact_grx_parser::{
    ir::{build_ir, ir_to_json, parse_to_ir},
    multi_file::resolve_multi_file,
    Parser,
};

use graphreact_engine_zig::Engine;

use crate::cli::{Cli, InitTemplate, RuntimeArg};
use crate::engine_select::{select, EngineKind};
use crate::fmt::format_file;
use crate::runtime::{dry_mount, RuntimeError};

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parser error: {0}")]
    Parser(#[from] graphreact_grx_parser::GrxParseError),
    #[error("engine error: {0}")]
    Engine(#[from] graphreact_engine_zig::EngineError),
    #[error("runtime error: {0}")]
    Runtime(#[from] RuntimeError),
    #[error("engine selection error: {0}")]
    EngineSelect(#[from] crate::engine_select::EngineSelectError),
    #[error("serde_json error: {0}")]
    Json(#[from] serde_json::Error),
    /// `grx cluster …` error path. Surfaces handshake / cookie / TLS
    /// / dial / configuration failures from `graphreact_dist`.
    #[error("cluster error: {0}")]
    Cluster(#[from] graphreact_dist::DistError),
    /// User-supplied CLI argument that didn't validate (peer string
    /// without `@`, malformed addr, etc.).
    #[error("cluster argument error: {0}")]
    ClusterArg(String),
    /// **M-Prod sub-track #3.** `grx secrets` error path. Wraps
    /// every backend's failure surface in the same typed error so
    /// the CLI message reads the same regardless of which store
    /// produced the failure.
    #[error("secrets error: {0}")]
    Secrets(#[from] graphreact_secrets::SecretsError),
    /// **M-Prod sub-track #1 Phase A.** `grx audit verify` failure
    /// path. Wraps the [`graphreact_observer::AuditChainError`]
    /// surface so the CLI prints a uniform message for parse / I/O
    /// / tamper / sequence-gap / prev-hash-mismatch outcomes, and
    /// exits non-zero (the bin's top-level error handler maps any
    /// CommandError to ExitCode::FAILURE).
    #[error("audit verify failure: {0}")]
    AuditVerify(#[from] graphreact_observer::AuditChainError),
}

pub type CommandResult<T> = Result<T, CommandError>;

/// `grx parse` — print the AST or IR.
pub fn run_parse(path: &Path, json: bool, ast: bool) -> CommandResult<String> {
    let source = std::fs::read_to_string(path)?;
    if ast {
        let file = Parser::parse_file(&source)?;
        if json {
            let value = serde_json::to_value(serde_json::json!({
                "imports": file.imports.iter().map(|i| serde_json::json!({
                    "src": i.src,
                    "loc": { "line": i.loc.line, "column": i.loc.column },
                })).collect::<Vec<_>>(),
                "graph": file.graph.as_ref().map(|g| serde_json::json!({
                    "id": g.id,
                    "loc": { "line": g.loc.line, "column": g.loc.column },
                    "n_children": g.children.len(),
                })),
                "definitions": file.definitions.iter().map(|d| serde_json::json!({
                    "component": d.component,
                    "loc": { "line": d.loc.line, "column": d.loc.column },
                    "code_len": d.code.len(),
                })).collect::<Vec<_>>(),
            }))?;
            Ok(serde_json::to_string_pretty(&value)?)
        } else {
            Ok(summarise_ast(&file))
        }
    } else {
        let ir = parse_to_ir(&source)?;
        if json {
            Ok(serde_json::to_string_pretty(&ir_to_json(&ir))?)
        } else {
            Ok(summarise_ir(&ir))
        }
    }
}

/// `grx check` — parse + import-resolve, error if any step fails.
pub fn run_check(path: &Path) -> CommandResult<String> {
    // resolve_multi_file does parser + import walk in one shot.
    let resolved = resolve_multi_file(path)?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "✓ {} parsed cleanly",
        resolved.root_path.display()
    );
    if !resolved.imports.is_empty() {
        let _ = writeln!(out, "  resolved {} imports:", resolved.imports.len());
        for imp in &resolved.imports {
            let _ = writeln!(out, "    • {}", imp.path.display());
        }
    }
    if let Some(g) = &resolved.root.graph {
        let n_children = g.children.len();
        let _ = writeln!(
            out,
            "  graph `{}` declared at line {} with {n_children} top-level children",
            g.id, g.loc.line
        );
    } else {
        let _ = writeln!(
            out,
            "  no <Graph> block (definition-only file — fine for libraries)"
        );
    }
    Ok(out)
}

/// `grx fmt` — pretty-print canonically.
pub fn run_fmt(path: &Path, check_only: bool) -> CommandResult<FmtOutcome> {
    let source = std::fs::read_to_string(path)?;
    let file = Parser::parse_file(&source)?;
    let formatted = format_file(&file);
    if formatted == source {
        return Ok(FmtOutcome::AlreadyFormatted);
    }
    if check_only {
        Ok(FmtOutcome::WouldChange { formatted })
    } else {
        std::fs::write(path, &formatted)?;
        Ok(FmtOutcome::Wrote { formatted })
    }
}

#[derive(Debug, Clone)]
pub enum FmtOutcome {
    AlreadyFormatted,
    WouldChange { formatted: String },
    Wrote { formatted: String },
}

/// `grx completions <shell>` — print shell completion script to stdout.
pub fn run_completions(shell: clap_complete::Shell) -> CommandResult<String> {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut cmd, "grx", &mut buf);
    Ok(String::from_utf8(buf).expect("clap_complete writes valid UTF-8"))
}

/// `grx init <path>` — scaffold a new `.grx` project from a template.
pub fn run_init(path: &Path, template: InitTemplate) -> CommandResult<String> {
    if path.exists() {
        return Err(CommandError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("path `{}` already exists; refusing to overwrite", path.display()),
        )));
    }
    std::fs::create_dir_all(path)?;
    let (filename, contents) = render_template(template);
    std::fs::write(path.join(filename), contents)?;
    std::fs::write(path.join("README.md"), render_readme(template, path))?;
    let mut out = String::new();
    let _ = writeln!(out, "scaffolded `{}` from `{template:?}` template", path.display());
    let _ = writeln!(out, "  • {}", filename);
    let _ = writeln!(out, "  • README.md");
    let _ = writeln!(out);
    let _ = writeln!(out, "next steps:");
    let _ = writeln!(out, "  cd {}", path.display());
    let _ = writeln!(out, "  grx check {}", filename);
    let _ = writeln!(out, "  grx run {}                   # null engine (topology dry-run)", filename);
    let _ = writeln!(out, "  grx run {} --runtime jsc     # once M12.3 lands", filename);
    Ok(out)
}

const HELLO_TEMPLATE: &str = r#"<Graph id="hello">
  <NodeType id="Greeter" component="Greeter" />
</Graph>

<Greeter>
  // Component body. Real engine (JSC, M12.3) will execute this.
  return {
    accepts: ["*"],
    emits: ["*"],
    init: () => { /* TODO */ },
  };
</Greeter>
"#;

const PINGPONG_TEMPLATE: &str = r#"<Graph id="ping-pong">
  <NodeType id="A" component="Pinger" />
  <NodeType id="B" component="Ponger" />
  <edge source="A" target="B" />
  <edge source="B" target="A" />
</Graph>

<Pinger>
  return {
    accepts: ["pong"],
    emits: ["ping"],
    init: () => { /* TODO: send first ping on mount */ },
  };
</Pinger>

<Ponger>
  return {
    accepts: ["ping"],
    emits: ["pong"],
    init: () => { /* TODO */ },
  };
</Ponger>
"#;

fn render_template(template: InitTemplate) -> (&'static str, &'static str) {
    match template {
        InitTemplate::Hello => ("hello.grx", HELLO_TEMPLATE),
        InitTemplate::PingPong => ("ping-pong.grx", PINGPONG_TEMPLATE),
    }
}

fn render_readme(template: InitTemplate, path: &Path) -> String {
    let (filename, _) = render_template(template);
    let project = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    format!(
        "# {project}\n\n\
         A GraphReact `.grx` project scaffolded from the `{template:?}` template.\n\n\
         ## Run\n\n\
         ```\n\
         grx check {filename}\n\
         grx run {filename}\n\
         ```\n\n\
         The null engine mounts the topology without executing component bodies. \
         Once M12.3 (JSC engine) ships, run `grx run {filename} --runtime jsc` for end-to-end execution.\n"
    )
}

/// Render a parser error with an inline source-snippet pointing at the
/// failing line/column. Used by the binary's top-level error handler.
/// Falls back to the plain error message when the error has no source
/// location, or when reading the source fails.
#[must_use]
pub fn format_parse_error(err: &graphreact_grx_parser::GrxParseError, path: &Path) -> String {
    let header = format!("error: {}\n  --> {}", err, path.display());
    let Some(loc) = err.source_loc() else {
        return header;
    };
    let Ok(source) = std::fs::read_to_string(path) else {
        return header;
    };
    let Some(line_text) = source.lines().nth(loc.line.saturating_sub(1)) else {
        return header;
    };
    let line_num = loc.line.to_string();
    let gutter_width = line_num.len();
    let caret_offset = loc.column.saturating_sub(1);
    let caret_indent = " ".repeat(caret_offset);
    format!(
        "{header}\n   {empty_gutter} |\n   {line_num} | {line_text}\n   {empty_gutter} | {caret_indent}^",
        empty_gutter = " ".repeat(gutter_width),
    )
}

/// `grx version` — engine + parser + CLI.
pub fn run_version() -> CommandResult<String> {
    let null_engine = graphreact_engine_null::NullEngine::new();
    let echo_engine = graphreact_engine_echo::EchoEngine::new();
    let jsc_line = jsc_version_line();
    let cpython_line = cpython_version_line();
    let signature_line = signature_url_line();
    Ok(format!(
        "grx {cli}\n\
         parser  {parser}\n\
         engine-trait  {engine_trait}\n\
         engine-null   {engine_null}\n\
         engine-echo   {engine_echo}\n\
         {jsc_line}\n\
         {cpython_line}\n\
         {signature_line}",
        cli = env!("CARGO_PKG_VERSION"),
        parser = env!("CARGO_PKG_VERSION"),
        engine_trait = env!("CARGO_PKG_VERSION"),
        engine_null = null_engine.engine_version(),
        engine_echo = echo_engine.engine_version(),
    ))
}

/// **M-Prod sub-track #7** — render the operator-verifiable signature URL
/// when it was baked in at build time (release pipeline sets
/// `GRX_SIGNATURE_URL`), or render nothing when absent / empty. The
/// build.rs unconditionally emits `cargo:rustc-env=GRX_SIGNATURE_URL=...`
/// (empty string when the env var was unset), so `option_env!()` returns
/// `Some(s)` and we branch on `s.is_empty()`. Returning the empty string
/// when unset means the `format!()` placeholder collapses to a trailing
/// newline-less zero-width slot — matches the contract documented in
/// `docs/backend/m-prod-supply-chain.md` (absent ⇒ no line at all).
///
/// Output shape when set: `signature: <URL>\n` (trailing newline so it
/// matches the rest of the version output and downstream parsers don't
/// fail on a missing final newline).
fn signature_url_line() -> String {
    match option_env!("GRX_SIGNATURE_URL") {
        Some(url) if !url.is_empty() => format!("signature: {url}\n"),
        _ => String::new(),
    }
}

#[cfg(target_os = "macos")]
fn cpython_version_line() -> String {
    // Hard-code the version string for the same reason as
    // `jsc_version_line`: Python is process-singleton and we don't
    // want `grx version` to drag the main interpreter up.
    "engine-cpython cpython-0.1.0-m13-phase3h (Python is resolved at build time \
     via pkg-config — `python-3.X-embed` or `python-3.Xt-embed`, newest wins; \
     GRAPHREACT_PYTHON_VERSION / GRAPHREACT_PYTHON_PKGCONFIG env-var overrides \
     supported; build floor is Python 3.12 (PEP 684 per-interpreter GIL). \
     M13-phase3h: PEP 703 free-threaded build linkage. When the \
     resolved Python is a free-threaded build (`python3.Xt` — detected via the \
     trailing `t` in pkg-config name or libname) the build script emits \
     --cfg python_free_threaded + cargo:python-free-threaded=1, xtask zig-build \
     passes -Dpython_free_threaded=true to `zig build`, and the engine version \
     string gains a ` free-threaded` annotation at init time. **No runtime \
     path changes** — PEP 703 removes the GIL but doesn't change any C-API \
     entry point we use; concurrent isolate execution requires Zig-side state \
     locking which is a separable refactor. The cfg flag flows through to the \
     Rust runtime layer so future code paths that DO want per-isolate \
     parallelism can branch on it. New #[cfg(python_free_threaded)] runtime \
     probe verifies sys._is_gil_enabled() == False (catches distribution bugs \
     where pkg-config auto-resolved a regular Python). 3h footgun: PEP 703 \
     builds inline _Py_atomic_* helpers into Py_DECREF / PyList_Check / \
     PyList_Size macros; Zig translate-c can't inline them and the symbols \
     aren't exported. All c.Py_DECREF swapped to c.Py_DecRef (function form); \
     PyList_Check / PyList_Size elided (PyList_AsTuple errors cleanly on \
     non-lists). M13-phase3f unchanged: PEP 684 per-interpreter GIL on Python \
     3.12+. M13-phase3g unchanged: useEffectAsync via asyncio bridge. \
     M13-followon-D analogue unchanged: per-node __hostCallbacks token GC. \
     CPython is per-minor-version-and-ABI ABI-locked, so distribution is \
     per-minor + per-ABI slices (grx-py-3.12 / 3.13 / 3.14 / 3.14t — same shape \
     as numpy wheels). **M13 phase 3 series CLOSED — every M13 sub-phase \
     shipped (10/10 phase 3 + 4/4 outstanding-work plan).**)"
        .to_string()
}

#[cfg(not(target_os = "macos"))]
fn cpython_version_line() -> String {
    "engine-cpython not-linked (M13-phase3f's build.rs supports Linux + Windows \
     via pkg-config python-3.X-embed; per-OS artifact production is a parallel \
     CI track, not blocked on M13-phase3h).".to_string()
}

#[cfg(target_os = "macos")]
fn jsc_version_line() -> String {
    // Hard-code the version string. Querying the Zig side requires
    // initialising JSC, which is process-global and serialises poorly
    // when `grx version` runs alongside other JSC-touching code paths
    // (notably the integration tests). Keep version reporting cheap
    // and stateless.
    "engine-jsc    jsc-0.1.0 (M12.3b-phase1.5: drain_microtasks REAL on macOS via extern-C C++ shim against the system framework's exported _ZN3JSC2VM15drainMicrotasksEv symbol; JSContextRef→JSC::VM* via JSContextGetGroup + APICast.h-style reinterpret_cast. drain wired into JsRuntime::process_batch_async (post-batch defensive drain) + host_effect_async's await loop (select! periodic drain). 3 acceptance tests added: callable-without-panic, multi-microtask-chain, deferred-host-resolve genuine-async. Validates variant-(b) symbol-shim approach BEFORE phase2-5 commit multi-session effort to vendored WebKit. Phase1's JS Promise → Rust Future bridge carries forward unchanged. M12.3a-followon series previously closed (A+B+C+D+E+F all shipped 2026-05-02). Outstanding M12.3b: phase2 (xtask webkit-clone/build), phase3 (linux/windows binaries), phase4 (vendored-WebKit drainMicrotasks shim — Linux/Windows path), phase5 (CI matrix).)"
        .to_string()
}

#[cfg(not(target_os = "macos"))]
fn jsc_version_line() -> String {
    "engine-jsc    not-linked (M12.3b: vendored WebKit for non-macOS)".to_string()
}

#[cfg(test)]
mod debug_validation_tests {
    use super::*;
    use crate::debug::{DebugEngine, DebugFlag};

    #[test]
    fn none_passes_through() {
        let r = validate_debug_against_runtime(None, RuntimeArg::Null).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn cpython_match_accepts() {
        let d = DebugFlag {
            engine: DebugEngine::Cpython,
            port: 5678,
        };
        let r = validate_debug_against_runtime(Some(d), RuntimeArg::Cpython).unwrap();
        assert!(r.is_some());
    }

    #[test]
    fn cpython_with_null_runtime_rejected() {
        let d = DebugFlag {
            engine: DebugEngine::Cpython,
            port: 5678,
        };
        let err = validate_debug_against_runtime(Some(d), RuntimeArg::Null).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--debug requires"),
            "expected runtime-required error, got: {msg}"
        );
    }

    #[test]
    fn cpython_with_jsc_runtime_rejected() {
        let d = DebugFlag {
            engine: DebugEngine::Cpython,
            port: 5678,
        };
        let err = validate_debug_against_runtime(Some(d), RuntimeArg::Jsc).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--runtime cpython"),
            "expected mismatch error, got: {msg}"
        );
    }

    /// **M16-Backend P9 SHIPPED 2026-05-08.** JSC + `--debug=jsc:<port>`
    /// validates clean — install path is the JSC RemoteInspector via
    /// the Zig bridge, lockdownd-discovered by Safari Web Inspector.
    /// Replaces the P8-era `jsc_request_rejected_until_p9` test.
    #[test]
    fn jsc_match_accepts() {
        let d = DebugFlag {
            engine: DebugEngine::Jsc,
            port: 9229,
        };
        let r = validate_debug_against_runtime(Some(d), RuntimeArg::Jsc).unwrap();
        assert!(r.is_some());
    }

    #[test]
    fn jsc_with_cpython_runtime_rejected() {
        let d = DebugFlag {
            engine: DebugEngine::Jsc,
            port: 9229,
        };
        let err = validate_debug_against_runtime(Some(d), RuntimeArg::Cpython).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--runtime jsc"),
            "expected mismatch error naming jsc, got: {msg}"
        );
    }

    #[test]
    fn jsc_with_null_runtime_rejected() {
        let d = DebugFlag {
            engine: DebugEngine::Jsc,
            port: 9229,
        };
        let err = validate_debug_against_runtime(Some(d), RuntimeArg::Null).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--debug requires"),
            "expected runtime-required error, got: {msg}"
        );
    }
}

/// **M16-Backend P8.** Reject `--debug=<engine>` that doesn't match
/// `--runtime`. Surfacing the mismatch at dispatch time means the
/// engine isn't initialised twice and the operator sees the error
/// immediately instead of finding it in the body's traceback. Also
/// rejects JSC requests until P9 lands.
pub fn validate_debug_against_runtime(
    debug: Option<crate::debug::DebugFlag>,
    runtime: RuntimeArg,
) -> Result<Option<crate::debug::DebugFlag>, CommandError> {
    use crate::debug::DebugEngine;

    let Some(d) = debug else {
        return Ok(None);
    };
    let want_engine = match d.engine {
        DebugEngine::Cpython => "cpython",
        DebugEngine::Jsc => "jsc",
        // M17 phase 1 substrate scaffold — the flag accepts `go:2345`
        // but the engine has no Delve attach wired yet. Fail loudly
        // until M17 phase 1 closes; the message routes the operator
        // at the plan file.
        DebugEngine::Go => {
            return Err(CommandError::ClusterArg(
                "--debug=go:<port> is parser-accepted but not yet wired. \
                 The Go engine ships its Delve DAP attach in M17 phase 1. \
                 See ~/.claude/plans/graphreact-rust-later-milestones.md §M17."
                    .into(),
            ));
        }
    };
    let runtime_str = match runtime {
        RuntimeArg::Cpython => "cpython",
        RuntimeArg::Jsc => "jsc",
        // **M22.** `--debug` is rejected under `--runtime=mixed`:
        // the flag picks a single engine slot to attach against,
        // and a mixed daemon hosts multiple engines simultaneously.
        // Multi-engine attach is deferred to M22 phase 5+.
        RuntimeArg::Mixed => {
            return Err(CommandError::ClusterArg(
                "--debug is not supported under `--runtime=mixed`. Pin to a single engine \
                 via `--runtime=cpython` or `--runtime=jsc` for debugger attach (M22 phase \
                 5+ adds per-engine `--debug`)."
                    .into(),
            ));
        }
        other => {
            return Err(CommandError::ClusterArg(format!(
                "--debug requires `--runtime cpython` or `--runtime jsc`; got `{other}`"
            )));
        }
    };
    if want_engine != runtime_str {
        return Err(CommandError::ClusterArg(format!(
            "--debug=<{want_engine}> requires `--runtime {want_engine}`; got `--runtime {runtime_str}`"
        )));
    }
    Ok(Some(d))
}

/// `grx run --watch` — drives the M15 hot-reload pipeline. Returns
/// only on SIGINT or fatal error; this is the long-running variant of
/// `grx run`. Engine-restricted to JSC plus CPython (the null plus
/// echo engines have no body to swap).
///
/// **M16-Backend P8.** `debug` (when `Some`) carries a per-body
/// source-debug listener spec; `debug_wait` toggles the
/// `wait_for_client` block. The watch loop installs the debug
/// listener after the first mount; subsequent reload cycles preserve
/// the same listener (it lives on the same Python process).
pub fn run_run_watch(
    path: &Path,
    runtime: RuntimeArg,
    debug: Option<crate::debug::DebugFlag>,
    debug_wait: bool,
) -> CommandResult<()> {
    crate::watch_mode::run_watch_blocking(path.to_path_buf(), runtime, debug, debug_wait)
}

/// **M15 Phase 9.** `grx reload <path>` — stub. Remote-daemon reload
/// requires M16-Backend's management API. Today this prints a
/// redirect message and returns an error so scripts fail loudly
/// rather than silently no-op.
pub fn run_reload(path: &Path) -> CommandResult<String> {
    Err(CommandError::Io(std::io::Error::other(format!(
        "`grx reload {}` requires a running daemon plus the management API \
         (M16-Backend) which has not shipped yet. For in-process auto-reload \
         use `grx run --watch {}` — same hot-reload pipeline, runs against \
         the daemon you start in the same process.",
        path.display(),
        path.display(),
    ))))
}

/// `grx run` — parse + mount onto the selected engine.
///
/// **M16-Backend P8.** `debug` (when `Some`) installs a per-body
/// source-debug listener after the engine mounts. `debug_wait`
/// toggles `debugpy.wait_for_client()` (default: block until the
/// IDE attaches so breakpoints in `def init(ctx)` actually fire).
pub fn run_run(
    path: &Path,
    runtime: RuntimeArg,
    debug: Option<crate::debug::DebugFlag>,
    debug_wait: bool,
) -> CommandResult<String> {
    let kind = EngineKind::from(runtime);
    let report = match kind {
        // Phase 3b: JSC uses `mount_with_scheduler` — each component
        // body is a re-callable function attached to a graphreact-core
        // Scheduler; one process_batch round runs all initial mounts
        // (state preservation working via the signal registry).
        #[cfg(target_os = "macos")]
        EngineKind::Jsc => {
            // **M14.8.x-followon (Phase G).** Drive the JSC mount
            // + first batch via `process_batch_async` so
            // `useEffectAsync` effects fire under `grx run`.
            // `JsRuntime` is `!Send` (Rc<RefCell<JscEngine>>) so
            // it must run on a `tokio::task::LocalSet`. The
            // binary's outer multi-thread runtime forbids
            // nested `block_on`, so we hop to an OS thread,
            // build a current_thread runtime + LocalSet there,
            // and block on it. This is the same shape used by
            // the existing async tests in jsruntime.rs.
            //
            // **M16-Backend P9.** When `--debug=jsc:<port>` is
            // set, install the JSC RemoteInspector listener
            // BEFORE the first batch so Safari Web Inspector
            // can attach + breakpoints inside `def init` bodies
            // hit on first render. JSC has no wait-for-client
            // semantic (lockdownd discovery is silent), so
            // `--debug-no-wait` is a no-op for JSC; documented in
            // m16-debug-jsc.md.
            let path_buf = path.to_path_buf();
            let debug_install = debug;
            let _ = debug_wait; // no wait-for-client on JSC lockdownd
            let report =
                std::thread::spawn(move || -> Result<crate::runtime::DryMountReport, CommandError> {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| {
                            CommandError::Io(std::io::Error::other(format!(
                                "build current_thread runtime: {e}"
                            )))
                        })?;
                    let local = tokio::task::LocalSet::new();
                    local.block_on(&rt, async move {
                        let mut runtime = crate::jsruntime::mount_with_scheduler(&path_buf)?;
                        if let Some(d) = debug_install {
                            let mut engine = runtime.engine.borrow_mut();
                            crate::debug::jsc_inspector::install_jsc_inspector(
                                &mut engine,
                                runtime.iso,
                                d.port,
                            )
                            .map_err(|e| {
                                CommandError::ClusterArg(format!(
                                    "debug install failed: {e}"
                                ))
                            })?;
                        }
                        runtime
                            .process_batch_async()
                            .await
                            .map_err(crate::runtime::RuntimeError::from)?;
                        let report = crate::runtime::DryMountReport {
                            engine_name: "jsc".to_string(),
                            engine_version: runtime.engine.borrow().engine_version(),
                            graph_id: runtime.graph_id.clone(),
                            n_nodes: runtime.n_nodes,
                            n_edges: runtime.n_edges,
                            n_definitions: runtime.n_definitions,
                            mounted_components: runtime.mounted.clone(),
                        };
                        let _ = runtime.shutdown();
                        Ok::<_, CommandError>(report)
                    })
                })
                .join()
                .map_err(|_| {
                    CommandError::Io(std::io::Error::other(
                        "jsc localset thread panicked",
                    ))
                })??;
            report
        }
        // CPython arm. Files with `lang="python"` definitions route
        // through `pyruntime::mount_python_definitions` (which now
        // routes through `mount_with_scheduler` under the hood —
        // M13-phase3b+ shipped real hooks + scheduler integration;
        // M13-phase3c shipped use_emit + use_message). Files with no
        // Python definitions fall back to the predecessor smoke-module
        // path so existing JSC-only fixtures continue to print a
        // trailer (no regression on `grx run --runtime=cpython
        // <jsc-fixture.grx>`).
        #[cfg(target_os = "macos")]
        EngineKind::Cpython => {
            // M13-phase3a: route .grx files with `lang="python"`
            // definitions through `pyruntime::mount_python_definitions`
            // — first time grx --runtime=cpython actually mounts an
            // author-written Python body. Files with no Python
            // definitions fall back to the predecessor smoke-module
            // path so existing JSC-only fixtures still produce the
            // engine trailer (no `--runtime=cpython` regression).
            let resolved = graphreact_grx_parser::multi_file::resolve_multi_file(path)?;
            let has_python_def = resolved
                .root
                .definitions
                .iter()
                .chain(resolved.imports.iter().flat_map(|i| i.file.definitions.iter()))
                .any(|d| d.lang == graphreact_grx_parser::ast::ComponentLang::Python);

            if has_python_def {
                // **M14.8.x-followon (Phase G).** Same shape as
                // the JSC arm: drive `process_batch_async` on a
                // current_thread runtime + LocalSet hosted on
                // an OS thread (PyRuntime is `!Send` for the
                // same reasons JsRuntime is). `useEffectAsync`
                // / Python asyncio coroutines now fire under
                // `grx run --runtime cpython`.
                let path_buf = path.to_path_buf();
                let debug_install = debug;
                std::thread::spawn(
                    move || -> Result<crate::runtime::DryMountReport, CommandError> {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| {
                                CommandError::Io(std::io::Error::other(format!(
                                    "build current_thread runtime: {e}"
                                )))
                            })?;
                        let local = tokio::task::LocalSet::new();
                        local.block_on(&rt, async move {
                            // Cleanup-Pass `m16-mount-path-
                            // consolidation` (2026-06-17): always
                            // use `mount_with_scheduler` regardless
                            // of `--debug` state. Debugpy still
                            // lands BEFORE the first batch
                            // (`process_batch_async` runs only AFTER
                            // the install block below). The
                            // historical non-debug path used
                            // `mount_python_definitions` which ran
                            // a sync first batch INSIDE the mount
                            // — that batch was redundant for the
                            // dry-mount report since
                            // `process_batch_async` runs anyway,
                            // and consolidating eliminates a
                            // duplicate body-fire on the non-debug
                            // path (perf wash for any body without
                            // observable side effects on second
                            // render).
                            let mut runtime =
                                crate::pyruntime::mount_with_scheduler(&path_buf)?;
                            if let Some(d) = debug_install {
                                {
                                    let mut engine = runtime.engine.borrow_mut();
                                    crate::debug::cpython_debugpy::install_debugpy(
                                        &mut engine,
                                        runtime.iso,
                                        d.port,
                                        debug_wait,
                                    )
                                    .map_err(|e| {
                                        CommandError::ClusterArg(format!(
                                            "debug install failed: {e}"
                                        ))
                                    })?;
                                }
                            }
                            runtime
                                .process_batch_async()
                                .await
                                .map_err(crate::runtime::RuntimeError::from)?;
                            let report = crate::runtime::DryMountReport {
                                engine_name: "cpython".to_string(),
                                engine_version: runtime.engine.borrow().engine_version(),
                                graph_id: runtime.graph_id.clone(),
                                n_nodes: runtime.n_nodes,
                                n_edges: runtime.n_edges,
                                n_definitions: runtime.n_definitions,
                                mounted_components: runtime.mounted.clone(),
                            };
                            let _ = runtime.shutdown();
                            Ok::<_, CommandError>(report)
                        })
                    },
                )
                .join()
                .map_err(|_| {
                    CommandError::Io(std::io::Error::other(
                        "cpython localset thread panicked",
                    ))
                })??
            } else {
                // Pre-phase-3a smoke fallback. Lets `grx run --runtime=cpython
                // <jsc-fixture.grx>` continue to print the engine
                // trailer without erroring; useful for ops to verify
                // the Python framework is linked + the engine spins up
                // even when the fixture has no Python bodies.
                let mut engine = select(kind)?;
                let graph_block_opt = resolved.root.graph.clone();
                let graph_id = graph_block_opt
                    .as_ref()
                    .map(|g| g.id.clone())
                    .unwrap_or_else(|| "<no graph>".to_string());
                let (n_nodes, n_edges) = if let Some(g) = graph_block_opt {
                    let ir = graphreact_grx_parser::ir::build_ir(&graphreact_grx_parser::GrxFile {
                        imports: resolved.root.imports.clone(),
                        graph: Some(g),
                        subgraphs: resolved.root.subgraphs.clone(),
                        definitions: resolved.root.definitions.clone(),
                    })?;
                    (ir.nodes.len(), ir.edges.len())
                } else {
                    (0, 0)
                };
                let n_definitions = resolved.root.definitions.len()
                    + resolved
                        .imports
                        .iter()
                        .map(|i| i.file.definitions.len())
                        .sum::<usize>();

                engine.initialize().map_err(crate::runtime::RuntimeError::from)?;
                let iso = engine
                    .create_isolate(graphreact_engine_zig::IsolateOptions::default())
                    .map_err(crate::runtime::RuntimeError::from)?;
                let module = engine
                    .eval_module(
                        iso,
                        "grx_run_smoke",
                        "def smoke():\n    return {'name': 'cpython-online', 'graph': '__GRAPH__'}\n",
                    )
                    .map_err(crate::runtime::RuntimeError::from)?;
                let smoke = engine
                    .resolve_function(iso, module, "smoke")
                    .map_err(crate::runtime::RuntimeError::from)?;
                let result = engine
                    .call_function(iso, smoke, &[])
                    .map_err(crate::runtime::RuntimeError::from)?;
                let smoke_repr = format!("{result:?}");
                let report = crate::runtime::DryMountReport {
                    engine_name: "cpython".to_string(),
                    engine_version: engine.engine_version(),
                    graph_id,
                    n_nodes,
                    n_edges,
                    n_definitions,
                    mounted_components: vec![format!("[smoke] {smoke_repr}")],
                };
                let _ = engine.dispose_isolate(iso);
                let _ = engine.shutdown();
                report
            }
        }
        // **M22.** Mixed-runtime arm. Brings up every engine
        // present in the IR via mount_mixed, runs one
        // process_batch_async round so every active engine fires
        // its mount-time bodies, then shuts down in reverse order.
        // Pure-lang fixtures degenerate to a single-engine slot
        // (back-compat).
        #[cfg(target_os = "macos")]
        EngineKind::Mixed => {
            // --debug under mixed is rejected by
            // validate_debug_against_runtime; debug_wait is then
            // a no-op.
            let _ = debug;
            let _ = debug_wait;
            let path_buf = path.to_path_buf();
            let result =
                std::thread::spawn(move || -> Result<crate::runtime::DryMountReport, CommandError> {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| {
                            CommandError::Io(std::io::Error::other(format!(
                                "build current_thread runtime: {e}"
                            )))
                        })?;
                    let local = tokio::task::LocalSet::new();
                    local.block_on(&rt, async move {
                        let mut runtime = crate::mixedruntime::mount_mixed(&path_buf)?;
                        runtime
                            .process_batch_async()
                            .await
                            .map_err(crate::runtime::RuntimeError::from)?;
                        let banner = runtime.engine_version_banner();
                        let report = crate::runtime::DryMountReport {
                            engine_name: "mixed".to_string(),
                            engine_version: banner,
                            graph_id: runtime.graph_id.clone(),
                            n_nodes: runtime.n_nodes,
                            n_edges: runtime.n_edges,
                            n_definitions: runtime.n_definitions,
                            mounted_components: runtime.mounted_components(),
                        };
                        let _ = runtime.shutdown();
                        Ok::<_, CommandError>(report)
                    })
                })
                .join()
                .map_err(|_| {
                    CommandError::Io(std::io::Error::other(
                        "mixed-runtime localset thread panicked",
                    ))
                })??;
            result
        }
        // Null + echo continue using the legacy IIFE-based
        // `eval_module` path. They do no real evaluation anyway —
        // their value is exercising the trait surface, not running
        // hooks. Phase 3 hook semantics live exclusively on JSC.
        _ => {
            let mut engine = select(kind)?;
            dry_mount(path, engine.as_mut())?
        }
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "running `{}` on engine `{}` ({})",
        path.display(),
        report.engine_name,
        report.engine_version,
    );
    let _ = writeln!(
        out,
        "  graph `{}`: {} nodes, {} edges, {} definitions",
        report.graph_id, report.n_nodes, report.n_edges, report.n_definitions
    );
    if !report.mounted_components.is_empty() {
        let _ = writeln!(
            out,
            "  mounted {} components: {}",
            report.mounted_components.len(),
            report.mounted_components.join(", ")
        );
    }
    if report.engine_name == "null" {
        let _ = writeln!(
            out,
            "\n(null engine: components were not actually executed. \
             Use --runtime=jsc on macOS for the M12.3a JSC engine — \
             real eval/resolve/call + bidirectional callbacks ship in M12.3a-phase2.5; \
             real hook semantics land in M12.3a-phase3.)"
        );
    } else if report.engine_name == "cpython" {
        let _ = writeln!(
            out,
            "\n(cpython engine, M13-phase3h: PEP 703 free-threaded build linkage. \
             Build-time selector probes pkg-config for `python-3.X-embed` or \
             `python-3.Xt-embed` (newest wins; GRAPHREACT_PYTHON_VERSION / \
             GRAPHREACT_PYTHON_PKGCONFIG env-var overrides). Build floor is \
             Python 3.12 (PEP 684 per-interpreter GIL). When a free- \
             threaded build is detected (trailing `t` in pkg-config name or \
             libname `python3.Xt`) the build script emits --cfg \
             python_free_threaded + cargo:python-free-threaded=1, xtask \
             zig-build passes -Dpython_free_threaded=true to `zig build`, and \
             the engine version string gains a ` free-threaded` annotation at \
             init time. **No runtime path changes for 3h** — PEP 703 removes \
             the GIL but doesn't change any C-API entry point we use \
             (Py_InitializeEx, PyThreadState_Swap, Py_NewInterpreterFromConfig, \
             PyRun_String, PyObject_*). Concurrent isolate execution requires \
             Zig-side state locking around state.* mutations + per-isolate \
             modules/functions list mutations — separable refactor, out of \
             scope for 3h's build-flag framing. The python_free_threaded cfg \
             flag flows through to the Rust runtime layer so future code paths \
             that DO want per-isolate parallelism can branch on it without \
             another build-system pass. New #[cfg(python_free_threaded)] \
             runtime probe verifies sys._is_gil_enabled() == False — catches \
             distribution bugs where pkg-config auto-resolved a regular Python \
             despite operator intent. 3h footgun: PEP 703 builds inline \
             _Py_atomic_* helpers into Py_DECREF (split-refcount) AND the \
             PyList_Check / PyList_Size macros (atomic type-cache reads); Zig's \
             translate-c can't inline them and the helpers aren't exported \
             symbols. All c.Py_DECREF swapped to c.Py_DecRef (function form, \
             exported symbol since 3.0); PyList_Check / PyList_Size elided \
             (defensive checks — PyList_AsTuple errors cleanly on non-lists). \
             Same fix shape as M13-phase3f's Py_INCREF → Py_IncRef swap. \
             M13-phase3f unchanged: PEP 684 per-interpreter GIL on Python 3.12+ \
             (Py_NewInterpreterFromConfig with gil = OWN_GIL; real Py_FinalizeEx \
             on shutdown). M13-phase3g unchanged: useEffectAsync via asyncio \
             bridge (manual coroutine trampoline). M13-followon-D analogue \
             unchanged: per-node __hostCallbacks token GC on unmount. M13 phase \
             3 closed at 3e-γ. PyRuntime::mount_with_scheduler binds Python \
             bodies to a graphreact-core Scheduler. Author convention: \
             `body(ctx)` (preferred) or `init(ctx)` (phase 3a back-compat). \
             Distribution = per-minor + per-ABI slices (grx-py-3.12 / 3.13 / \
             3.14 / 3.14t), same shape as numpy wheels. **M13 closes here — \
             every M13 sub-phase shipped.**)"
        );
    } else if report.engine_name == "jsc" {
        let _ = writeln!(
            out,
            "\n(jsc engine, M12.3b-phase1.5: drain_microtasks REAL on macOS \
             via extern-C C++ shim that resolves the system framework's \
             exported _ZN3JSC2VM15drainMicrotasksEv symbol \
             (JSC::VM::drainMicrotasks). JSContextRef → JSC::VM* derived \
             via JSContextGetGroup + an APICast.h-style reinterpret_cast \
             (no vendored headers). drain wired into \
             JsRuntime::process_batch_async (defensive post-batch drain) \
             + host_effect_async's await loop (select! periodic drain — \
             genuine-async I/O via deferred host resolution settles \
             without phase4). Validates variant-(b) symbol-shim approach \
             before phase2-5 commit to vendored WebKit. Phase1's \
             useEffectAsync JS Promise → Rust Future bridge carries \
             forward; phase1.5 makes the manual-drain capability real. \
             `grx run` still on sync path (CLI activation deferred — \
             same as phase1). All previous surface carries forward: \
             Tier 1 (live wiring) + Tier 2 (followon-E policy-derived \
             reachability) + Tier 3 (followon-F negotiation-aware \
             planning) + 20 hooks real on JSC + all 4 source kinds + \
             edge-accept-predicate-vetoed AcceptEdge. M12.3a-followon \
             series previously CLOSED. Outstanding M12.3b: phase2-5 \
             (vendored WebKit + cross-platform binaries + Linux/Windows \
             drainMicrotasks shim + CI matrix).)"
        );
    }
    Ok(out)
}

fn summarise_ir(ir: &graphreact_grx_parser::GrxIr) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "graph: {}", ir.graph_id);
    if !ir.imports.is_empty() {
        let _ = writeln!(out, "imports ({}):", ir.imports.len());
        for i in &ir.imports {
            let _ = writeln!(out, "  • {i}");
        }
    }
    let _ = writeln!(out, "nodes ({}):", ir.nodes.len());
    for n in &ir.nodes {
        let _ = writeln!(
            out,
            "  - {} <{}> at {}:{}, depth={}, parent={}",
            n.id,
            n.component,
            n.source_loc.line,
            n.source_loc.column,
            n.depth,
            if n.parent_path.len() > 1 {
                n.parent_path[n.parent_path.len() - 2].as_str()
            } else {
                "<root>"
            }
        );
    }
    let _ = writeln!(out, "edges ({}):", ir.edges.len());
    for e in &ir.edges {
        let _ = writeln!(
            out,
            "  - {} → {} ({:?}) at {}:{}",
            e.source, e.target, e.kind, e.source_loc.line, e.source_loc.column
        );
    }
    if !ir.definitions.is_empty() {
        let _ = writeln!(out, "definitions ({}):", ir.definitions.len());
        for d in &ir.definitions {
            let _ = writeln!(out, "  • {} ({} bytes)", d.component, d.code.len());
        }
    }
    out
}

fn summarise_ast(file: &graphreact_grx_parser::GrxFile) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "ast:");
    let _ = writeln!(out, "  imports: {}", file.imports.len());
    let _ = writeln!(out, "  definitions: {}", file.definitions.len());
    if let Some(g) = &file.graph {
        let _ = writeln!(out, "  graph `{}`: {} children", g.id, g.children.len());
    } else {
        let _ = writeln!(out, "  graph: <none>");
    }
    out
}

/// Build an [`IR`] from any path. Public so integration tests can
/// reuse the conversion without going through the CLI.
pub fn build_ir_for_path(
    path: &Path,
) -> Result<graphreact_grx_parser::GrxIr, CommandError> {
    let source = std::fs::read_to_string(path)?;
    let file = Parser::parse_file(&source)?;
    Ok(build_ir(&file)?)
}

// =====================================================================
// M14.8 — `grx cluster …` handlers.
// =====================================================================
//
// Two entry shapes per subcommand:
//
//  - `start_*` returns a live `Cluster` + scheduler so tests (and
//    future ops surfaces) can drive the runtime + introspect the
//    state without blocking the test thread.
//  - `run_*_blocking` builds via `start_*` and then drives the
//    daemon loop (process_batch + sleep, or wait for SIGINT).
//
// M14.8 shipped the engine-free shape; M14.8.x lifts the deferral
// for JSC by mounting via `mount_with_scheduler` and then handing
// the resulting `JsRuntime`'s scheduler to `Cluster::start`. The
// load-bearing detail is the M14.8.x retrofit on
// `Scheduler::install_remote_group_sink`: when sinks are installed
// AFTER mount-time bodies have already called `useGroupJoin`, the
// install method synthesises Join events for every existing
// membership so `DistGroups` populates correctly. Without that
// retrofit, mount-then-cluster would silently drop the local
// memberships from the dist crate's view.
//
// CPython is structurally identical; wiring it under
// `grx cluster daemon` is a follow-on slice — same shape as the
// JSC path, just routes through `pyruntime::mount_python_definitions`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use graphreact_core::Scheduler;
use graphreact_dist::{
    Cluster, ClusterConfig, Creation, NetSplitPolicy, NetTickConfig, NodeId, SecretCookie,
    TlsConfig, RELOAD_V1_CAPABILITY,
};
use rustls::pki_types::{
    pem::PemObject, CertificateDer, PrivateKeyDer,
};

/// **M-Prod sub-track #5 safest slice.** Default upper bound on
/// the OTel `SdkTracerProvider::shutdown()` call during daemon
/// teardown.
///
/// Rationale: 5 seconds is well under typical k8s
/// `terminationGracePeriodSeconds` (30s) and well above the
/// expected drain time of an OTel BatchSpanProcessor with a healthy
/// collector (sub-second). If the collector is unreachable, the
/// SDK's internal export retry can otherwise block for the full
/// `terminationGracePeriodSeconds` budget — silent loss of any
/// SIGTERM-arrival audit-log line that would otherwise let
/// operators tell "graceful drain" apart from "SIGKILL'd by
/// kubelet".
///
/// Overridable via the `OBSERVER_SHUTDOWN_DEADLINE` env var
/// (seconds; integer; ≤0 or unparseable falls back to default).
pub const DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS: u64 = 5;

/// Read the `OBSERVER_SHUTDOWN_DEADLINE` env var (seconds) and
/// return the resulting `Duration`. Falls back to
/// [`DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS`] on missing /
/// unparseable / zero values (zero would race the OTel join to
/// instant timeout, which surfaces as a guaranteed audit-log
/// noise line; safer to fall back).
///
/// `pub(crate)` rather than private so the M-Prod test
/// `observer_shutdown_deadline_env_var` can verify the env-var
/// contract without spinning the whole daemon. `allow(dead_code)`
/// because the only production caller is gated on `feature = "otel"`,
/// but the env-var contract + tests must be reachable in every
/// feature configuration.
#[allow(dead_code)]
pub(crate) fn observer_shutdown_deadline() -> Duration {
    let secs = std::env::var("OBSERVER_SHUTDOWN_DEADLINE")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS);
    Duration::from_secs(secs)
}

/// **M-Prod sub-track #4 Phase α.** Default outer SIGTERM drain
/// deadline.
///
/// Budget math (locked against k8s default
/// `terminationGracePeriodSeconds = 30s`):
///
/// - 20s drain (this default)
/// - +5s OTel teardown ([`DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS`])
/// - +5s slack for cluster shutdown + engine teardown
/// - = 30s, fits exactly inside the k8s default.
///
/// Operators raising `OBSERVER_SHUTDOWN_DEADLINE` past 5s SHOULD
/// also raise `terminationGracePeriodSeconds` to keep the total
/// budget consistent — `start_daemon` warns at boot when
/// drain_deadline ≤ observer_shutdown_deadline (the locked
/// invariant says drain MUST be ≥ OTel teardown per
/// `docs/backend/m-prod-observability.md` cross-track contract).
///
/// Overridable via the `--drain-deadline-secs` CLI flag or
/// `GRX_DRAIN_DEADLINE_SECS` env var (integer seconds; ≤0 /
/// unparseable falls back to the default).
pub const DEFAULT_DRAIN_DEADLINE_SECS: u64 = 20;

/// **M-Prod sub-track #4 Phase α.** Resolve the effective drain
/// deadline. Precedence: explicit `--drain-deadline-secs` >
/// `GRX_DRAIN_DEADLINE_SECS` env var > [`DEFAULT_DRAIN_DEADLINE_SECS`].
///
/// Unparseable / zero values fall back to the default — the zero
/// case in particular would race the in-flight quiescence loop
/// to instant timeout and emit a guaranteed
/// `drain_deadline_exceeded` audit-log line on every clean
/// shutdown, which is noisier than just falling back.
pub fn resolve_drain_deadline(cli_secs: Option<u64>) -> Duration {
    let secs = cli_secs
        .or_else(|| {
            std::env::var("GRX_DRAIN_DEADLINE_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
        })
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_DRAIN_DEADLINE_SECS);
    Duration::from_secs(secs)
}

/// Daemon CLI knobs translated into structured form. Constructed
/// from the [`crate::cli::ClusterOp::Daemon`] variant; consumed by
/// [`start_daemon`].
pub struct DaemonOptions {
    pub path: Option<std::path::PathBuf>,
    pub node_id: NodeId,
    pub listen: SocketAddr,
    pub cookie: SecretCookie,
    pub tls: TlsConfig,
    pub peers: Vec<(NodeId, SocketAddr)>,
    pub runtime: RuntimeArg,
    pub net_tick: NetTickConfig,
    pub net_split_policy: NetSplitPolicy,
    pub duration_secs: Option<u64>,
    /// **M16-Backend P2.** Resolved observer-server bind. `None`
    /// means no observer port bound (zero-overhead default).
    /// `Some(_)` means construct + spawn the HTTP server alongside
    /// the cluster, install [`PrometheusSink`] on the scheduler +
    /// cluster, and join the server's lifetime to the daemon's.
    ///
    /// Constructed by the bin/grx translation layer via
    /// [`graphreact_observer::BindConfig::from_flags`] before
    /// `start_daemon` is called, so any flag-validation error
    /// surfaces at process start (NOT during the first scrape).
    pub observer_bind: Option<graphreact_observer::BindConfig>,
    /// **M16-Backend P8.** Per-body source-debug listener spec.
    /// `Some(_)` installs debugpy (P8) / JSC inspector (P9) on
    /// the engine matching `runtime` after first mount. `None`
    /// runs without a debug listener.
    pub debug: Option<crate::debug::DebugFlag>,
    /// **M16-Backend P8.** When `debug` is set, blocks on
    /// `debugpy.wait_for_client()` (or the JSC analogue in P9)
    /// before running the first batch. Default `true` so
    /// breakpoints in `def init(ctx)` hit on first mount; the
    /// `--debug-no-wait` CLI flag flips it.
    pub debug_wait: bool,
    /// **M16-Backend P10.** OTLP/gRPC endpoint for OpenTelemetry
    /// trace export. `Some(url)` constructs a
    /// [`graphreact_observer::OtelSink`] (under `--features otel`),
    /// installs it as both a `TelemetrySink` (via the daemon's
    /// `MultiplexSink`) AND a `WireTracingHook` (via
    /// [`graphreact_dist::Cluster::install_wire_tracing_hook`]) so
    /// outbound remote messages carry the current OTel span context
    /// in their wire packet's `trace_context` field, and receiver-
    /// side spans render cross-cluster traces with `Link`
    /// attributes. The daemon advertises the
    /// `observer-otel-v1` capability when this field is set; peers
    /// without the capability still receive trace_context bytes
    /// correctly (wire format is mandatorily v3) but their own
    /// outbound emissions don't carry it. `None` (default) skips
    /// OTel entirely; non-OTel deployments don't pay the SDK dep
    /// cost. Without `--features otel`, setting this returns
    /// `CommandError::ClusterArg` at start_daemon.
    pub observer_otel_endpoint: Option<String>,
    /// **M16-Backend P10.** OTel resource attribute `service.name`
    /// for spans emitted via `observer_otel_endpoint`. Defaults to
    /// `"graphreact"`. Per OTel SDK convention, can also be set via
    /// the `OTEL_SERVICE_NAME` environment variable.
    pub observer_otel_service_name: Option<String>,
    /// **M-Prod sub-track #1 Phase A.** When `Some(path)`, open a
    /// [`graphreact_observer::FileAuditSink`] on `path` (append-
    /// only, recovering chain head from the file's tail) and wire
    /// it through [`graphreact_observer::SpawnOptions::audit_sink`].
    /// The existing
    /// `target="graphreact::observer::audit"` tracing stream STILL
    /// fires bit-for-bit unchanged. `None` (default) skips the
    /// sink — operator + test paths preserve the legacy
    /// tracing-only audit surface.
    pub observer_audit_log: Option<std::path::PathBuf>,
    /// **M-Prod sub-track #4 Phase α.** Outer SIGTERM drain
    /// deadline (seconds). `None` falls through to
    /// `GRX_DRAIN_DEADLINE_SECS` env var, then to
    /// [`DEFAULT_DRAIN_DEADLINE_SECS`]. The drain bound is the
    /// OUTER budget; the OTel teardown bound
    /// (`OBSERVER_SHUTDOWN_DEADLINE`) runs INSIDE the drain budget
    /// so a healthy collector still gets to flush spans before
    /// SIGKILL. See `docs/backend/m-prod-lifecycle.md` for the
    /// budget math vs k8s `terminationGracePeriodSeconds`.
    pub drain_deadline_secs: Option<u64>,
}

/// `grx cluster connect` knobs. Mirrors [`DaemonOptions`] minus
/// `path`/`peers`/`runtime` — `connect` is a one-shot dial and
/// doesn't host a `.grx` mount.
pub struct ConnectOptions {
    pub peer: NodeId,
    pub peer_addr: SocketAddr,
    pub node_id: NodeId,
    pub listen: SocketAddr,
    pub cookie: SecretCookie,
    pub tls: TlsConfig,
    pub net_tick: NetTickConfig,
    pub net_split_policy: NetSplitPolicy,
    /// Run-duration cap. `None` runs indefinitely until SIGINT —
    /// symmetric with `DaemonOptions::duration_secs`. `Some(n)`
    /// exits after `n` seconds, useful in tests + scripted
    /// shutdowns.
    pub duration_secs: Option<u64>,
}

/// What kind of scheduler-driver the daemon is hosting. Engine-free
/// is a bare [`Scheduler`]; engine-hosted variants own the engine
/// runtime (`JsRuntime` for JSC; `PyRuntime` follows the same
/// shape under M14.8.x-cpython once wired) and route
/// `process_batch` through that runtime's TLS-installing entry
/// so component bodies fire correctly.
pub enum SchedulerHost {
    /// No `.grx` mount — the daemon acts as a relay for cross-node
    /// traffic. Useful for `grx cluster connect` and for tests that
    /// don't need engine hooks.
    Empty(Box<Scheduler>),
    /// JSC engine-hosted runtime. Owns the engine + isolate; calls
    /// to [`Self::process_batch`] route through
    /// [`crate::jsruntime::JsRuntime::process_batch`] so the engine
    /// TLS guards (`EngineGuard::with_directory`) are installed for
    /// the duration of body evaluation. M14.8.x.
    #[cfg(target_os = "macos")]
    Jsc(Box<crate::jsruntime::JsRuntime>),
    /// CPython engine-hosted runtime. Mirrors the JSC variant —
    /// owns the engine + isolate; routes `process_batch` through
    /// [`crate::pyruntime::PyRuntime::process_batch`] so the
    /// equivalent CPython TLS guards are installed for the
    /// duration of body evaluation. M14.8.x.
    #[cfg(target_os = "macos")]
    Cpython(Box<crate::pyruntime::PyRuntime>),
    /// **M22.** Mixed-runtime host — co-owns JSC + CPython + Go
    /// runtimes against the same `.grx` IR. `process_batch` drives
    /// every active engine slot in init order; `scheduler_mut`
    /// returns the FIRST active engine's scheduler so callers like
    /// `Cluster::start` can still install dist sinks (the dist
    /// crate's sink shape is per-Scheduler today — M22 phase 5+
    /// extends the wiring to multiplex across engines). Pure-lang
    /// fixtures fed through `--runtime=mixed` degenerate to a
    /// single-engine slot (back-compat preserved).
    #[cfg(target_os = "macos")]
    Mixed(Box<crate::mixedruntime::MixedRuntime>),
}

impl SchedulerHost {
    /// Mutable reference to the underlying [`Scheduler`]. Used by
    /// `Cluster::start` (to install dist sinks) and by tests that
    /// drive `group_broadcast` / inspect monitor state directly.
    ///
    /// **M22.** For the Mixed variant this returns the JSC slot's
    /// scheduler when present, else CPython's, else Go's. Pure-lang
    /// fixtures see the correct engine's scheduler; mixed fixtures
    /// get the JSC scheduler (dist-sink installation across
    /// multiple engines is M22 phase 5+). An empty Mixed (no engine
    /// slots — possible for a `--runtime=mixed` invocation against
    /// a definitions-empty .grx) falls through to the host's
    /// `empty_fallback` scheduler so the daemon doesn't panic; see
    /// `SchedulerHost::Mixed` field docs.
    pub fn scheduler_mut(&mut self) -> &mut Scheduler {
        match self {
            SchedulerHost::Empty(s) => s,
            #[cfg(target_os = "macos")]
            SchedulerHost::Jsc(r) => &mut r.scheduler,
            #[cfg(target_os = "macos")]
            SchedulerHost::Cpython(r) => &mut r.scheduler,
            #[cfg(target_os = "macos")]
            SchedulerHost::Mixed(m) => m.scheduler_mut(),
        }
    }

    /// Drive one `process_batch` round. For JSC / CPython this
    /// installs the engine TLS guards via the runtime's own
    /// `process_batch` entry point. **M22.** Mixed drives every
    /// active engine slot in init order.
    pub fn process_batch(&mut self) -> Result<(), graphreact_core::RuntimeError> {
        match self {
            SchedulerHost::Empty(s) => s.process_batch(),
            #[cfg(target_os = "macos")]
            SchedulerHost::Jsc(r) => r.process_batch(),
            #[cfg(target_os = "macos")]
            SchedulerHost::Cpython(r) => r.process_batch(),
            #[cfg(target_os = "macos")]
            SchedulerHost::Mixed(m) => m.process_batch(),
        }
    }
}

/// Live daemon. Exposed so tests can introspect [`Cluster::registry`]
/// without going through subprocess. Owns the [`SchedulerHost`] (boxed
/// — `!Send` + held across `await`s by the binary's blocking driver).
pub struct DaemonHandle {
    cluster: Cluster,
    host: SchedulerHost,
    /// **M16-Backend P2+P4.** Observer management-API handle when
    /// `--observer-bind` was passed. `None` means no observer port
    /// bound. [`Self::shutdown`] joins the server's task before
    /// dropping the cluster. Owns the snapshot-bridge receiver,
    /// drained by [`drive_handle_until`] after each `process_batch`
    /// round.
    observer: Option<graphreact_observer::ApiServerHandle>,
    /// **M22 verify fix.** Shared `MetricsInner` Arc parked here so
    /// `drive_handle_until` can record per-tick
    /// `grx_engine_eval_count{engine}` increments without
    /// re-threading the metrics handle through every call site.
    /// `None` when no `--observer-bind` was passed (no metrics
    /// surface to populate). Same Arc the management API serves
    /// `/v1/metrics` from, so the per-tick increments are visible
    /// on the next scrape.
    metrics: Option<std::sync::Arc<graphreact_observer::MetricsInner>>,
    /// **M16-Backend P10.** OTel `SdkTracerProvider` parked here so
    /// the daemon's shutdown can flush buffered spans via
    /// `provider.shutdown()` BEFORE the cluster tears down.
    /// `None` for non-OTel deployments. Only present under
    /// `--features otel` AND when `--observer-otel-endpoint` was
    /// passed.
    #[cfg(feature = "otel")]
    otel_provider: Option<graphreact_observer::SdkTracerProvider>,
    /// **M-Prod sub-track #4 Phase α.** Drain bit shared with the
    /// observer's [`graphreact_observer::ApiState`]. `None` when
    /// no observer was wired (drain becomes a no-op then — there's
    /// no ingress surface to refuse). `Some(flag)` when `start_daemon`
    /// constructed an observer via
    /// [`graphreact_observer::spawn_management_api_with_drain`].
    drain: Option<graphreact_observer::DrainFlag>,
    /// **M-Prod sub-track #4 Phase α.** In-flight ingress POST
    /// counter shared with the observer. Always present (cheap to
    /// construct); read by [`Self::start_drain`] to know when
    /// ingress quiesced.
    ingress_in_flight: graphreact_observer::IngressInFlight,
}

/// **M-Prod sub-track #4 Phase α.** Outcome of a graceful drain
/// triggered by [`DaemonHandle::start_drain`]. Returned to the
/// orchestrator (today: SIGTERM path in `run_daemon_blocking`;
/// future: REST `POST /v1/admin/drain`, operator `grx admin drain`)
/// so it can decide whether to escalate to hard-shutdown or wait
/// further. The fields are all observable independent of one
/// another — `in_flight_settled = false` + `cluster_quiesced =
/// true` is possible if the deadline expired while a single
/// long-running ingress call was outstanding but the cluster
/// handshake population was empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainOutcome {
    /// `true` if both [`graphreact_observer::PendingCalls::is_empty`]
    /// AND [`graphreact_observer::IngressInFlight::load`] hit zero
    /// before the deadline; `false` if the deadline expired with
    /// either non-zero.
    pub in_flight_settled: bool,
    /// `true` for Phase α unconditionally (handshake-quiescence
    /// is deferred; see `m-prod-lifecycle.md` for why). Future
    /// phases populate this from a `Cluster::handshakes_pending()`
    /// counter; today the cluster's per-peer tasks are aborted by
    /// the subsequent `cluster.shutdown()` call.
    pub cluster_quiesced: bool,
    /// Wall-clock span between [`graphreact_observer::DrainFlag::start`]
    /// returning `true` and quiescence (or deadline expiry).
    pub drain_duration: Duration,
    /// `true` when the deadline elapsed without quiescence. Mirrors
    /// the `drain_deadline_exceeded` audit event the orchestrator
    /// emitted at the same moment; included here so synchronous
    /// callers don't have to grep audit logs to know they ran out
    /// of budget.
    pub deadline_exceeded: bool,
}

impl DaemonHandle {
    #[must_use]
    pub fn cluster(&self) -> &Cluster {
        &self.cluster
    }

    /// Read the live [`Scheduler`] — for tests that want to drive
    /// `group_broadcast` manually or inspect monitor / link state.
    pub fn scheduler(&mut self) -> &mut Scheduler {
        self.host.scheduler_mut()
    }

    /// Drive one `process_batch` round through whichever
    /// [`SchedulerHost`] flavor this daemon was constructed with.
    /// Engine-free hosts go through the bare scheduler; JSC hosts
    /// route through `JsRuntime::process_batch` so engine TLS guards
    /// are installed for the duration of body evaluation.
    pub fn process_batch(&mut self) -> Result<(), graphreact_core::RuntimeError> {
        self.host.process_batch()
    }

    /// **M22 verify fix.** Read the shared `MetricsInner` Arc, if any
    /// was set up during `start_daemon` (i.e. `--observer-bind` was
    /// passed). Used by `drive_handle_until` to record per-tick
    /// `grx_engine_eval_count{engine}` increments.
    #[must_use]
    pub fn observer_metrics(&self) -> Option<&graphreact_observer::MetricsInner> {
        self.metrics.as_deref()
    }

    /// **M-Prod sub-track #4 Phase α.** Initiate a graceful drain.
    ///
    /// 1. Flip the shared [`graphreact_observer::DrainFlag`] (emits
    ///    `drain_started` audit BEFORE the AtomicBool flip, so
    ///    `/v1/readyz` scrapes racing the flip can't see
    ///    `ready=true` and `no drain_started` in the log).
    /// 2. Poll the in-flight predicates
    ///    (`ingress_in_flight.load() == 0`
    ///    AND, if an observer is wired,
    ///    `observer.pending_calls.is_empty()`) on a 25ms cadence
    ///    until quiescence or `deadline` elapses.
    /// 3. Emit `drain_completed` (success) or
    ///    `drain_deadline_exceeded` (timeout) on the
    ///    `graphreact::observer::audit` target.
    ///
    /// Idempotent — duplicate SIGTERMs / operator double-fires
    /// are no-ops past the first call: the underlying
    /// `DrainFlag::start` is CAS-gated, so the second invocation
    /// observes `false` from the gate and returns immediately
    /// with the no-op `DrainOutcome` (the orchestrator can still
    /// proceed to shutdown).
    ///
    /// Does NOT call [`Self::shutdown`] — drain leaves the
    /// daemon's runtime up so the observer can keep replying
    /// 503 `draining` to LB probes while the operator decides
    /// when to escalate. Typical wiring (SIGTERM path):
    /// `handle.start_drain(deadline).await; handle.shutdown().await`.
    ///
    /// When `self.drain` is `None` (no observer wired at boot)
    /// this is a no-op — returns immediately with
    /// `in_flight_settled: true` because there's no ingress
    /// surface to quiesce.
    pub async fn start_drain(&self, deadline: Duration) -> DrainOutcome {
        let Some(flag) = self.drain.as_ref() else {
            // No observer was wired. Drain has nothing to do —
            // the daemon's ingress surface doesn't exist. Skip
            // audit emit too: an operator would never call this
            // path unless they piped SIGTERM into a no-observer
            // daemon, and even then there's nothing to surface.
            return DrainOutcome {
                in_flight_settled: true,
                cluster_quiesced: true,
                drain_duration: Duration::ZERO,
                deadline_exceeded: false,
            };
        };
        let deadline_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX);
        // Idempotent CAS: only the FIRST call gets `true`.
        // Duplicate SIGTERMs return immediately without resetting
        // the deadline or re-emitting `drain_started`.
        if !flag.start(deadline_ms) {
            return DrainOutcome {
                in_flight_settled: true,
                cluster_quiesced: true,
                drain_duration: Duration::ZERO,
                deadline_exceeded: false,
            };
        }
        let started = std::time::Instant::now();
        let ingress_counter = self.ingress_in_flight.clone();
        let pending_calls = self.observer.as_ref().map(|o| o.pending_calls.clone());
        // Poll the in-flight predicates on a 25ms cadence. The
        // tight loop gives us sub-50ms drain quiescence for the
        // healthy fast path (no in-flight requests at SIGTERM
        // time → drain_completed fires almost immediately);
        // bounded by `deadline` for the slow path.
        let mut poll = tokio::time::interval(Duration::from_millis(25));
        poll.tick().await; // skip immediate
        let quiesced = loop {
            let ingress_done = ingress_counter.load() == 0;
            let calls_done = pending_calls
                .as_ref()
                .is_none_or(graphreact_observer::PendingCalls::is_empty);
            if ingress_done && calls_done {
                break true;
            }
            tokio::select! {
                _ = poll.tick() => {}
                () = tokio::time::sleep_until(
                    tokio::time::Instant::from_std(started + deadline)
                ) => {
                    break false;
                }
            }
        };
        let elapsed = started.elapsed();
        if quiesced {
            graphreact_observer::log_drain_completed(elapsed, true, true);
            DrainOutcome {
                in_flight_settled: true,
                // Phase α reports `cluster_quiesced: true`
                // unconditionally — handshake-quiescence is a
                // Phase β deliverable; today the cluster's
                // per-peer tasks get aborted by the subsequent
                // `cluster.shutdown()` call. See
                // `docs/backend/m-prod-lifecycle.md` for the
                // deferral rationale.
                cluster_quiesced: true,
                drain_duration: elapsed,
                deadline_exceeded: false,
            }
        } else {
            let ingress_remaining = ingress_counter.load();
            let pending_remaining = pending_calls
                .as_ref()
                .map_or(0, graphreact_observer::PendingCalls::len);
            graphreact_observer::log_drain_deadline_exceeded(
                deadline,
                elapsed,
                pending_remaining,
                ingress_remaining,
            );
            DrainOutcome {
                in_flight_settled: false,
                cluster_quiesced: true,
                drain_duration: elapsed,
                deadline_exceeded: true,
            }
        }
    }

    /// Drop the cluster and tear down the engine (if any).
    /// **M16-Backend P2:** also signals + joins the observer HTTP
    /// server (if one was spawned). **M16-Backend P10:** flushes
    /// any buffered OTel spans via `provider.shutdown()` BEFORE the
    /// cluster tears down so the collector receives the final batch.
    /// Async because the observer shutdown + OTel provider flush
    /// both await background tasks.
    ///
    /// **M-Prod sub-track #5 safest slice.** The OTel provider's
    /// `shutdown()` is bounded by the `OBSERVER_SHUTDOWN_DEADLINE`
    /// env var (seconds; default
    /// [`DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS`]). A collector
    /// that's gone away (network-partitioned, OOM-killed,
    /// node-drained ahead of us) MUST NOT hang the daemon's
    /// shutdown — k8s SIGKILLs at `terminationGracePeriodSeconds`
    /// anyway, and we want to surface the timeout in the audit
    /// log rather than be murdered silently.
    pub async fn shutdown(self) {
        // **M16-Backend P10 + M-Prod #5.** Flush the OTel span
        // batch processor FIRST so any buffered spans hit the
        // collector before the scheduler/cluster teardown drops
        // in-flight context. The SDK's `shutdown()` is synchronous
        // (blocking on the export task's join); we call it via
        // spawn_blocking to keep the async runtime cooperative,
        // AND we race the join against `OBSERVER_SHUTDOWN_DEADLINE`
        // so an unreachable collector can't wedge teardown.
        #[cfg(feature = "otel")]
        if let Some(provider) = self.otel_provider {
            let deadline = observer_shutdown_deadline();
            let join = tokio::task::spawn_blocking(move || {
                let _ = provider.shutdown();
            });
            match tokio::time::timeout(deadline, join).await {
                Ok(_) => {}
                Err(_) => {
                    // Audit-log convention: emit on the same
                    // `graphreact::observer::audit` target the bind
                    // / accept / handshake-fail trail uses, so
                    // operators see "OTel shutdown exceeded
                    // deadline" in the same JSON-log stream as the
                    // rest of the lifecycle events. Don't propagate
                    // — we still want the cluster + engine teardown
                    // to run, and re-trying the shutdown wouldn't
                    // change the outcome (the collector is gone).
                    tracing::warn!(
                        target: "graphreact::observer::audit",
                        event = "otel_shutdown_timeout",
                        deadline_ms = deadline.as_millis() as u64,
                        "OTel provider shutdown exceeded OBSERVER_SHUTDOWN_DEADLINE",
                    );
                    // **M-Prod sub-track #1 Phase A.** Fan to the
                    // sink in parallel. The tracing line above is
                    // unchanged; the chained record gives operators
                    // a tamper-evident anchor for the deadline
                    // breach during teardown.
                    if let Some(observer) = self.observer.as_ref() {
                        if let Some(sink) = observer.audit_sink.as_ref() {
                            sink.append(graphreact_observer::AuditRecordInput {
                                kind: "otel_shutdown_timeout",
                                fields: serde_json::json!({
                                    "deadline_ms": deadline.as_millis() as u64,
                                }),
                            });
                        }
                    }
                }
            }
        }
        // Stop the observer server FIRST so any in-flight scrape
        // serializes a final consistent view of the metrics
        // before the scheduler tears down.
        if let Some(observer) = self.observer {
            observer.shutdown().await;
        }
        self.cluster.shutdown();
        // Engine teardown — `shutdown` disposes the isolate + the
        // engine on JSC / CPython; engine-free path has nothing
        // to release.
        match self.host {
            SchedulerHost::Empty(_) => {}
            #[cfg(target_os = "macos")]
            SchedulerHost::Jsc(mut runtime) => {
                let _ = runtime.shutdown();
            }
            #[cfg(target_os = "macos")]
            SchedulerHost::Cpython(mut runtime) => {
                let _ = runtime.shutdown();
            }
            // **M22.** Mixed shutdown reverses init order across
            // every active engine slot; errors are logged inside
            // MixedRuntime::shutdown but never propagated past the
            // daemon teardown.
            #[cfg(target_os = "macos")]
            SchedulerHost::Mixed(mut runtime) => {
                let _ = runtime.shutdown();
            }
        }
    }

    /// **M16-Backend P2.** Local socket address the observer HTTP
    /// server is bound to, or `None` if no observer was configured.
    /// Lets tests scrape `/metrics` without parsing CLI output.
    #[must_use]
    pub fn observer_addr(&self) -> Option<std::net::SocketAddr> {
        self.observer.as_ref().map(|h| h.local_addr)
    }

    /// **M-Prod sub-track #4 Phase α.** Clone the in-flight ingress
    /// counter so out-of-process tests can simulate stuck requests
    /// (drop the returned guard's clone never → drain hits the
    /// deadline) without going through real HTTP. Returns `None`
    /// when no observer was wired (the counter exists but is
    /// inert — there's no ingress surface bumping it).
    #[must_use]
    pub fn observer_ingress_counter(&self) -> Option<graphreact_observer::IngressInFlight> {
        self.observer.as_ref().map(|_| self.ingress_in_flight.clone())
    }
}

/// Construct a daemon, install all dist sinks on the scheduler,
/// open the TLS listener, optionally dial peers. Returns a live
/// [`DaemonHandle`]. Async because `Cluster::start` + `Cluster::connect`
/// are async.
pub async fn start_daemon(opts: DaemonOptions) -> Result<DaemonHandle, CommandError> {
    let mut host = build_scheduler_host(opts.path.as_deref(), opts.runtime, opts.debug)?;
    // **M16-Backend P8.** Install debugpy on the freshly-mounted
    // CPython isolate (if `--debug=cpython:<port>` was passed).
    // build_scheduler_host (above) already mounted via the
    // body-deferring `mount_with_scheduler` path so the first batch
    // hasn't fired yet. With `wait=true` (default), `debugpy.listen`
    // blocks until the operator's IDE attaches; subsequent
    // `process_batch` calls then run with breakpoints active.
    #[cfg(target_os = "macos")]
    if let Some(d) = opts.debug {
        if let SchedulerHost::Cpython(runtime) = &mut host {
            let mut engine = runtime.engine.borrow_mut();
            crate::debug::cpython_debugpy::install_debugpy(
                &mut engine,
                runtime.iso,
                d.port,
                opts.debug_wait,
            )
            .map_err(|e| CommandError::ClusterArg(format!("debug install failed: {e}")))?;
        }
    }
    // **M16-Backend P10.** Build the cluster capability list, adding
    // the `observer-otel-v1` capability when this daemon advertises
    // OTel-aware outbound trace propagation. Peers with the matching
    // capability participate in cross-cluster traces; peers without
    // still receive trace_context bytes correctly (wire format is
    // mandatorily v3) but their own outbound emissions don't carry
    // it. Without `--features otel` setting `observer_otel_endpoint`
    // returns `CommandError::ClusterArg` below before we get here,
    // so the capability is only advertised when OTel is actually
    // wired.
    #[allow(unused_mut)] // mut only needed under `--features otel`
    let mut capabilities: Vec<String> = vec![
        "grx-v1".into(),
        RELOAD_V1_CAPABILITY.into(),
        // **M-Ingress phase 2.** This daemon understands
        // `wire::Message.correlation_id` — peers advertising the
        // same flag will propagate correlation_id over the wire,
        // letting cross-cluster `POST /v1/nodes/:id/call` flows
        // resolve replies on remote handlers' emits. Always
        // advertised because the daemon ships the management API
        // call endpoint by default; non-call workloads pay no
        // additional wire cost (correlation_id stays None on
        // every emit).
        graphreact_dist::INGRESS_CALL_V1_CAPABILITY.into(),
    ];
    #[cfg(feature = "otel")]
    if opts.observer_otel_endpoint.is_some() {
        capabilities.push(graphreact_dist::OBSERVER_OTEL_V1_CAPABILITY.into());
    }
    #[cfg(not(feature = "otel"))]
    if opts.observer_otel_endpoint.is_some() {
        return Err(CommandError::ClusterArg(
            "--observer-otel-endpoint requires `--features otel` on graphreact-cli".into(),
        ));
    }
    let config = ClusterConfig {
        node_id: opts.node_id.clone(),
        creation: Creation(default_creation()),
        listen_addr: opts.listen,
        cookie: opts.cookie,
        tls: opts.tls,
        capabilities,
        capabilities_bitmap: 0,
        net_tick: opts.net_tick,
        net_split_policy: opts.net_split_policy,
    };
    // Cluster::start installs the dist sinks on `&mut Scheduler`.
    let cluster = Cluster::start(config, host.scheduler_mut()).await?;

    // **M16-Backend P10.** Construct the OtelSink + install it on
    // BOTH the cluster's TelemetrySink slot (joined into the
    // MultiplexSink below) AND the cluster's WireTracingHook slot
    // (so outbound remote messages carry trace_context). The
    // SdkTracerProvider is parked on the DaemonHandle so
    // `shutdown()` can flush buffered spans before tearing down.
    #[cfg(feature = "otel")]
    let (otel_sink_dyn, otel_provider): (
        Option<std::sync::Arc<dyn graphreact_core::TelemetrySink>>,
        Option<graphreact_observer::SdkTracerProvider>,
    ) = if let Some(endpoint) = opts.observer_otel_endpoint.as_deref() {
        let (sink, provider) = graphreact_observer::build_otel_sink(
            endpoint,
            opts.observer_otel_service_name.as_deref(),
        )
        .map_err(|e| CommandError::ClusterArg(format!("otel exporter build: {e}")))?;
        // Install as the wire-tracing hook so route_outbound
        // populates wire::Message::trace_context.
        cluster.install_wire_tracing_hook(
            sink.clone() as std::sync::Arc<dyn graphreact_dist::WireTracingHook>,
        );
        let sink_dyn = sink as std::sync::Arc<dyn graphreact_core::TelemetrySink>;
        (Some(sink_dyn), Some(provider))
    } else {
        (None, None)
    };
    #[cfg(not(feature = "otel"))]
    let otel_sink_dyn: Option<std::sync::Arc<dyn graphreact_core::TelemetrySink>> = None;

    // M16-Backend P2+P4 — install PrometheusSink on scheduler +
    // cluster, then spawn the management API HTTP server (axum
    // 0.7 with /v1/healthz, /v1/identity, /v1/metrics, /v1/nodes,
    // /v1/nodes/{id}, /v1/supervisor/tree). The same
    // `Arc<MetricsInner>` is shared between the sink (writer) and
    // the server's `/v1/metrics` handler (reader). The snapshot
    // bridge receiver lives on the scheduler thread (drained by
    // `drive_handle_until` after each `process_batch`); axum
    // handlers send `SnapshotRequest` on the bridge sender. When
    // `opts.observer_bind` is `None`, none of this runs.
    // **M22 phase 4.** Track the active MetricsInner arc so the
    // per-engine mount counters can be populated after the
    // observer is wired. `None` when no observer is configured
    // (zero-overhead default).
    #[allow(unused_assignments)]
    let mut metrics_for_engine_counts: Option<std::sync::Arc<graphreact_observer::MetricsInner>> =
        None;
    // **M-Prod sub-track #5.** Cluster-readiness counter shared
    // between the `/v1/readyz` handler and the per-peer dial loop
    // below. The handler returns 200 once `configured_peers == 0`
    // (single-node) OR `dialed_count.load() >= 1` (at least one
    // peer dialed). Each successful `cluster.connect()` below
    // bumps the counter; partial-init failure stays at 0 so the
    // partial daemon is correctly NOT-ready.
    let configured_peers = opts.peers.len();
    let dialed_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dialed_for_probe = dialed_count.clone();
    let cluster_readyz_probe: std::sync::Arc<graphreact_observer::ClusterReadyzProbe> =
        std::sync::Arc::new(move || {
            configured_peers == 0
                || dialed_for_probe.load(std::sync::atomic::Ordering::Relaxed) >= 1
        });

    // **M-Prod sub-track #4 Phase α.** Mint the drain bit BEFORE
    // the observer spawn so the spawn can clone it into ApiState
    // for the management API hot path AND we can hold a clone on
    // the DaemonHandle for the SIGTERM-driven orchestrator.
    let drain_flag = graphreact_observer::DrainFlag::new();

    let observer = if let Some(bind_cfg) = opts.observer_bind {
        let metrics = std::sync::Arc::new(graphreact_observer::MetricsInner::new());
        metrics_for_engine_counts = Some(metrics.clone());
        let prom_sink = std::sync::Arc::new(graphreact_observer::PrometheusSink::from_arc(
            metrics.clone(),
        ));
        // Random process-start id for crash detection by ops
        // tooling (see Identity::process_start_id docs).
        let process_start_id = {
            use rand::RngCore;
            rand::thread_rng().next_u64()
        };
        // **M-Prod sub-track #1 Phase A.** Construct the
        // FileAuditSink BEFORE the management API bind so the very
        // first event (`bind`) lands in the chained file. Surfacing
        // the open error here gives operators a fail-fast signal
        // (chain-head recovery picked up a tampered tail) — far
        // better than silently degrading to tracing-only audit at
        // start-up.
        let audit_sink: Option<
            std::sync::Arc<dyn graphreact_observer::AuditSink>,
        > = match opts.observer_audit_log.as_ref() {
            Some(path) => Some(std::sync::Arc::new(
                graphreact_observer::FileAuditSink::open(path).map_err(|e| {
                    CommandError::ClusterArg(format!(
                        "observer-audit-log open {}: {e}",
                        path.display()
                    ))
                })?,
            )),
            None => None,
        };
        let server = graphreact_observer::spawn_management_api_with_options(
            graphreact_observer::SpawnOptions {
                bind: bind_cfg,
                metrics,
                process_start_id,
                cluster_readyz: Some(cluster_readyz_probe.clone()),
                audit_sink,
                // **M-Prod sub-track #4 Phase α.** Thread the drain
                // flag through SpawnOptions so the management API's
                // /v1/readyz + ingress handlers can observe the
                // SIGTERM-driven flip emitted by
                // DaemonHandle::start_drain (commands.rs:start_drain).
                draining: Some(drain_flag.clone()),
                // **M-Prod sub-track #6 Phase A.** `None` = read the
                // per-IP cap from `GRX_RATE_LIMIT_PER_IP_{PER_SEC,BURST}`
                // env vars (or fall back to the in-code defaults).
                // Operators tune the cap without a daemon recompile;
                // tests inject explicit caps via `Some(RateLimitConfig)`.
                rate_limit: None,
            },
        )
        .await
        .map_err(|e| CommandError::ClusterArg(format!("observer bind: {e}")))?;
        // M16-Backend P5 — fan PrometheusSink + the API server's
        // RingBufferTelemetry through one MultiplexSink so both
        // see every event without two install_telemetry_sink calls
        // (which would overwrite). The ring's Arc is shared with
        // the API handler that serves /v1/nodes/:id/messages/recent.
        // M16-Backend P7 — adds the WebSocketSink as a third
        // consumer; per-event cost is one owned-event allocation +
        // one broadcast send (atomic on the ring; no blocking even
        // if subscribers are slow).
        let ring_sink = server.ring.clone() as std::sync::Arc<dyn graphreact_core::TelemetrySink>;
        let prom_dyn = prom_sink as std::sync::Arc<dyn graphreact_core::TelemetrySink>;
        let mut mux_builder = graphreact_observer::MultiplexSink::builder()
            .push(prom_dyn.clone())
            .push(ring_sink.clone());
        if let Some(ws_sink) = server.websocket.as_ref() {
            let ws_dyn = std::sync::Arc::new(ws_sink.clone())
                as std::sync::Arc<dyn graphreact_core::TelemetrySink>;
            mux_builder = mux_builder.push(ws_dyn);
        }
        // **M-Ingress phase 2.** Add the PendingCalls registry as
        // a multiplex consumer so the scheduler's `on_call_reply`
        // event resolves the awaiting `oneshot::Sender` for any
        // in-flight `POST /v1/nodes/:id/call`.
        let pending_calls_dyn =
            std::sync::Arc::new(server.pending_calls.clone())
                as std::sync::Arc<dyn graphreact_core::TelemetrySink>;
        mux_builder = mux_builder.push(pending_calls_dyn);
        // **M16-Backend P10.** Add OtelSink as a fourth multiplex
        // consumer (after Prom + Ring + WS). Only installed when
        // `--observer-otel-endpoint` is set; non-OTel deployments
        // skip this branch.
        if let Some(ref otel_sink) = otel_sink_dyn {
            mux_builder = mux_builder.push(otel_sink.clone());
        }
        let mux = std::sync::Arc::new(mux_builder.build())
            as std::sync::Arc<dyn graphreact_core::TelemetrySink>;
        host.scheduler_mut().install_telemetry_sink(mux.clone());
        cluster.install_telemetry_sink(mux);
        Some(server)
    } else if let Some(otel_sink) = otel_sink_dyn.clone() {
        // **M16-Backend P10.** OTel sink installed without
        // `--observer-bind` — install directly on scheduler +
        // cluster via single-sink path. Supports the "OTel-only"
        // deployment shape (no Prometheus, no management API).
        host.scheduler_mut().install_telemetry_sink(otel_sink.clone());
        cluster.install_telemetry_sink(otel_sink);
        None
    } else {
        None
    };

    // **M22 phase 4.** Populate `grx_engine_mount_count{engine}`
    // immediately after observer setup. The counters are bounded-
    // cardinality (one entry per engine slot) and read by the
    // `/v1/metrics` Prometheus scrape; recording here means the
    // FIRST scrape (which may fire before any process_batch
    // round) sees the correct mount counts.
    if let Some(metrics) = &metrics_for_engine_counts {
        record_engine_mount_metrics(&host, metrics);
    }

    // M14.8.x: run one initial `process_batch` so the engine-
    // hosted bodies actually evaluate at boot — `mount_with_scheduler`
    // / `mount_python_definitions` register nodes but do NOT run
    // their bodies. Running batch HERE (after `Cluster::start`,
    // BEFORE peer dial) makes sure every body's mount-time
    // `useGroupJoin` / `useMonitor` / `useLink` call fires through
    // the dist sinks BEFORE any peer's `PgDiscover` arrives, so
    // our `PgSync` reply includes local memberships and the
    // cluster converges on the first round.
    //
    // For the engine-free path this is a no-op (no nodes registered
    // → process_batch settles immediately).
    host.process_batch().map_err(|e| {
        CommandError::Runtime(crate::runtime::RuntimeError::from(e))
    })?;

    // **M22 phase 4.** Record the first batch's body-evaluation
    // counts. Subsequent `process_batch` rounds (driven by
    // `drive_handle_until`) record via the same helper after each
    // tick.
    if let Some(metrics) = &metrics_for_engine_counts {
        record_engine_eval_metrics(&host, metrics);
    }

    // Dial each peer. `connect` waits for handshake completion;
    // any failure short-circuits the daemon boot (substrate-
    // not-MVP: ops would rather see the error early than have a
    // half-converged cluster).
    //
    // **M-Prod sub-track #5.** Bump `dialed_count` after each
    // successful dial so the `/v1/readyz` cluster-membership
    // probe (constructed above) can return 200 once at least one
    // peer is alive. The atomic is read by the management API
    // handler on every readyz request; bumping it here keeps the
    // probe lock-free.
    for (peer_id, peer_addr) in opts.peers {
        cluster.connect(peer_addr, peer_id).await?;
        dialed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    // **M-Prod sub-track #4 Phase α.** Surface the in-flight
    // ingress counter from the observer (or a fresh empty one
    // when no observer was wired) so `DaemonHandle::start_drain`
    // can poll it during quiescence.
    let ingress_in_flight = observer
        .as_ref()
        .map(|s| s.ingress_in_flight.clone())
        .unwrap_or_default();
    // **M-Prod sub-track #4 Phase α.** Only park the drain bit
    // on the handle when an observer is actually wired —
    // start_drain treats `None` as "no ingress surface to
    // quiesce" and returns the no-op outcome immediately.
    let drain = observer.as_ref().map(|_| drain_flag);

    // **M-Prod sub-track #4 Phase α.** Validate the drain
    // budget composes correctly with OBSERVER_SHUTDOWN_DEADLINE.
    // The locked cross-track contract (m-prod-observability.md
    // line 113) says drain_deadline MUST be ≥
    // OBSERVER_SHUTDOWN_DEADLINE so OTel teardown runs INSIDE
    // the drain budget. Warn rather than reject so existing
    // tests with custom env-var combos keep working; ops
    // dashboards can alert on the warn line in production.
    let drain_dur = resolve_drain_deadline(opts.drain_deadline_secs);
    let otel_dur = observer_shutdown_deadline();
    if drain_dur < otel_dur {
        let drain_ms = u64::try_from(drain_dur.as_millis()).unwrap_or(u64::MAX);
        let otel_ms = u64::try_from(otel_dur.as_millis()).unwrap_or(u64::MAX);
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "drain_budget_misconfig",
            drain_deadline_ms = drain_ms,
            observer_shutdown_deadline_ms = otel_ms,
            "drain deadline shorter than OBSERVER_SHUTDOWN_DEADLINE — OTel teardown may be cut short by total grace-period exhaustion"
        );
    }

    Ok(DaemonHandle {
        cluster,
        host,
        observer,
        metrics: metrics_for_engine_counts,
        #[cfg(feature = "otel")]
        otel_provider,
        drain,
        ingress_in_flight,
    })
}

/// **M22 phase 4.** Increment `grx_engine_mount_count{engine}`
/// according to the engine slots active on `host`. Called once
/// during `start_daemon` after the observer is wired so the first
/// `/v1/metrics` scrape sees the mount counts. Pure-lang
/// daemons emit one series; mixed daemons emit one per active
/// engine slot.
fn record_engine_mount_metrics(
    host: &SchedulerHost,
    metrics: &graphreact_observer::MetricsInner,
) {
    match host {
        SchedulerHost::Empty(_) => {}
        #[cfg(target_os = "macos")]
        SchedulerHost::Jsc(r) => {
            for _ in &r.mounted {
                metrics.engine_mount_count_inc("jsc");
            }
        }
        #[cfg(target_os = "macos")]
        SchedulerHost::Cpython(r) => {
            for _ in &r.mounted {
                metrics.engine_mount_count_inc("cpython");
            }
        }
        #[cfg(target_os = "macos")]
        SchedulerHost::Mixed(m) => {
            m.record_mount_metrics(metrics);
        }
    }
}

/// **M22 phase 4.** Increment `grx_engine_eval_count{engine}`
/// according to the engine slots active on `host`. Called after
/// each `process_batch` round (initial boot batch + every
/// `drive_handle_until` tick). The per-batch upper bound is one
/// eval per mounted node per active engine.
fn record_engine_eval_metrics(
    host: &SchedulerHost,
    metrics: &graphreact_observer::MetricsInner,
) {
    match host {
        SchedulerHost::Empty(_) => {}
        #[cfg(target_os = "macos")]
        SchedulerHost::Jsc(r) => {
            for _ in &r.mounted {
                metrics.engine_eval_count_inc("jsc");
            }
        }
        #[cfg(target_os = "macos")]
        SchedulerHost::Cpython(r) => {
            for _ in &r.mounted {
                metrics.engine_eval_count_inc("cpython");
            }
        }
        #[cfg(target_os = "macos")]
        SchedulerHost::Mixed(m) => {
            m.record_eval_metrics(metrics);
        }
    }
}

/// Mount the engine for the daemon. Engine-free for `path: None`
/// or `runtime: Null`; engine-hosted variants own a `JsRuntime` /
/// `PyRuntime` per chosen runtime. The M14.8.x retrofit on
/// `Scheduler::install_remote_group_sink` makes the
/// mount-then-cluster ordering safe: `Cluster::start` is called
/// AFTER the mount has already evaluated bodies once, but the
/// retrofit synthesises `Join` events for every membership the
/// bodies registered, so `DistGroups` populates correctly.
fn build_scheduler_host(
    path: Option<&Path>,
    runtime: RuntimeArg,
    debug: Option<crate::debug::DebugFlag>,
) -> Result<SchedulerHost, CommandError> {
    match (path, runtime) {
        (None, _) => Ok(SchedulerHost::Empty(Box::default())),
        #[cfg(target_os = "macos")]
        (Some(path), RuntimeArg::Jsc) => {
            // **M16-Backend P9.** JSC mount via `mount_with_scheduler`
            // registers definitions but does NOT run any body
            // (bodies fire on the first `process_batch`, scheduled
            // by `start_daemon` AFTER `Cluster::start`). Installing
            // the JSC RemoteInspector listener here makes the
            // JSContext appear in `Safari → Develop → [Hostname]
            // → JSContext` BEFORE the first body executes — operator
            // can attach Web Inspector and set breakpoints; first
            // render hits them.
            //
            // Lockdownd discovery is async/silent: there's no
            // wait-for-client semantic in JSC's RemoteInspector
            // (unlike debugpy.wait_for_client), so the install
            // returns immediately. If the operator attaches mid-
            // render the breakpoints take effect on the next
            // body invocation.
            let runtime = crate::jsruntime::mount_with_scheduler(path)?;
            if let Some(d) = debug {
                let mut engine = runtime.engine.borrow_mut();
                crate::debug::jsc_inspector::install_jsc_inspector(
                    &mut engine,
                    runtime.iso,
                    d.port,
                )
                .map_err(|e| {
                    CommandError::ClusterArg(format!("debug install failed: {e}"))
                })?;
            }
            Ok(SchedulerHost::Jsc(Box::new(runtime)))
        }
        #[cfg(target_os = "macos")]
        (Some(path), RuntimeArg::Cpython) => {
            // **M16-Backend P8.** When `--debug=cpython:<port>` is
            // set we mount via `mount_with_scheduler` (registers
            // definitions WITHOUT running any body) so debugpy
            // lands BEFORE the first batch — required for `def
            // init(ctx)` breakpoints to hit on first mount. The
            // first `process_batch` happens inside
            // `drive_handle_until` after Cluster::start, by which
            // time debugpy is listening + (if `wait=true`) the
            // operator has attached.
            let runtime = if debug.is_some() {
                crate::pyruntime::mount_with_scheduler(path)?
            } else {
                crate::pyruntime::mount_python_definitions(path)?
            };
            Ok(SchedulerHost::Cpython(Box::new(runtime)))
        }
        (Some(_), RuntimeArg::Null) => Err(CommandError::ClusterArg(
            "`--runtime null` requires no `--path`; remove the path or pick a real runtime"
                .into(),
        )),
        // **M22.** Mixed-runtime daemon arm. Brings up every
        // engine present in the IR via `mount_mixed`. Debug attach
        // is rejected by `validate_debug_against_runtime` above so
        // `debug` is unconditionally None on this arm.
        #[cfg(target_os = "macos")]
        (Some(path), RuntimeArg::Mixed) => {
            if debug.is_some() {
                // Defensive — the validator should have rejected
                // this combination already, but keep the
                // belt-and-braces check so a future caller that
                // bypasses validation doesn't silently drop the
                // flag.
                return Err(CommandError::ClusterArg(
                    "--debug under `--runtime=mixed` is rejected; pin to a single engine."
                        .into(),
                ));
            }
            let runtime = crate::mixedruntime::mount_mixed(path)?;
            Ok(SchedulerHost::Mixed(Box::new(runtime)))
        }
        (Some(_), other) => Err(CommandError::ClusterArg(format!(
            "runtime `{other}` not wired under `grx cluster daemon` — \
             use `--runtime jsc`, `--runtime cpython`, or `--runtime mixed`"
        ))),
    }
}

/// Build a daemon and drive it for the configured duration (or
/// until SIGINT/SIGTERM). Used by the binary; the test path uses
/// [`start_daemon`] directly.
///
/// **M-Prod sub-track #4 Phase α.** SIGTERM (unix) now triggers a
/// graceful drain via [`DaemonHandle::start_drain`] BEFORE the
/// existing teardown path runs:
/// 1. SIGTERM arrives → `/v1/readyz` starts 503ing `draining`,
///    ingress POSTs start refusing with the same 503 `draining` body.
/// 2. `start_drain(deadline)` polls in-flight quiescence; deadline
///    sourced from `opts.drain_deadline_secs` /
///    `GRX_DRAIN_DEADLINE_SECS` / [`DEFAULT_DRAIN_DEADLINE_SECS`].
/// 3. `handle.shutdown()` runs the existing OTel-flush +
///    cluster-shutdown + engine-teardown sequence.
///
/// SIGINT (Ctrl-C) keeps the existing snappy semantics —
/// developer Ctrl-C goes straight to `shutdown()` without the
/// drain phase (no point making local-dev exits slower).
pub async fn run_daemon_blocking(opts: DaemonOptions) -> Result<String, CommandError> {
    let duration = opts.duration_secs;
    let drain_deadline = resolve_drain_deadline(opts.drain_deadline_secs);
    let mut handle = start_daemon(opts).await?;
    let exit = drive_handle_until(&mut handle, duration).await?;
    if matches!(exit, LoopExit::Sigterm) {
        // **M-Prod sub-track #4 Phase α.** Drain BEFORE shutdown
        // so the LB sees /v1/readyz=503 and ingress sees 503
        // `draining` while in-flight requests finish. Outcome is
        // logged to the audit stream via `drain_completed` /
        // `drain_deadline_exceeded` inside `start_drain`; we
        // don't propagate the deadline-exceeded condition past
        // shutdown — k8s SIGKILL is the next escalation and we
        // already emitted the audit row.
        let _ = handle.start_drain(drain_deadline).await;
    }
    handle.shutdown().await;
    Ok(match (duration, exit) {
        (Some(secs), _) => format!("daemon exited after {secs}s"),
        (None, LoopExit::Sigterm) => "daemon exited (SIGTERM, drained)".to_string(),
        (None, _) => "daemon exited (signal)".to_string(),
    })
}

/// **M-Prod sub-track #4 Phase α.** Discriminator returned by
/// [`drive_handle_until`] so callers know whether to run the
/// graceful-drain path (SIGTERM) or the snappy shutdown path
/// (SIGINT / duration). Stays a public enum because both binary
/// callers (`run_daemon_blocking`, `run_connect_blocking`) need
/// to read the discriminator; tests that drive the loop directly
/// can match on it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopExit {
    /// `duration_secs` was set and elapsed. Snappy shutdown.
    Duration,
    /// Ctrl-C (SIGINT). Developer-driven; snappy shutdown — drain
    /// is k8s/operator territory.
    Sigint,
    /// SIGTERM (unix). Kubelet-driven; trigger graceful drain
    /// before shutdown.
    Sigterm,
}

/// One-shot dial. Spins up an empty cluster, opens a TLS
/// connection to `peer_addr`, holds the connection alive for
/// `duration_secs` (or until SIGINT when absent — symmetric
/// with `daemon`'s default), then exits.
pub async fn run_connect_blocking(opts: ConnectOptions) -> Result<String, CommandError> {
    let duration = opts.duration_secs;
    let scheduler = Box::new(Scheduler::default());
    let mut host = SchedulerHost::Empty(scheduler);
    let config = ClusterConfig {
        node_id: opts.node_id.clone(),
        creation: Creation(default_creation()),
        listen_addr: opts.listen,
        cookie: opts.cookie,
        tls: opts.tls,
        capabilities: vec![
            "grx-v1".into(),
            RELOAD_V1_CAPABILITY.into(),
            graphreact_dist::INGRESS_CALL_V1_CAPABILITY.into(),
        ],
        capabilities_bitmap: 0,
        net_tick: opts.net_tick,
        net_split_policy: opts.net_split_policy,
    };
    let cluster = Cluster::start(config, host.scheduler_mut()).await?;
    cluster.connect(opts.peer_addr, opts.peer.clone()).await?;
    let mut handle = DaemonHandle {
        cluster,
        host,
        observer: None,
        // `grx connect` is a one-shot dial; no metrics surface is
        // wired so per-tick `grx_engine_eval_count` recording is a
        // no-op (drive_handle_until short-circuits when
        // observer_metrics() is None).
        metrics: None,
        // **M16-Backend P10.** `connect` is a one-shot dial; no
        // OTel sink is constructed for it (the OTel sink lives on
        // `start_daemon`'s daemon, not on connect's minimal
        // cluster). `None` here regardless of feature.
        #[cfg(feature = "otel")]
        otel_provider: None,
        // **M-Prod sub-track #4 Phase α.** `connect` has no
        // observer / ingress surface. Drain is a no-op; SIGTERM
        // composition (when sub-track #4 wires it for `connect`
        // too) goes straight to `shutdown()`.
        drain: None,
        ingress_in_flight: graphreact_observer::IngressInFlight::default(),
    };
    let _exit = drive_handle_until(&mut handle, duration).await?;
    handle.shutdown().await;
    Ok(match duration {
        Some(secs) => format!("connect to `{}` exited after {secs}s", opts.peer),
        None => format!("connect to `{}` exited (signal)", opts.peer),
    })
}

/// Drive the daemon's `process_batch` on a 50ms cadence until
/// either `duration` elapses (when `Some`) OR SIGINT / SIGTERM
/// fires (when `None`). `pub` so binaries can split
/// `start_daemon` + readiness signal + drive into separate
/// steps (the subprocess CLI test relies on the binary
/// printing its bound address BETWEEN start and drive).
///
/// **M-Prod sub-track #4 Phase α.** Now distinguishes SIGTERM
/// (kubelet-driven graceful drain) from SIGINT (developer Ctrl-C
/// snappy shutdown) via the [`LoopExit`] return value. SIGTERM
/// is unix-only; on Windows the SIGTERM arm is omitted at
/// compile time and only `ctrl_c` fires (which the SIGINT arm
/// catches). The drain-vs-snap distinction is the caller's
/// concern: `run_daemon_blocking` runs [`DaemonHandle::start_drain`]
/// only when `LoopExit::Sigterm` is returned.
pub async fn drive_handle_until(
    handle: &mut DaemonHandle,
    duration: Option<u64>,
) -> Result<LoopExit, CommandError> {
    let deadline =
        duration.map(|secs| tokio::time::Instant::now() + Duration::from_secs(secs));
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.tick().await; // skip immediate

    // **M-Prod sub-track #4 Phase α.** Register a SIGTERM signal
    // stream once outside the loop (re-registering on every
    // iteration would leak signal-handler slots). Unix-only:
    // Windows daemons fall back to ctrl_c-as-snap-shutdown. The
    // `tokio::signal::unix::signal(SignalKind::terminate())`
    // ctor returns `io::Result`; failure here is exceptional
    // (signalfd-unsupported kernel) and we surface it through
    // `CommandError::ClusterArg` rather than panic. Bind under
    // cfg(unix) so non-unix builds don't see the unused-import
    // warning.
    #[cfg(unix)]
    let mut sigterm = {
        use tokio::signal::unix::{signal, SignalKind};
        signal(SignalKind::terminate())
            .map_err(|e| CommandError::ClusterArg(format!("SIGTERM register: {e}")))?
    };

    loop {
        if let Some(d) = deadline {
            if tokio::time::Instant::now() >= d {
                return Ok(LoopExit::Duration);
            }
        }
        // Race a process_batch tick against ctrl_c (and the
        // optional deadline). `tokio::select!` is fair across
        // arms so the deadline arm gets evaluated even under
        // sustained tick traffic.
        let sigint = tokio::signal::ctrl_c();
        #[cfg(unix)]
        tokio::select! {
            _ = tick.tick() => {
                drive_one_tick(handle)?;
            }
            _ = sigint, if duration.is_none() => {
                return Ok(LoopExit::Sigint);
            }
            _ = sigterm.recv(), if duration.is_none() => {
                return Ok(LoopExit::Sigterm);
            }
        }
        #[cfg(not(unix))]
        tokio::select! {
            _ = tick.tick() => {
                drive_one_tick(handle)?;
            }
            _ = sigint, if duration.is_none() => {
                return Ok(LoopExit::Sigint);
            }
        }
    }
}

/// **M-Prod sub-track #4 Phase α.** One tick of the drive loop —
/// pulled out so the select! macro stays small enough to read
/// across the unix / non-unix cfg-gated branches.
fn drive_one_tick(handle: &mut DaemonHandle) -> Result<(), CommandError> {
    // **M22 verify fix.** Route the tick through
    // `handle.process_batch()` (which dispatches via
    // `SchedulerHost::process_batch`) instead of the raw
    // `scheduler().process_batch()`. Under `--runtime=mixed` the
    // latter would only tick the first engine slot's scheduler —
    // CPython + Go bodies would never re-render after the boot
    // batch. The `SchedulerHost::process_batch` arm for Mixed
    // fans out across every active engine in init order via
    // `MixedRuntime::process_batch`. Single-engine paths see
    // identical behavior (same engine TLS guard install, same
    // batch semantics).
    handle
        .process_batch()
        .map_err(|e| CommandError::Runtime(crate::runtime::RuntimeError::from(e)))?;
    // **M22 verify fix.** Per-tick increment of
    // `grx_engine_eval_count{engine}` so the metric actually
    // tracks evaluation throughput rather than reporting a
    // one-shot boot snapshot.
    if let Some(metrics) = handle.observer_metrics() {
        record_engine_eval_metrics(&handle.host, metrics);
    }
    // M16-Backend P4 — drain pending management-API snapshot
    // requests. Each request was posted by an axum handler
    // running on a tokio worker thread; we respond on the
    // scheduler thread (the only thread that can read scheduler
    // state, since Scheduler is !Send). Worst-case latency for
    // an API request: one process_batch round (~50ms tick
    // cadence + the drain that follows).
    drain_management_api_snapshots(handle);
    Ok(())
}

/// Drain any pending management-API snapshot requests from the
/// observer's bridge channel + respond from scheduler state.
/// Called once per `drive_handle_until` tick (after `process_batch`
/// completes) so handlers see consistent post-batch state.
fn drain_management_api_snapshots(handle: &mut DaemonHandle) {
    use graphreact_observer::SnapshotRequest;
    let Some(observer) = handle.observer.as_mut() else {
        return;
    };
    // Borrow split: pull the receiver out of the &mut observer,
    // call introspect_* on the scheduler (which lives in
    // host.scheduler_mut()) without conflicting with the receiver
    // borrow.
    let scheduler = handle.host.scheduler_mut();
    graphreact_observer::drain_snapshot_requests(&mut observer.snapshot_rx, |req| {
        match req {
            SnapshotRequest::Nodes(tx) => {
                let _ = tx.send(scheduler.introspect_nodes());
            }
            SnapshotRequest::NodeDetail(id, tx) => {
                let _ = tx.send(scheduler.introspect_node(&id));
            }
            SnapshotRequest::SupervisorTree(tx) => {
                let _ = tx.send(scheduler.introspect_supervisors());
            }
            // **M-Ingress phase 1.** Validate node existence on the
            // scheduler thread (we hold the Scheduler borrow), then
            // dispatch via `SchedulerHandle::send` which prepends
            // `from = "__external__"` so the manifest's
            // `accepts_external` allowlist is enforced at delivery.
            SnapshotRequest::InjectMessage {
                node_id,
                message_type,
                payload,
                oneshot_tx,
            } => {
                if scheduler.introspect_node(&node_id).is_none() {
                    let _ = oneshot_tx
                        .send(graphreact_observer::InjectResult::NodeNotFound);
                } else {
                    // `send_ingress` prepends `from = "__ingress__"`, which
                    // makes `Scheduler::deliver` enforce the target's
                    // `manifest.accepts_external` allowlist. Pre-existing
                    // `send` callers (tests, dist, sources) keep the
                    // legacy bypass under `from = "__external__"`.
                    scheduler
                        .handle()
                        .send_ingress(node_id, message_type, payload);
                    let _ = oneshot_tx
                        .send(graphreact_observer::InjectResult::Accepted);
                }
            }
            // **M-Ingress phase 2.** Validate node existence + the
            // manifest's `accepts_external_call` table on the
            // scheduler thread (we hold the borrow). Match the
            // (request_type, reply_type) pair against the
            // declared entries; unmatched → `RejectedByManifest`
            // (HTTP 403). On dispatch, `send_ingress_with_correlation`
            // attaches the correlation_id so `Scheduler::deliver`'s
            // auto-propagation tags the first matching reply.
            SnapshotRequest::InjectMessageWithReply {
                node_id,
                message_type,
                payload,
                reply_type,
                correlation_id,
                oneshot_tx,
            } => {
                if scheduler.introspect_node(&node_id).is_none() {
                    let _ = oneshot_tx.send(
                        graphreact_observer::InjectMessageWithReplyDisposition::NodeNotFound,
                    );
                } else {
                    // Look up the manifest from the scheduler's
                    // live index for the matching
                    // `accepts_external_call` entry.
                    let manifest_match = scheduler
                        .manifests
                        .borrow()
                        .get(&node_id)
                        .map(|m| {
                            m.accepts_external_call.iter().any(|c| {
                                c.request_type == message_type
                                    && c.reply_type == reply_type
                            })
                        })
                        .unwrap_or(false);
                    if manifest_match {
                        scheduler.handle().send_ingress_with_correlation(
                            node_id,
                            message_type,
                            payload,
                            correlation_id,
                        );
                        let _ = oneshot_tx.send(
                            graphreact_observer::InjectMessageWithReplyDisposition::Dispatched,
                        );
                    } else {
                        let _ = oneshot_tx.send(
                            graphreact_observer::InjectMessageWithReplyDisposition::RejectedByManifest,
                        );
                    }
                }
            }
            // **M-Ingress phase 3.** Per-line bulk inject. Same
            // node-existence + manifest-gate sequence as the call
            // path, but reusing Phase 1's `accepts_external`
            // allowlist (single-shot shape, not call shape) since
            // bulk lines are fire-and-forget. The manifest check
            // is hoisted to drain (rather than deferring to
            // `Scheduler::deliver` like Phase 1's single-shot
            // path) so the per-line response can surface
            // `RejectedByManifest` synchronously to the streaming
            // NDJSON client.
            SnapshotRequest::InjectMessageBulkLine {
                node_id,
                message_type,
                payload,
                oneshot_tx,
            } => {
                if scheduler.introspect_node(&node_id).is_none() {
                    let _ = oneshot_tx
                        .send(graphreact_observer::BulkInjectResult::NodeNotFound);
                } else {
                    let manifest_match = scheduler
                        .manifests
                        .borrow()
                        .get(&node_id)
                        .map(|m| {
                            m.accepts_external.iter().any(|t| t == &message_type)
                        })
                        .unwrap_or(false);
                    if manifest_match {
                        scheduler
                            .handle()
                            .send_ingress(node_id, message_type, payload);
                        let _ = oneshot_tx
                            .send(graphreact_observer::BulkInjectResult::Accepted);
                    } else {
                        let _ = oneshot_tx
                            .send(graphreact_observer::BulkInjectResult::RejectedByManifest);
                    }
                }
            }
        }
    });
}

/// Process-monotonic creation epoch. Increments per
/// `start_daemon` / `run_connect_blocking` call so two
/// in-process instances spawned by the same test see
/// distinguishable Creation values (the OTP convention).
fn default_creation() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

// ---------------------------------------------------------------------
// CLI argument helpers — translate the raw `clap`-parsed strings into
// the structured forms `start_daemon` / `run_connect_blocking` consume.
// ---------------------------------------------------------------------

/// Parse a peer entry of the form `name@host:port` into a
/// `(NodeId, SocketAddr)`. The host:port suffix doubles as the
/// dial address; `NodeId::host` is `host:port` literally and is
/// handed to `tokio::net::TcpStream::connect` (via `dial`).
pub fn parse_peer_entry(spec: &str) -> Result<(NodeId, SocketAddr), CommandError> {
    let id: NodeId = spec.parse().map_err(|e| {
        CommandError::ClusterArg(format!("peer `{spec}`: {e}"))
    })?;
    let addr: SocketAddr = id.host().parse().map_err(|e| {
        CommandError::ClusterArg(format!(
            "peer `{spec}` host `{}` is not a valid socket address: {e}",
            id.host()
        ))
    })?;
    Ok((id, addr))
}

/// Resolve the cookie source (env / file) into a [`SecretCookie`].
/// Returns [`CommandError::ClusterArg`] when neither is provided.
pub fn resolve_cookie(
    cookie_env: Option<&str>,
    cookie_file: Option<&Path>,
) -> Result<SecretCookie, CommandError> {
    match (cookie_env, cookie_file) {
        (Some(name), None) => Ok(SecretCookie::from_env(name)?),
        (None, Some(path)) => Ok(SecretCookie::from_file(path)?),
        (None, None) => Err(CommandError::ClusterArg(
            "must specify --cookie-env or --cookie-file".into(),
        )),
        (Some(_), Some(_)) => Err(CommandError::ClusterArg(
            "--cookie-env and --cookie-file are mutually exclusive".into(),
        )),
    }
}

/// Load PEM material from disk into a [`TlsConfig`]. mTLS is
/// always required (matches the production default).
pub fn load_tls_from_paths(
    cert_path: &Path,
    key_path: &Path,
    ca_path: &Path,
) -> Result<TlsConfig, CommandError> {
    let cert_path_str = cert_path.display();
    let key_path_str = key_path.display();
    let ca_path_str = ca_path.display();
    let cert_chain: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(cert_path)
            .map_err(|e| CommandError::ClusterArg(format!("cert {cert_path_str}: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| CommandError::ClusterArg(format!("cert {cert_path_str}: {e}")))?;
    let private_key: PrivateKeyDer<'static> =
        PrivateKeyDer::from_pem_file(key_path)
            .map_err(|e| CommandError::ClusterArg(format!("key {key_path_str}: {e}")))?;
    let ca: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(ca_path)
        .map_err(|e| CommandError::ClusterArg(format!("ca {ca_path_str}: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| CommandError::ClusterArg(format!("ca {ca_path_str}: {e}")))?;
    Ok(TlsConfig::from_pem_bytes(cert_chain, private_key, ca, true)?)
}

// `Arc` import is used by `load_tls_from_paths`'s arena symbol
// resolution under `pub use rustls::sign::CertifiedKey` (clippy
// would warn otherwise).
#[allow(dead_code)]
fn _arc_kept_alive() -> Arc<()> {
    Arc::new(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **M-Prod sub-track #5 safest slice.** Verify the
    /// `OBSERVER_SHUTDOWN_DEADLINE` env var contract: unset / empty
    /// / unparseable / zero all fall through to the default, a
    /// positive integer is accepted as seconds.
    ///
    /// The env var is set + cleared inside `std::sync::Mutex`
    /// because cargo runs unit tests in-process on parallel threads
    /// by default and we don't want test-to-test cross-contamination.
    #[test]
    fn observer_shutdown_deadline_env_var() {
        // Run all env-var probes under a single mutex so concurrent
        // unit tests in this crate don't race the
        // `OBSERVER_SHUTDOWN_DEADLINE` env mutation. Cargo runs
        // unit tests in-process and env vars are process-global.
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        std::env::remove_var("OBSERVER_SHUTDOWN_DEADLINE");
        assert_eq!(
            observer_shutdown_deadline(),
            Duration::from_secs(DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS),
            "unset falls through to default",
        );

        std::env::set_var("OBSERVER_SHUTDOWN_DEADLINE", "");
        assert_eq!(
            observer_shutdown_deadline(),
            Duration::from_secs(DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS),
            "empty falls through to default",
        );

        std::env::set_var("OBSERVER_SHUTDOWN_DEADLINE", "not-a-number");
        assert_eq!(
            observer_shutdown_deadline(),
            Duration::from_secs(DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS),
            "unparseable falls through to default",
        );

        std::env::set_var("OBSERVER_SHUTDOWN_DEADLINE", "0");
        assert_eq!(
            observer_shutdown_deadline(),
            Duration::from_secs(DEFAULT_OBSERVER_SHUTDOWN_DEADLINE_SECS),
            "zero falls through to default (would race join to instant timeout)",
        );

        std::env::set_var("OBSERVER_SHUTDOWN_DEADLINE", "12");
        assert_eq!(
            observer_shutdown_deadline(),
            Duration::from_secs(12),
            "positive integer is accepted as seconds",
        );

        // Leave the env clean for any other test running on this
        // binary.
        std::env::remove_var("OBSERVER_SHUTDOWN_DEADLINE");
    }

    /// **M-Prod sub-track #4 Phase α.** Mirror of the
    /// `OBSERVER_SHUTDOWN_DEADLINE` env-var contract for the
    /// `GRX_DRAIN_DEADLINE_SECS` knob backing
    /// [`resolve_drain_deadline`]. Precedence: explicit CLI value
    /// wins, then env var, then default. Empty / unparseable /
    /// zero values fall through to the default so an operator
    /// can't accidentally race the in-flight quiescence loop to
    /// instant timeout.
    #[test]
    fn drain_deadline_resolution_precedence() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        std::env::remove_var("GRX_DRAIN_DEADLINE_SECS");
        assert_eq!(
            resolve_drain_deadline(None),
            Duration::from_secs(DEFAULT_DRAIN_DEADLINE_SECS),
            "unset env + no CLI falls through to default",
        );
        assert_eq!(
            resolve_drain_deadline(Some(7)),
            Duration::from_secs(7),
            "explicit CLI value wins over default",
        );

        std::env::set_var("GRX_DRAIN_DEADLINE_SECS", "");
        assert_eq!(
            resolve_drain_deadline(None),
            Duration::from_secs(DEFAULT_DRAIN_DEADLINE_SECS),
            "empty env falls through to default",
        );
        std::env::set_var("GRX_DRAIN_DEADLINE_SECS", "0");
        assert_eq!(
            resolve_drain_deadline(None),
            Duration::from_secs(DEFAULT_DRAIN_DEADLINE_SECS),
            "zero env falls through to default (would race to instant timeout)",
        );
        std::env::set_var("GRX_DRAIN_DEADLINE_SECS", "garbage");
        assert_eq!(
            resolve_drain_deadline(None),
            Duration::from_secs(DEFAULT_DRAIN_DEADLINE_SECS),
            "unparseable env falls through to default",
        );

        std::env::set_var("GRX_DRAIN_DEADLINE_SECS", "30");
        assert_eq!(
            resolve_drain_deadline(None),
            Duration::from_secs(30),
            "positive env value is accepted as seconds",
        );
        assert_eq!(
            resolve_drain_deadline(Some(5)),
            Duration::from_secs(5),
            "explicit CLI value still wins over env",
        );

        std::env::remove_var("GRX_DRAIN_DEADLINE_SECS");
    }

    /// **M15 Phase 9.** `grx reload <path>` returns an error today —
    /// remote-daemon reload requires M16-Backend's management API.
    /// The error message must redirect users to `grx run --watch`
    /// so scripts fail loudly rather than silently no-op.
    #[test]
    fn run_reload_returns_redirect_error() {
        let result = run_reload(Path::new("foo.grx"));
        let err = result.expect_err("run_reload must error today");
        let msg = format!("{err}");
        assert!(
            msg.contains("--watch"),
            "redirect message must mention --watch: {msg}",
        );
        assert!(
            msg.contains("M16-Backend") || msg.contains("management API"),
            "redirect must explain why this is gated: {msg}",
        );
    }

    /// **M-Prod sub-track #7** — `signature_url_line()` returns an empty
    /// string when `GRX_SIGNATURE_URL` was unset / empty at build time.
    ///
    /// Note on test surface: `option_env!()` resolves at COMPILE time,
    /// so we can only exercise the path that the CI / dev environment
    /// happens to be running under. Local + PR CI (`GRX_SIGNATURE_URL`
    /// unset) hits the empty branch — exercised here. The set-to-a-URL
    /// branch is exercised in the release pipeline (sub-track #8) where
    /// the build env injects `GRX_SIGNATURE_URL`; we lock the rendered
    /// contract via the formatting in `signature_url_line()` itself
    /// (`format!("signature: {url}\n")`) rather than a unit test.
    #[test]
    fn version_omits_signature_when_unset() {
        // build.rs emits `cargo:rustc-env=GRX_SIGNATURE_URL=<value>`
        // unconditionally; `<value>` defaults to "" when the build
        // env didn't set the var. The signature renderer must
        // collapse that to a zero-byte line.
        let line = signature_url_line();
        // EITHER the build env set GRX_SIGNATURE_URL (release-pipeline
        // build — should start with "signature: ") OR it didn't (every
        // other build — must be empty). Branch on what we observe so
        // this test stays green in both worlds.
        match option_env!("GRX_SIGNATURE_URL") {
            None | Some("") => {
                assert!(
                    line.is_empty(),
                    "GRX_SIGNATURE_URL unset/empty must collapse to empty line, got: {line:?}",
                );
                // And the full version output must NOT contain a
                // "signature: " line.
                let version_out = run_version().expect("version ok");
                assert!(
                    !version_out.contains("signature:"),
                    "version output must omit signature line when env unset: {version_out}",
                );
            }
            Some(url) => {
                assert!(
                    line.starts_with("signature: "),
                    "set GRX_SIGNATURE_URL must render `signature: ...`, got: {line:?}",
                );
                assert!(
                    line.contains(url),
                    "rendered line must contain the baked URL: {line:?} vs {url}",
                );
                assert!(
                    line.ends_with('\n'),
                    "rendered line must end with newline: {line:?}",
                );
            }
        }
    }

    /// **M-Prod sub-track #7** — the legacy version output shape is
    /// preserved. Operator scripts that grep `grx version` for `grx <ver>`
    /// or any of the engine-* prefixes must not regress when the
    /// optional signature line is appended.
    #[test]
    fn version_shape_preserved() {
        let out = run_version().expect("version ok");
        // Locked prefixes: anything an operator script might depend on
        // when parsing `grx version`.
        for needle in ["grx ", "parser ", "engine-trait", "engine-null", "engine-echo"] {
            assert!(
                out.contains(needle),
                "version output must contain `{needle}`: {out}",
            );
        }
    }
}
