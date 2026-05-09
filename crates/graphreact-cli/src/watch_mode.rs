//! **M15 Phase 9.** `grx run --watch` driver: file watcher → diff
//! classifier → engine adapter → reload orchestrator → re-render.
//!
//! End-to-end demo of the M15 stack. After the initial mount + first
//! `process_batch_async`, the watch loop watches the resolved `.grx`
//! source plus any imported files; on each [`WatchEvent::Modified`]
//! it re-resolves the IR, classifies the delta against the prior IR,
//! applies via [`graphreact_hotreload::ReloadOrchestrator`], and
//! drives a fresh `process_batch_async` so the new bodies render.
//!
//! # Architectural notes
//!
//! - **JSC and CPython runtimes are `!Send`** (each holds
//!   `Rc<RefCell<Engine>>`). The watch loop runs on a current-thread
//!   tokio runtime hosted on a dedicated OS thread, mirroring the
//!   existing `commands::run_run` pattern. The notify-driven coalesce
//!   task spawned by [`FileWatcher::watch`] uses `tokio::spawn`,
//!   which schedules onto the active runtime; the coalesce task's
//!   future is `Send` so it runs cleanly on the current-thread
//!   driver.
//! - **Multi-file imports are watched.** `resolve_multi_file` returns
//!   the root path plus every transitively-imported path; we register
//!   a watch on each. Phase 1 (tree-sitter incremental) refines this
//!   to skip-unchanged via `mtime + tree-id` cache.
//! - **Engine adapters share the runtime's isolate** via
//!   `for_runtime(&runtime)` so per-isolate state — JS-side
//!   `__hostCallbacks` token registry, the Rust-side `MEMO_REGISTRY`
//!   cache (keyed on `(node_id, hook_pos)`), the `SIGNAL_REGISTRY`
//!   slots, the CPython sub-interpreter's sharded `CALLBACK_REGISTRY`
//!   — survives every swap. This is what makes "compatible-change
//!   reload preserves `useState`" work end-to-end.
//! - **Parse failures are recoverable.** A half-saved file (mid-edit
//!   typo) makes `resolve_multi_file` return a parse error; the watch
//!   loop logs a warning and waits for the next event rather than
//!   crashing the daemon. Same shape as M-Prod's "transient
//!   filesystem state shouldn't kill production processes."
//! - **SIGINT exits cleanly.** Mirrors the cluster daemon's
//!   `drive_handle_until` pattern; the runtime is shut down
//!   explicitly so engine FFI cleanup runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use graphreact_grx_parser::{GrxIr, ParseCache};
use graphreact_hotreload::{
    classify, DiffCache, FileWatcher, ReloadDelta, ReloadOrchestrator, ReloadOutcome, WatchEvent,
};

use crate::cli::RuntimeArg;
use crate::commands::CommandError;
use crate::runtime::{resolve_and_assemble_ir_cached, RuntimeError};

/// Reject runtimes that have no body to swap. Echo plus null are
/// surface-only — the orchestrator's `EngineAdapter` would have
/// nothing meaningful to compile against.
pub fn validate_watch_runtime(runtime: RuntimeArg) -> Result<(), CommandError> {
    match runtime {
        RuntimeArg::Jsc | RuntimeArg::Cpython => Ok(()),
        RuntimeArg::Null | RuntimeArg::Echo => Err(CommandError::Runtime(
            RuntimeError::Engine(graphreact_engine_zig::EngineError::Internal(format!(
                "`--watch` requires `--runtime jsc` or `--runtime cpython`; got `{runtime}`. \
                 The {runtime} engine has no body to swap.",
            ))),
        )),
    }
}

