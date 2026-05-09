//! End-to-end tests for `grx` subcommands. Calls the library entry
//! points directly so tests don't depend on the binary being built.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Echo engine has process-global C state — tests that exercise it
/// must serialise within this test binary.
fn echo_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

use graphreact_cli::{
    cli::{InitTemplate, RuntimeArg},
    commands::{
        format_parse_error, run_check, run_completions, run_fmt, run_init, run_parse, run_run,
        run_version, FmtOutcome,
    },
};

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../graphreact-grx-parser/tests/fixtures");
    p.push(name);
    p
}

#[test]
fn parse_text_summarises_topology() {
    let out = run_parse(&fixture("ping-pong.grx"), false, false).expect("ok");
    assert!(out.contains("graph: ping-pong"));
    assert!(out.contains("ping"));
    assert!(out.contains("pong"));
    assert!(out.contains("Explicit"));
}

#[test]
fn parse_json_outputs_well_formed_json() {
    let out = run_parse(&fixture("pipeline.grx"), true, false).expect("ok");
    let value: serde_json::Value = serde_json::from_str(&out).expect("valid json");
    assert_eq!(value["graphId"], "pipeline");
    assert!(value["nodes"].as_array().unwrap().len() >= 3);
}

#[test]
fn parse_ast_dump_reports_imports_and_definitions() {
    let out = run_parse(&fixture("multi-file/main.grx"), false, true).expect("ok");
    assert!(out.contains("imports: 2"));
    assert!(out.contains("graph"));
}

#[test]
fn check_succeeds_on_valid_file() {
    let out = run_check(&fixture("counter-network.grx")).expect("ok");
    assert!(out.contains("parsed cleanly"));
    assert!(out.contains("counter-network"));
}

#[test]
fn check_succeeds_on_multi_file() {
    let out = run_check(&fixture("multi-file/main.grx")).expect("ok");
    assert!(out.contains("resolved 2 imports"));
    assert!(out.contains("greenhouse"));
}

#[test]
fn check_errors_on_malformed_file() {
    let tmp = tempfile::NamedTempFile::new().expect("tmp");
    std::fs::write(tmp.path(), "<NotAGraph />").expect("write");
    let r = run_check(tmp.path());
    assert!(r.is_err());
}

#[test]
fn fmt_idempotent_returns_already_formatted() {
    let mut tmp = tempfile::NamedTempFile::new().expect("tmp");
    let formatted = "<Graph id=\"x\"></Graph>\n";
    tmp.write_all(formatted.as_bytes()).expect("write");
    // First format normalises.
    let _ = run_fmt(tmp.path(), false).expect("fmt1");
    // Second pass: should be already-formatted.
    let outcome = run_fmt(tmp.path(), true).expect("fmt2");
    assert!(matches!(outcome, FmtOutcome::AlreadyFormatted));
}

#[test]
fn fmt_writes_when_check_off() {
    let tmp = tempfile::NamedTempFile::new().expect("tmp");
    std::fs::write(tmp.path(), "<Graph id=\"x\"><A id=\"a\" /></Graph>")
        .expect("write");
    let outcome = run_fmt(tmp.path(), false).expect("fmt");
    assert!(matches!(outcome, FmtOutcome::Wrote { .. } | FmtOutcome::AlreadyFormatted));
}

#[test]
fn version_reports_engines() {
    let out = run_version().expect("ok");
    assert!(out.contains("grx"));
    assert!(out.contains("engine-null"));
    assert!(out.contains("M12.3"));
}

#[test]
fn run_with_null_engine_reports_topology() {
    let out = run_run(&fixture("ping-pong.grx"), RuntimeArg::Null, None, false).expect("ok");
    assert!(out.contains("engine `null`"));
    assert!(out.contains("ping-pong"));
    assert!(out.contains("2 nodes"));
}

#[test]
fn run_with_null_engine_handles_multi_file() {
    let out = run_run(&fixture("multi-file/main.grx"), RuntimeArg::Null, None, false).expect("ok");
    assert!(out.contains("greenhouse"));
    // Definitions across all imported files should be mounted.
    assert!(out.contains("mounted") || out.contains("definitions"));
}

#[test]
fn run_with_echo_engine_completes() {
    let _g = echo_lock();
    let out = run_run(&fixture("ping-pong.grx"), RuntimeArg::Echo, None, false).expect("ok");
    assert!(out.contains("engine `echo-c`"));
    assert!(out.contains("ping-pong"));
}