/// **M15 Phase 9.** Top-level dispatcher for `grx run --watch <path>`.
///
/// Selects the engine arm, mounts, and drives the watch loop until
/// SIGINT (or a fatal engine error). Hosted on a dedicated OS thread
/// plus a current-thread tokio runtime so the runtime's `!Send`
/// guarantees compose with `LocalSet`.
pub fn run_watch_blocking(
    path: PathBuf,
    runtime: RuntimeArg,
    debug: Option<crate::debug::DebugFlag>,
    debug_wait: bool,
) -> Result<(), CommandError> {
    validate_watch_runtime(runtime)?;

    match runtime {
        #[cfg(target_os = "macos")]
        RuntimeArg::Jsc => {
            // **M16-Backend P9.** JSC has no wait-for-client semantic
            // (lockdownd discovery is silent), so `debug_wait` is
            // a no-op for JSC; documented in m16-debug-jsc.md. The
            // inspector install fires inside `run_jsc_watch_blocking`
            // before the first `process_batch_async` so Safari Web
            // Inspector can attach + breakpoints hit on first render.
            let _ = debug_wait;
            run_jsc_watch_blocking(path, debug)
        }
        #[cfg(target_os = "macos")]
        RuntimeArg::Cpython => run_cpython_watch_blocking(path, debug, debug_wait),
        #[cfg(not(target_os = "macos"))]
        RuntimeArg::Jsc | RuntimeArg::Cpython => Err(CommandError::Runtime(
            RuntimeError::Engine(graphreact_engine_zig::EngineError::Internal(
                "JSC plus CPython engines are macOS-only in M12.3a/M13. Linux plus Windows land at M12.3b.".into(),
            )),
        )),
        RuntimeArg::Null | RuntimeArg::Echo => unreachable!("validated above"),
    }
}

#[cfg(target_os = "macos")]
fn run_jsc_watch_blocking(
    path: PathBuf,
    debug: Option<crate::debug::DebugFlag>,
) -> Result<(), CommandError> {
    std::thread::spawn(move || -> Result<(), CommandError> {
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
            let mut runtime = crate::jsruntime::mount_with_scheduler(&path)
                .map_err(CommandError::Runtime)?;
            // **M16-Backend P9.** Install JSC inspector BEFORE the
            // first `process_batch_async` so Safari Web Inspector
            // sees the JSContext + breakpoints fire on first render.
            // The inspector install is idempotent at the framework
            // level + at the JscEngine `AtomicBool` guard, so reload
            // cycles (which re-mount but re-use the same JscEngine)
            // are safe.
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
            runtime
                .process_batch_async()
                .await
                .map_err(crate::runtime::RuntimeError::from)
                .map_err(CommandError::Runtime)?;

            let engine_version = {
                use graphreact_engine_zig::Engine;
                runtime.engine.borrow().engine_version()
            };
            print_mount_banner(
                "jsc",
                &engine_version,
                &runtime.graph_id,
                runtime.n_nodes,
                runtime.n_edges,
                runtime.n_definitions,
                &runtime.mounted,
            );

            let adapter = crate::hotreload_adapter::JscReloadAdapter::for_runtime(&runtime);
            let mut orchestrator = ReloadOrchestrator::new(adapter);
            let watch_paths = collect_watch_paths(&path)?;
            let (_watcher, rx) = FileWatcher::watch(&watch_paths)
                .map_err(|e| CommandError::Io(std::io::Error::other(format!(
                    "FileWatcher::watch failed: {e}"
                ))))?;

            // **M15 Phase 1 slice 1A.** Per-process parse cache so
            // unchanged imports skip reparse on every reload event.
            // Pre-populated by the initial IR snapshot below; the
            // watch loop reuses it across reloads.
            let mut parse_cache = ParseCache::new();

            // Snapshot the initial IR for the diff classifier.
            let (_, initial_ir) = resolve_and_assemble_ir_cached(&path, &mut parse_cache)
                .map_err(CommandError::Runtime)?;

            eprintln!(
                "[grx watch] watching {} path(s); save your `.grx` to reload",
                watch_paths.len()
            );

            drive_jsc_watch_loop(
                &mut runtime,
                &mut orchestrator,
                &path,
                initial_ir,
                &mut parse_cache,
                rx,
            )
            .await?;

            let _ = runtime.shutdown();
            Ok(())
        })
    })
    .join()
    .map_err(|_| CommandError::Io(std::io::Error::other("jsc watch thread panicked")))?
}