#[test]
fn run_with_echo_engine_handles_multi_file_definitions() {
    let _g = echo_lock();
    let out = run_run(&fixture("multi-file/main.grx"), RuntimeArg::Echo, None, false).expect("ok");
    assert!(out.contains("echo-c"));
    assert!(out.contains("greenhouse"));
    assert!(out.contains("4 definitions") || out.contains("mounted"));
}

#[test]
#[cfg(target_os = "macos")]
fn run_with_jsc_engine_dry_mounts_on_macos() {
    let _g = jsc_lock();
    // M4-followon (2026-05-02) closed the scheduler's TTL-inheritance
    // gap: edge-attribute TTL (`<edge ttl={5}/>`) now caps emissions on
    // that edge, and `make_emit` falls back to the from-node's
    // last-received TTL across the dirty → re-evaluate → flush_effects
    // boundary. ping-pong runs to natural termination on JSC; the
    // phase-3d-era `MaxDepthExceeded` fallback is no longer needed.
    let out = run_run(&fixture("ping-pong.grx"), RuntimeArg::Jsc, None, false).expect("jsc run ok");
    assert!(out.contains("jsc"), "expected engine name `jsc` in output: {out}");
    assert!(out.contains("ping-pong"), "expected graph id in output: {out}");
}

#[test]
#[cfg(not(target_os = "macos"))]
fn run_with_jsc_engine_returns_not_linked_error_off_macos() {
    let r = run_run(&fixture("ping-pong.grx"), RuntimeArg::Jsc, None, false);
    let err = r.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("M12.3"), "msg: {msg}");
}

/// JSC's `JSContextGroup` is process-global; serialise tests that
/// touch it within this binary.
#[cfg(target_os = "macos")]
fn jsc_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn completions_zsh_emits_compdef_block() {
    let out = run_completions(clap_complete::Shell::Zsh).expect("ok");
    assert!(out.contains("#compdef grx"), "missing compdef header: {out}");
    assert!(out.contains("run"), "subcommand `run` should appear in completions");
    assert!(out.contains("init"), "subcommand `init` should appear in completions");
    assert!(out.contains("completions"), "subcommand `completions` itself should appear");
}

#[test]
fn completions_bash_emits_complete_call() {
    let out = run_completions(clap_complete::Shell::Bash).expect("ok");
    assert!(out.contains("complete -F"), "missing `complete -F` directive: {out}");
    assert!(out.contains("grx"), "binary name should appear");
}

#[test]
fn init_hello_creates_directory_with_grx_and_readme() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("demo");
    let out = run_init(&target, InitTemplate::Hello).expect("ok");
    assert!(out.contains("scaffolded"));
    assert!(target.join("hello.grx").exists());
    assert!(target.join("README.md").exists());
    let grx = std::fs::read_to_string(target.join("hello.grx")).expect("read");
    assert!(grx.contains("<Graph id=\"hello\">"));
    assert!(grx.contains("<Greeter>"));
}

#[test]
fn init_pingpong_creates_pingpong_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("demo");
    run_init(&target, InitTemplate::PingPong).expect("ok");
    assert!(target.join("ping-pong.grx").exists());
    let grx = std::fs::read_to_string(target.join("ping-pong.grx")).expect("read");
    assert!(grx.contains("<Pinger>"));
    assert!(grx.contains("<Ponger>"));
}

#[test]
fn init_refuses_existing_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("already-here");
    std::fs::create_dir_all(&target).expect("mkdir");
    let err = run_init(&target, InitTemplate::Hello).expect_err("expected error");
    let msg = format!("{err}");
    assert!(msg.contains("already exists"), "msg: {msg}");
}

#[test]
fn init_hello_output_runs_through_check() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("demo");
    run_init(&target, InitTemplate::Hello).expect("ok");
    let out = run_check(&target.join("hello.grx")).expect("check ok");
    assert!(out.contains("parsed cleanly"));
}

#[test]
fn parser_error_snippet_contains_line_and_caret() {
    // Force a lex/parse error and confirm format_parse_error renders
    // the source line with a caret pointing at the column.
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    // Two missing-id errors and a malformed graph; parser should reject.
    write!(tmp.as_file(), "<Graph>\n  <NodeType id=\"X\" />\n</Graph>\n").expect("write");
    let err = run_check(tmp.path()).unwrap_err();
    let parse_err = match err {
        graphreact_cli::commands::CommandError::Parser(e) => e,
        other => panic!("expected Parser error, got {other:?}"),
    };
    let rendered = format_parse_error(&parse_err, tmp.path());
    assert!(rendered.starts_with("error:"), "rendered: {rendered}");
    assert!(rendered.contains(" --> "), "missing source-pointer line: {rendered}");
    assert!(rendered.contains('^'), "missing caret: {rendered}");
}