#[cfg(target_os = "macos")]
fn run_cpython_watch_blocking(
    path: PathBuf,
    debug: Option<crate::debug::DebugFlag>,
    debug_wait: bool,
) -> Result<(), CommandError> {
    std::thread::spawn(move || -> Result<(), CommandError> {
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
            let mut runtime = crate::pyruntime::mount_with_scheduler(&path)
                .map_err(CommandError::Runtime)?;
            // **M16-Backend P8.** Install debugpy BEFORE the first
            // `process_batch_async` so `def init(ctx)` breakpoints
            // hit on first mount. Same pattern as `start_daemon`.
            if let Some(d) = debug {
                let mut engine = runtime.engine.borrow_mut();
                crate::debug::cpython_debugpy::install_debugpy(
                    &mut engine,
                    runtime.iso,
                    d.port,
                    debug_wait,
                )
                .map_err(|e| {
                    CommandError::ClusterArg(format!("debug install failed: {e}"))
                })?;
            }
            runtime
                .process_batch_async()
                .await
                .map_err(crate::runtime::RuntimeError::from)
                .map_err(CommandError::Runtime)?;

            let engine_version = {
                use graphreact_engine_zig::Engine;
                runtime.engine.borrow().engine_version()
            };
            print_mount_banner(
                "cpython",
                &engine_version,
                &runtime.graph_id,
                runtime.n_nodes,
                runtime.n_edges,
                runtime.n_definitions,
                &runtime.mounted,
            );

            let adapter = crate::hotreload_adapter::PyReloadAdapter::for_runtime(&runtime);
            let mut orchestrator = ReloadOrchestrator::new(adapter);
            let watch_paths = collect_watch_paths(&path)?;
            let (_watcher, rx) = FileWatcher::watch(&watch_paths)
                .map_err(|e| CommandError::Io(std::io::Error::other(format!(
                    "FileWatcher::watch failed: {e}"
                ))))?;

            // **M15 Phase 1 slice 1A.** Same cache plumbing as the
            // JSC arm — Python authors editing multi-file fixtures
            // get the same partial-reparse speedup.
            let mut parse_cache = ParseCache::new();
            let (_, initial_ir) = resolve_and_assemble_ir_cached(&path, &mut parse_cache)
                .map_err(CommandError::Runtime)?;

            eprintln!(
                "[grx watch] watching {} path(s); save your `.grx` to reload",
                watch_paths.len()
            );

            drive_cpython_watch_loop(
                &mut runtime,
                &mut orchestrator,
                &path,
                initial_ir,
                &mut parse_cache,
                rx,
            )
            .await?;

            let _ = runtime.shutdown();
            Ok(())
        })
    })
    .join()
    .map_err(|_| CommandError::Io(std::io::Error::other("cpython watch thread panicked")))?
}

/// Per-engine watch loop driver. The two engines have parallel shapes
/// — same logic, different `NodeRuntime` types — so we keep them as
/// thin wrappers around the orchestrator-plus-render pattern.
///
/// `runtime` is borrowed across `await`s on `process_batch_async`;
/// because `JsRuntime` is `!Send`, the surrounding executor must be
/// a current-thread runtime + LocalSet.
#[cfg(target_os = "macos")]
async fn drive_jsc_watch_loop(
    runtime: &mut crate::jsruntime::JsRuntime,
    orchestrator: &mut ReloadOrchestrator<crate::hotreload_adapter::JscReloadAdapter>,
    root_path: &Path,
    mut current_ir: GrxIr,
    parse_cache: &mut ParseCache,
    mut events: tokio::sync::mpsc::Receiver<WatchEvent>,
) -> Result<(), CommandError> {
    let mut diff_cache = DiffCache::new();
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            _ = &mut signal => {
                eprintln!("[grx watch] received SIGINT; tearing down");
                return Ok(());
            }
            maybe_event = events.recv() => {
                let Some(event) = maybe_event else {
                    eprintln!("[grx watch] watcher channel closed; exiting");
                    return Ok(());
                };
                if !is_relevant(&event) {
                    continue;
                }
                match handle_event(root_path, &current_ir, &mut diff_cache, parse_cache, &event) {
                    Ok((new_ir, delta)) => {
                        let outcome = orchestrator.apply(&mut runtime.scheduler, delta);
                        log_reload_outcome(&event, &outcome, parse_cache);
                        if let Err(e) = runtime.process_batch_async().await {
                            eprintln!("[grx watch] post-reload render error: {e}");
                        }
                        current_ir = new_ir;
                    }
                    Err(WatchHandleError::DeferToNextEvent(reason)) => {
                        eprintln!("[grx watch] {reason}; deferring to next event");
                    }
                    Err(WatchHandleError::Fatal(e)) => return Err(e),
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
async fn drive_cpython_watch_loop(
    runtime: &mut crate::pyruntime::PyRuntime,
    orchestrator: &mut ReloadOrchestrator<crate::hotreload_adapter::PyReloadAdapter>,
    root_path: &Path,
    mut current_ir: GrxIr,
    parse_cache: &mut ParseCache,
    mut events: tokio::sync::mpsc::Receiver<WatchEvent>,
) -> Result<(), CommandError> {
    let mut diff_cache = DiffCache::new();
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            _ = &mut signal => {
                eprintln!("[grx watch] received SIGINT; tearing down");
                return Ok(());
            }
            maybe_event = events.recv() => {
                let Some(event) = maybe_event else {
                    eprintln!("[grx watch] watcher channel closed; exiting");
                    return Ok(());
                };
                if !is_relevant(&event) {
                    continue;
                }
                match handle_event(root_path, &current_ir, &mut diff_cache, parse_cache, &event) {
                    Ok((new_ir, delta)) => {
                        let outcome = orchestrator.apply(&mut runtime.scheduler, delta);
                        log_reload_outcome(&event, &outcome, parse_cache);
                        if let Err(e) = runtime.process_batch_async().await {
                            eprintln!("[grx watch] post-reload render error: {e}");
                        }
                        current_ir = new_ir;
                    }
                    Err(WatchHandleError::DeferToNextEvent(reason)) => {
                        eprintln!("[grx watch] {reason}; deferring to next event");
                    }
                    Err(WatchHandleError::Fatal(e)) => return Err(e),
                }
            }
        }
    }
}

/// Internal classification of a watch event into "skip", "act", or
/// "defer (transient state — wait)".
#[derive(Debug)]
enum WatchHandleError {
    /// Filesystem snapshot is in a bad shape (mid-save typo, missing
    /// import). Log + wait for the next event rather than crashing.
    DeferToNextEvent(String),
    /// Error the daemon can't recover from (e.g. permissions, runtime
    /// death). Surface up.
    Fatal(CommandError),
}

/// Re-resolve from disk plus classify against the prior IR. Pulled
/// into a helper so both engine arms share the same parse-error
/// recovery shape.
///
/// **M15 Phase 1 slice 1A.** `parse_cache` is mtime-keyed; on each
/// reload event the resolver skips reparse for unchanged imports.
/// Stats are reset before each call so `log_reload_outcome` can
/// surface per-event hit/miss counts.
fn handle_event(
    root_path: &Path,
    current_ir: &GrxIr,
    diff_cache: &mut DiffCache,
    parse_cache: &mut ParseCache,
    event: &WatchEvent,
) -> Result<(GrxIr, ReloadDelta), WatchHandleError> {
    match event {
        WatchEvent::Removed { path } => Err(WatchHandleError::DeferToNextEvent(format!(
            "watched file removed: {}",
            path.display()
        ))),
        WatchEvent::Modified { path } => {
            // Per-event stats — reset so `log_reload_outcome` reports
            // cache behavior for THIS reload, not the cumulative
            // counters across the lifetime of the daemon.
            parse_cache.reset_stats();
            let (_, new_ir) = match resolve_and_assemble_ir_cached(root_path, parse_cache) {
                Ok(t) => t,
                Err(RuntimeError::Parser(e)) => {
                    return Err(WatchHandleError::DeferToNextEvent(format!(
                        "parse error after edit to {}: {e}",
                        path.display()
                    )));
                }
                Err(RuntimeError::Io(e)) => {
                    return Err(WatchHandleError::DeferToNextEvent(format!(
                        "io error reading {} after edit: {e}",
                        path.display()
                    )));
                }
                Err(other) => return Err(WatchHandleError::Fatal(CommandError::Runtime(other))),
            };
            let delta = classify(current_ir, &new_ir, diff_cache);
            Ok((new_ir, delta))
        }
    }
}

/// Filter events to only `.grx` paths. The watcher is registered on
/// the parent directories of every resolved file, so it fires for
/// every save in those directories — most won't be `.grx` files.
fn is_relevant(event: &WatchEvent) -> bool {
    let path = match event {
        WatchEvent::Modified { path } | WatchEvent::Removed { path } => path,
    };
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("grx")
    )
}

/// Build the union of paths to register with the file watcher: the
/// root file's parent directory plus the parent directory of every
/// import file. Watching the parent directory plus filtering by
/// extension robustly captures atomic-save sequences (vim, emacs,
/// JetBrains) which surface as create/rename events on the directory
/// rather than direct events on the file path.
fn collect_watch_paths(root_path: &Path) -> Result<Vec<PathBuf>, CommandError> {
    let resolved = graphreact_grx_parser::multi_file::resolve_multi_file(root_path)
        .map_err(|e| CommandError::Runtime(RuntimeError::Parser(e)))?;
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    if let Some(parent) = resolved.root_path.parent() {
        dirs.insert(parent.to_path_buf());
    }
    for imp in &resolved.imports {
        if let Some(parent) = imp.path.parent() {
            dirs.insert(parent.to_path_buf());
        }
    }
    if dirs.is_empty() {
        // Single-file in cwd with no parent (rare); fall back to the
        // file path itself.
        dirs.insert(root_path.to_path_buf());
    }
    Ok(dirs.into_iter().collect())
}

/// Print a one-line summary per reload event, mirroring the cluster
/// daemon's `grx-daemon-ready` plus `grx-daemon-exited` style. Tests
/// scrape this output to assert reload behavior end-to-end.
///
/// **M15 Phase 1 slice 1A.** Cache stats appended at the tail —
/// `cache=Hh/Mm` reports per-event hits + misses so users see
/// "edit one file in a 4-file fixture → cache=3h/1m" feedback that
/// the multi-file caching is working.
fn log_reload_outcome(event: &WatchEvent, outcome: &ReloadOutcome, parse_cache: &ParseCache) {
    use std::fmt::Write as _;
    let path = match event {
        WatchEvent::Modified { path } | WatchEvent::Removed { path } => path.display(),
    };
    let mut summary = format!("[grx watch] {path}: nodes=");
    if outcome.nodes.is_empty() {
        summary.push_str("none");
    } else {
        summary.push_str(
            &outcome
                .nodes
                .iter()
                .map(|(id, kind)| format!("{id}:{kind:?}"))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    if !outcome.edges.is_empty() {
        let _ = write!(summary, " edges={}", outcome.edges.len());
    }
    if let Some(rb) = &outcome.rollback {
        let _ = write!(
            summary,
            " rollback=restored:{}/failed:{}",
            rb.restored,
            rb.failed.len()
        );
    }
    if !outcome.errors.is_empty() {
        let _ = write!(summary, " errors={}", outcome.errors.len());
    }
    if outcome.is_success() {
        summary.push_str(" status=ok");
    } else {
        summary.push_str(" status=err");
    }
    // **M15 Phase 1 slice 1A.** Per-event ParseCache hit/miss
    // counts. Reset by `handle_event` before each resolution so
    // these numbers reflect THIS reload, not cumulative.
    let _ = write!(
        summary,
        " cache={}h/{}m",
        parse_cache.hits(),
        parse_cache.misses()
    );
    eprintln!("{summary}");
}

/// Render the mount banner identical to `commands::run_run`'s output
/// shape so users see the same first-tick summary regardless of
/// whether they passed `--watch`.
fn print_mount_banner(
    engine_name: &str,
    engine_version: &str,
    graph_id: &str,
    n_nodes: usize,
    n_edges: usize,
    n_definitions: usize,
    mounted_components: &[String],
) {
    println!("running on engine `{engine_name}` ({engine_version})");
    println!("  graph `{graph_id}`: {n_nodes} nodes, {n_edges} edges, {n_definitions} definitions");
    if !mounted_components.is_empty() {
        println!(
            "  mounted {} components: {}",
            mounted_components.len(),
            mounted_components.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cli::RuntimeArg;

    #[test]
    fn validate_watch_runtime_accepts_engine_runtimes() {
        assert!(validate_watch_runtime(RuntimeArg::Jsc).is_ok());
        assert!(validate_watch_runtime(RuntimeArg::Cpython).is_ok());
    }

    #[test]
    fn validate_watch_runtime_rejects_null() {
        let err = validate_watch_runtime(RuntimeArg::Null).expect_err("null must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("watch"), "error must mention --watch: {msg}");
        assert!(msg.contains("null"), "error must name the rejected runtime: {msg}");
    }

    #[test]
    fn validate_watch_runtime_rejects_echo() {
        let err = validate_watch_runtime(RuntimeArg::Echo).expect_err("echo must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("echo"));
    }

    /// `handle_event(Modified)` re-resolves the file from disk and
    /// classifies the delta against the prior IR. Identical contents
    /// produce `ReloadDelta::NoChange`.
    #[test]
    fn handle_event_modified_with_unchanged_content_yields_nochange() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = dir.path().join("a.grx");
        let source = r#"<C lang="js">
return { accepts: [], emits: [] };
</C>

<Graph id="g">
  <C id="n1" />
</Graph>
"#;
        std::fs::write(&fixture, source).expect("write");
        let prior = graphreact_grx_parser::ir::parse_to_ir(source).expect("parse prior");
        let mut cache = DiffCache::new();
        let mut parse_cache = ParseCache::new();
        let event = WatchEvent::Modified { path: fixture.clone() };
        let (_new_ir, delta) =
            handle_event(&fixture, &prior, &mut cache, &mut parse_cache, &event).expect("handle ok");
        assert!(
            matches!(delta, ReloadDelta::NoChange),
            "identical content must produce NoChange, got {delta:?}"
        );
    }

    /// `handle_event(Modified)` on a file with a body-source change
    /// produces a non-empty delta (Incompatible under conservative
    /// body-hash mode; Phase 3 promotes string-literal-only changes
    /// to Compatible).
    #[test]
    fn handle_event_modified_with_body_change_produces_delta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = dir.path().join("a.grx");
        let v2 = r#"<C lang="js">
return { accepts: ['v2'], emits: [] };
</C>

<Graph id="g">
  <C id="n1" />
</Graph>
"#;
        std::fs::write(&fixture, v2).expect("write v2");
        let v1_source = r#"<C lang="js">
return { accepts: ['v1'], emits: [] };
</C>

<Graph id="g">
  <C id="n1" />
</Graph>
"#;
        let prior = graphreact_grx_parser::ir::parse_to_ir(v1_source).expect("parse prior");
        let mut cache = DiffCache::new();
        let mut parse_cache = ParseCache::new();
        let event = WatchEvent::Modified { path: fixture.clone() };
        let (_new_ir, delta) =
            handle_event(&fixture, &prior, &mut cache, &mut parse_cache, &event).expect("handle ok");
        assert!(
            !matches!(delta, ReloadDelta::NoChange),
            "body change must produce a non-NoChange delta, got {delta:?}"
        );
    }

    /// Mid-edit transient state: a file with parse errors must NOT
    /// crash the watch loop. `handle_event` returns
    /// `WatchHandleError::DeferToNextEvent` so the daemon continues.
    #[test]
    fn handle_event_parse_failure_defers_to_next_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = dir.path().join("a.grx");
        std::fs::write(&fixture, "<<<not valid grx>>>").expect("write garbage");
        let prior = graphreact_grx_parser::ir::parse_to_ir(
            r#"<C lang="js">
return { accepts: [], emits: [] };
</C>

<Graph id="g">
  <C id="n1" />
</Graph>
"#,
        )
        .expect("parse prior");
        let mut cache = DiffCache::new();
        let mut parse_cache = ParseCache::new();
        let event = WatchEvent::Modified { path: fixture.clone() };
        match handle_event(&fixture, &prior, &mut cache, &mut parse_cache, &event) {
            Err(WatchHandleError::DeferToNextEvent(reason)) => {
                assert!(reason.contains("parse error"), "reason should mention parse: {reason}");
            }
            other => panic!("expected DeferToNextEvent on parse failure, got {other:?}"),
        }
    }

    /// `handle_event(Removed)` defers — operators may be moving the
    /// file via atomic-save, and shutting down the daemon on a
    /// transient remove would surprise them.
    #[test]
    fn handle_event_removed_defers_to_next_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = dir.path().join("missing.grx");
        let prior = graphreact_grx_parser::ir::parse_to_ir(
            r#"<C lang="js">
return { accepts: [], emits: [] };
</C>

<Graph id="g">
  <C id="n1" />
</Graph>
"#,
        )
        .expect("parse prior");
        let mut cache = DiffCache::new();
        let mut parse_cache = ParseCache::new();
        let event = WatchEvent::Removed { path: fixture.clone() };
        match handle_event(&fixture, &prior, &mut cache, &mut parse_cache, &event) {
            Err(WatchHandleError::DeferToNextEvent(reason)) => {
                assert!(reason.contains("removed"), "reason should mention removed: {reason}");
            }
            other => panic!("expected DeferToNextEvent on removed, got {other:?}"),
        }
    }

    /// **M15 Phase 1 slice 1A.** Two consecutive `handle_event`
    /// calls on a multi-file fixture share the parse cache —
    /// editing only the root file in a 2-file fixture yields one
    /// hit (the unchanged import) plus one miss (the edited root)
    /// per event after the initial population.
    #[test]
    fn handle_event_parse_cache_hits_unchanged_imports() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root.grx");
        let leaf = dir.path().join("leaf.grx");

        let root_v1 = r#"<import src="./leaf.grx" />
<C lang="js">
return { accepts: ['v1'], emits: [] };
</C>

<Graph id="g">
  <C id="n1" />
</Graph>
"#;
        let root_v2 = r#"<import src="./leaf.grx" />
<C lang="js">
return { accepts: ['v2'], emits: [] };
</C>

<Graph id="g">
  <C id="n1" />
</Graph>
"#;
        std::fs::write(&root, root_v1).expect("write root v1");
        std::fs::write(&leaf, r"<L>const x = 1;</L>").expect("write leaf");

        let prior = graphreact_grx_parser::ir::parse_to_ir(root_v1).expect("parse prior");
        let mut diff_cache = DiffCache::new();
        let mut parse_cache = ParseCache::new();

        // First event: writing root_v2 + invoking handle_event.
        // Wait briefly so the rewrite gets a distinct mtime relative
        // to the initial write above.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&root, root_v2).expect("write root v2");
        let event = WatchEvent::Modified {
            path: root.clone(),
        };
        let _ = handle_event(&root, &prior, &mut diff_cache, &mut parse_cache, &event)
            .expect("first handle_event");
        // First event after a fresh cache: 2 misses (root + leaf).
        assert_eq!(parse_cache.misses(), 2);
        assert_eq!(parse_cache.hits(), 0);

        // Second event: only the root changes; leaf stays
        // unchanged. handle_event resets stats per call.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let root_v3 = r#"<import src="./leaf.grx" />
<C lang="js">
return { accepts: ['v3'], emits: [] };
</C>

<Graph id="g">
  <C id="n1" />
</Graph>
"#;
        std::fs::write(&root, root_v3).expect("write root v3");
        let event = WatchEvent::Modified {
            path: root.clone(),
        };
        let _ = handle_event(&root, &prior, &mut diff_cache, &mut parse_cache, &event)
            .expect("second handle_event");
        // Second event: leaf.grx hits (unchanged), root.grx misses
        // (mtime moved). handle_event reset stats first so these
        // are per-event counts.
        assert_eq!(
            parse_cache.hits(),
            1,
            "leaf should hit (unchanged across events)"
        );
        assert_eq!(parse_cache.misses(), 1, "root should miss (re-edited)");
    }

    /// `is_relevant` filters out non-`.grx` files that show up on the
    /// parent-directory watch (transient editor artifacts already
    /// pre-filtered by the watcher itself; this is the second-line
    /// filter).
    #[test]
    fn is_relevant_filters_non_grx_paths() {
        assert!(is_relevant(&WatchEvent::Modified { path: "/tmp/a.grx".into() }));
        assert!(is_relevant(&WatchEvent::Removed { path: "/tmp/a.grx".into() }));
        assert!(!is_relevant(&WatchEvent::Modified { path: "/tmp/a.txt".into() }));
        assert!(!is_relevant(&WatchEvent::Modified { path: "/tmp/.DS_Store".into() }));
    }

    /// **M15 Phase 9 in-process integration test (JSC).** Synthesises
    /// the `WatchEvent` path that the file watcher would otherwise
    /// generate, drives the orchestrator end-to-end against a real
    /// `JsRuntime`, and pins down the load-bearing semantic — the
    /// post-reload pipeline runs cleanly without crashing the daemon.
    ///
    /// The test bypasses `notify` deliberately: notify-driven tests
    /// re-introduce the macOS FSEvents parallel-test flakiness already
    /// documented in the watcher's `#[ignore]`'d tests. The watcher's
    /// own dedicated tests cover that surface; this test pins down
    /// the `commands.rs` plus `watch_mode.rs` glue.
    ///
    /// Note on scope: under the conservative body-hash classifier
    /// shipped before Phase 3, any body source change classifies as
    /// `BodyChangedIncompatible` (drain plus remount). Phase 3
    /// (per-language hook-AST diffing) is what promotes
    /// string-literal-only changes to Compatible (preserving
    /// `useState`). For Phase 9 we assert the watch driver successfully
    /// drives the Incompatible path — the reload completes without
    /// crashing the daemon, the new body's manifest accepts list is
    /// active. State preservation across body edits lands at Phase 3.
    #[cfg(target_os = "macos")]
    #[test]
    fn jsc_watch_loop_drives_reload_pipeline_end_to_end() {
        let _g = crate::jsruntime::engine_lock();
        crate::jsruntime::reset_global_state();

        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = dir.path().join("counter.grx");
        let v1_source = r#"<Counter lang="js">
useState(0);
return { accepts: ['v1'], emits: [] };
</Counter>

<Graph id="watch-counter">
  <Counter id="c1" />
</Graph>
"#;
        std::fs::write(&fixture, v1_source).expect("write v1");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let mut runtime =
                crate::jsruntime::mount_with_scheduler(&fixture).expect("mount v1");
            runtime
                .process_batch_async()
                .await
                .expect("first batch under v1");

            // The body executed at least once — the slot table will
            // have an entry for ("c1", 0).
            assert!(
                crate::jsruntime::signal_id_for("c1", 0).is_some(),
                "v1's useState body should have minted a signal slot",
            );

            // Re-write the file with a body change. Conservative
            // body-hash classifier flags this as Incompatible.
            let v2_source = r#"<Counter lang="js">
useState(0);
return { accepts: ['v2'], emits: [] };
</Counter>

<Graph id="watch-counter">
  <Counter id="c1" />
</Graph>
"#;
            std::fs::write(&fixture, v2_source).expect("write v2");

            // The "before" IR snapshot is what the runtime mounted
            // with — the v1 source string in memory (the file on disk
            // is now at v2).
            let v1_ir =
                graphreact_grx_parser::ir::parse_to_ir(v1_source).expect("parse v1 ir");

            let adapter = crate::hotreload_adapter::JscReloadAdapter::for_runtime(&runtime);
            let mut orchestrator = ReloadOrchestrator::new(adapter);
            let mut diff_cache = DiffCache::new();
            let mut parse_cache = ParseCache::new();

            let event = WatchEvent::Modified { path: fixture.clone() };
            let (_new_ir, delta) = handle_event(
                &fixture,
                &v1_ir,
                &mut diff_cache,
                &mut parse_cache,
                &event,
            )
            .expect("handle ok on v2");

            // Conservative classifier: body source differs across
            // v1 plus v2 plus so this is BodyChangedIncompatible.
            // Phase 3 promotes literal-only changes to Compatible.
            let outcome = orchestrator.apply(&mut runtime.scheduler, delta);
            assert!(
                outcome.is_success(),
                "reload outcome must succeed (drain plus remount path is supported by Phase 8): errors={:?}",
                outcome.errors,
            );

            // Drive the new body. The post-reload render must not
            // crash; this is the load-bearing daemon-stability gate
            // for Phase 9.
            runtime
                .process_batch_async()
                .await
                .expect("post-reload batch under v2");

            // After Incompatible reload (drain plus remount) the new
            // node has fresh hook slots. SIGNAL_BY_HOOK_POS may have
            // a fresh entry for ("c1", 0) — check existence rather
            // than identity (Phase 3 is what makes identity stable).
            assert!(
                crate::jsruntime::signal_id_for("c1", 0).is_some(),
                "v2's useState body should have re-minted a signal slot post-remount",
            );

            runtime.shutdown().expect("shutdown");
        });
    }

    /// `collect_watch_paths` deduplicates parent directories across
    /// the root file plus its imports. For a single-file fixture in
    /// a tempdir, that's exactly one path.
    #[test]
    fn collect_watch_paths_deduplicates_to_parent_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = dir.path().join("solo.grx");
        std::fs::write(
            &fixture,
            r#"<C lang="js">
return { accepts: [], emits: [] };
</C>

<Graph id="g">
  <C id="n1" />
</Graph>
"#,
        )
        .expect("write");
        let paths = collect_watch_paths(&fixture).expect("collect");
        assert_eq!(paths.len(), 1, "single-file fixture yields one parent dir");
        assert_eq!(
            paths[0].canonicalize().expect("canon"),
            dir.path().canonicalize().expect("canon parent")
        );
    }
}
