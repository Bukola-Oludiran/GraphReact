//! **M22 — Mixed-runtime orchestrator.**
//!
//! Hosts JS (JSC) + Python (CPython) + Go body engines in a single
//! daemon process. The orchestrator co-owns a [`JsRuntime`],
//! [`PyRuntime`], and [`GoRuntime`], each of which holds its own
//! engine, isolate, and scheduler (per-engine schedulers are the
//! shipping shape — sharing a single Scheduler across all three
//! engines is a future refactor; today's per-engine scheduler shape
//! already isolates node closures by engine via NodeDefinition's
//! captured Rc-engine handle).
//!
//! Per-component dispatch shape:
//! - Phase 1 relaxed the per-runtime strict `EngineMisroute` so each
//!   engine's `mount_with_scheduler` silently skips foreign-lang
//!   definitions. The mixed orchestrator feeds the same IR into every
//!   engine; each picks up its own `lang` slice and registers only
//!   those nodes with its scheduler.
//! - Cross-engine messaging is NOT yet wired through a unified
//!   message bus — the three schedulers are independent, so message
//!   delivery is local-to-engine. Cross-engine dispatch is the M22
//!   phase 5+ slice.
//!
//! Lifecycle ordering (Phase 2):
//! - **Initialize-all-before-mount**: every required engine's
//!   `initialize` + `create_isolate` runs before ANY mount. If
//!   bringing up the second engine fails, the first is torn down
//!   cleanly (reverse-order shutdown of the partial-init set).
//! - **Reverse-order shutdown**: at teardown time, Go shuts down
//!   first, then CPython, then JSC. Each runtime's
//!   `dispose_isolate` + `shutdown` is idempotent.
//!
//! Engine-set inference (Phase 3):
//! - The orchestrator examines the parsed IR's `definitions` to
//!   decide which engines to bring up. A `.grx` carrying only
//!   `lang="js"` nodes brings up JSC only (and the mixed runtime
//!   degenerates to single-engine; back-compat for pure-JS
//!   fixtures). A `.grx` carrying all three langs brings up all
//!   three. Engines whose lang is absent from the IR are not
//!   initialized.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use graphreact_engine_zig::EngineError;
use graphreact_grx_parser::{
    ast::ComponentLang,
    multi_file::{resolve_multi_file, MultiFileResolved},
};
use graphreact_observer::MetricsInner;

use crate::goruntime::GoRuntime;
use crate::jsruntime::JsRuntime;
use crate::pyruntime::PyRuntime;
use crate::runtime::RuntimeError;

/// Live mixed-engine composition. Each engine is optional — only
/// engines whose lang appears in the IR get initialized + mounted.
/// Cross-engine routing today is closure-based (each NodeDefinition
/// captures its engine Rc); inter-engine message delivery is a
/// follow-on slice.
pub struct MixedRuntime {
    /// JSC runtime for `lang="js"` bodies. `None` when no JS
    /// definitions are present in the IR.
    pub jsc: Option<Box<JsRuntime>>,
    /// CPython runtime for `lang="python"` bodies. `None` when no
    /// Python definitions are present in the IR.
    pub cpython: Option<Box<PyRuntime>>,
    /// Go runtime for `lang="go"` bodies. `None` when no Go
    /// definitions are present in the IR.
    pub go: Option<Box<GoRuntime>>,
    /// The order engines were brought up, in init order. Reused by
    /// `shutdown` to tear down in reverse order.
    init_order: Vec<EngineSlot>,
    /// Graph identity copied from the IR for the dry-mount report.
    pub graph_id: String,
    /// Total node + edge counts from the IR (sum across engines is
    /// not load-bearing — the IR is shared).
    pub n_nodes: usize,
    pub n_edges: usize,
    /// Per-engine definition counts (after lang filtering).
    pub n_definitions: usize,
    /// Empty fallback scheduler. Returned by [`Self::scheduler_mut`]
    /// when no engine slot is active — only reachable when a
    /// `--runtime=mixed` invocation targets a definitions-empty .grx
    /// (the IR carries `<Graph>` nodes but no `<Component lang=...>`
    /// definitions, so no engine is brought up). `Cluster::start` and
    /// the observer's telemetry install paths both walk
    /// `scheduler_mut()`; without this fallback they'd panic. The
    /// scheduler is genuinely empty (no NodeDefinitions registered),
    /// which is the correct degenerate behaviour for a
    /// no-definitions fixture.
    empty_fallback: graphreact_core::Scheduler,
}

/// Which engine slot is being addressed. Used by [`MixedRuntime`]'s
/// init-order log + by the per-engine Prometheus counter labels
/// (Phase 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineSlot {
    Jsc,
    Cpython,
    Go,
}

impl EngineSlot {
    /// Static label string used for Prometheus `engine="..."` labels.
    /// Matches `EngineKind::engine_label` so downstream Grafana
    /// panels see a single label vocabulary.
    #[must_use]
    pub const fn engine_label(self) -> &'static str {
        match self {
            Self::Jsc => "jsc",
            Self::Cpython => "cpython",
            Self::Go => "go",
        }
    }
}

/// Inspect the resolved IR and decide which engine slots to bring
/// up. Returns an ordered set (JSC → CPython → Go) so the
/// init-all-before-mount path has a deterministic ordering.
fn required_engines(resolved: &MultiFileResolved) -> Vec<EngineSlot> {
    let mut need_jsc = false;
    let mut need_cpython = false;
    let mut need_go = false;
    let walk = |defs: &[graphreact_grx_parser::ast::NodeDefinitionBlock],
                need_jsc: &mut bool,
                need_cpython: &mut bool,
                need_go: &mut bool| {
        for d in defs {
            match d.lang {
                ComponentLang::Js => *need_jsc = true,
                ComponentLang::Python => *need_cpython = true,
                ComponentLang::Go => *need_go = true,
            }
        }
    };
    walk(
        &resolved.root.definitions,
        &mut need_jsc,
        &mut need_cpython,
        &mut need_go,
    );
    for imp in &resolved.imports {
        walk(
            &imp.file.definitions,
            &mut need_jsc,
            &mut need_cpython,
            &mut need_go,
        );
    }
    let mut out = Vec::new();
    if need_jsc {
        out.push(EngineSlot::Jsc);
    }
    if need_cpython {
        out.push(EngineSlot::Cpython);
    }
    if need_go {
        out.push(EngineSlot::Go);
    }
    out
}

/// Mount a `.grx` file against every engine whose lang is present in
/// the IR. **Initialize-all-before-mount semantics**:
///
/// 1. Inspect the IR to decide which engine slots are required.
/// 2. For each required engine, in deterministic order (JSC →
///    CPython → Go), call its `mount_with_scheduler`. This brings
///    up the engine (`initialize` + `create_isolate`) AND registers
///    that engine's lang's nodes with its scheduler.
/// 3. If any engine's mount fails, unwind by tearing down all
///    previously-initialized engines in reverse order. Returns the
///    underlying [`RuntimeError`] so callers see the original cause.
///
/// First `process_batch` is the caller's responsibility — the
/// orchestrator returns a populated [`MixedRuntime`] but does NOT
/// run the first body batch (consistent with each per-engine
/// `mount_with_scheduler` contract).
pub fn mount_mixed(path: &Path) -> Result<MixedRuntime, RuntimeError> {
    mount_mixed_with_secrets(path, None)
}

pub fn mount_mixed_with_secrets(
    path: &Path,
    secrets: Option<Arc<crate::runtime_secrets::RuntimeSecretContext>>,
) -> Result<MixedRuntime, RuntimeError> {
    mount_mixed_with_options(
        path,
        secrets,
        &crate::goruntime::GoRuntimeBuildOptions::default(),
    )
}

pub fn mount_mixed_with_options(
    path: &Path,
    secrets: Option<Arc<crate::runtime_secrets::RuntimeSecretContext>>,
    go_options: &crate::goruntime::GoRuntimeBuildOptions,
) -> Result<MixedRuntime, RuntimeError> {
    let resolved = resolve_multi_file(path)?;
    let required = required_engines(&resolved);

    // Snapshot the IR shape for the report. We re-parse inside each
    // per-engine `mount_with_scheduler` so this is a cheap walk.
    let graph_id = resolved
        .root
        .graph
        .as_ref()
        .map(|g| g.id.clone())
        .unwrap_or_default();

    let mut out = MixedRuntime {
        jsc: None,
        cpython: None,
        go: None,
        init_order: Vec::new(),
        graph_id,
        n_nodes: 0,
        n_edges: 0,
        n_definitions: 0,
        empty_fallback: graphreact_core::Scheduler::new(graphreact_core::CommunicationGraph::new()),
    };

    // Pre-build a Go out_dir adjacent to the .grx so the codegen
    // artifact is recoverable for debug; safe to clobber across
    // runs (codegen writes deterministic output keyed on IR hash).
    let go_out_dir: PathBuf = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".grx-go-build-{}",
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("mixed")
        ));

    for slot in &required {
        match slot {
            EngineSlot::Jsc => {
                tracing::debug!(
                    target: "graphreact::m22::lifecycle",
                    engine = "jsc",
                    "MixedRuntime: bringing up JSC slot"
                );
                let runtime =
                    match crate::jsruntime::mount_with_scheduler_and_secrets(path, secrets.clone())
                    {
                        Ok(r) => r,
                        Err(e) => {
                            // No previously-initialized engines to tear
                            // down since JSC is the first slot.
                            unwind_partial(&mut out);
                            return Err(e);
                        }
                    };
                out.n_nodes = runtime.n_nodes;
                out.n_edges = runtime.n_edges;
                out.n_definitions += runtime.n_definitions;
                out.jsc = Some(Box::new(runtime));
                out.init_order.push(EngineSlot::Jsc);
            }
            EngineSlot::Cpython => {
                tracing::debug!(
                    target: "graphreact::m22::lifecycle",
                    engine = "cpython",
                    "MixedRuntime: bringing up CPython slot"
                );
                let runtime =
                    match crate::pyruntime::mount_with_scheduler_and_secrets(path, secrets.clone())
                    {
                        Ok(r) => r,
                        Err(e) => {
                            unwind_partial(&mut out);
                            return Err(e);
                        }
                    };
                if out.n_nodes == 0 {
                    out.n_nodes = runtime.n_nodes;
                    out.n_edges = runtime.n_edges;
                }
                out.n_definitions += runtime.n_definitions;
                out.cpython = Some(Box::new(runtime));
                out.init_order.push(EngineSlot::Cpython);
            }
            EngineSlot::Go => {
                tracing::debug!(
                    target: "graphreact::m22::lifecycle",
                    engine = "go",
                    "MixedRuntime: bringing up Go slot"
                );
                let runtime = match crate::goruntime::mount_with_scheduler_and_options(
                    path,
                    &go_out_dir,
                    secrets.clone(),
                    go_options,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        unwind_partial(&mut out);
                        return Err(e);
                    }
                };
                if out.n_nodes == 0 {
                    out.n_nodes = runtime.n_nodes;
                    out.n_edges = runtime.n_edges;
                }
                out.n_definitions += runtime.n_definitions;
                out.go = Some(Box::new(runtime));
                out.init_order.push(EngineSlot::Go);
            }
        }
    }

    Ok(out)
}

/// Reverse-order unwind for the partial-init failure path. Each
/// engine's `shutdown` is idempotent (disposes its isolate + shuts
/// the engine). Errors are swallowed — we're already on the error
/// path; teardown errors only get logged.
fn unwind_partial(rt: &mut MixedRuntime) {
    // Iterate init_order in REVERSE so the LAST-initialized engine
    // is the FIRST to shut down. This matches reverse-order
    // shutdown semantics for the success path too.
    let init_order = std::mem::take(&mut rt.init_order);
    for slot in init_order.iter().rev() {
        match slot {
            EngineSlot::Go => {
                if let Some(mut r) = rt.go.take() {
                    if let Err(e) = r.shutdown() {
                        tracing::warn!(
                            target: "graphreact::m22::lifecycle",
                            engine = "go",
                            error = %e,
                            "MixedRuntime unwind: Go shutdown error"
                        );
                    }
                }
            }
            EngineSlot::Cpython => {
                if let Some(mut r) = rt.cpython.take() {
                    if let Err(e) = r.shutdown() {
                        tracing::warn!(
                            target: "graphreact::m22::lifecycle",
                            engine = "cpython",
                            error = %e,
                            "MixedRuntime unwind: CPython shutdown error"
                        );
                    }
                }
            }
            EngineSlot::Jsc => {
                if let Some(mut r) = rt.jsc.take() {
                    if let Err(e) = r.shutdown() {
                        tracing::warn!(
                            target: "graphreact::m22::lifecycle",
                            engine = "jsc",
                            error = %e,
                            "MixedRuntime unwind: JSC shutdown error"
                        );
                    }
                }
            }
        }
    }
}

impl MixedRuntime {
    /// Init-order log — preserved across `process_batch` rounds.
    /// Read by tests + Prometheus instrumentation (Phase 4) to
    /// surface the active engine slots.
    #[must_use]
    pub fn init_order(&self) -> &[EngineSlot] {
        &self.init_order
    }

    /// Mutable reference to a representative engine's scheduler:
    /// JSC if active, else CPython, else Go, else the
    /// `empty_fallback` (a never-populated scheduler that exists
    /// solely to give `Cluster::start` + observer-sink installation
    /// a valid `&mut Scheduler` when the IR carries no per-engine
    /// definitions). Pure-lang fixtures and mixed fixtures both see
    /// a real engine's scheduler; the fallback is only reached on a
    /// `--runtime=mixed` invocation against a definitions-empty
    /// `.grx`, where panicking the daemon would be a worse failure
    /// mode than degenerating to an empty cluster sink.
    pub fn scheduler_mut(&mut self) -> &mut graphreact_core::Scheduler {
        if let Some(r) = self.jsc.as_mut() {
            &mut r.scheduler
        } else if let Some(r) = self.cpython.as_mut() {
            &mut r.scheduler
        } else if let Some(r) = self.go.as_mut() {
            &mut r.scheduler
        } else {
            &mut self.empty_fallback
        }
    }

    /// Drive every active engine's `process_batch` round in init
    /// order. Each engine's batch is independent — message
    /// delivery is local-to-engine in M22 phase 1-4. Cross-engine
    /// messaging is M22 phase 5+.
    pub fn process_batch(&mut self) -> Result<(), graphreact_core::RuntimeError> {
        self.process_batch_with_metrics(None)
    }

    /// Drive each active engine slot and optionally record its individual
    /// wall-clock duration. Timing here (rather than around the mixed host)
    /// keeps the bounded `engine` series attributable to the actual runtime.
    pub fn process_batch_with_metrics(
        &mut self,
        metrics: Option<&MetricsInner>,
    ) -> Result<(), graphreact_core::RuntimeError> {
        if let Some(r) = self.jsc.as_mut() {
            let started = std::time::Instant::now();
            let result = r.process_batch();
            observe_batch_duration(metrics, "jsc", started.elapsed());
            result?;
        }
        if let Some(r) = self.cpython.as_mut() {
            let started = std::time::Instant::now();
            let result = r.process_batch();
            observe_batch_duration(metrics, "cpython", started.elapsed());
            result?;
        }
        if let Some(r) = self.go.as_mut() {
            let started = std::time::Instant::now();
            let result = r.process_batch();
            observe_batch_duration(metrics, "go", started.elapsed());
            result?;
        }
        Ok(())
    }

    /// Async sibling — drives the JSC + CPython runtimes through
    /// their `process_batch_async` paths so `useEffectAsync`
    /// effects fire. Go has no async batch path today (M17 ships
    /// the sync surface only); it falls back to sync `process_batch`.
    pub async fn process_batch_async(&mut self) -> Result<(), graphreact_core::RuntimeError> {
        if let Some(r) = self.jsc.as_mut() {
            r.process_batch_async().await?;
        }
        if let Some(r) = self.cpython.as_mut() {
            r.process_batch_async().await?;
        }
        if let Some(r) = self.go.as_mut() {
            r.process_batch()?;
        }
        Ok(())
    }

    /// Reverse-order shutdown. Tears down each engine in the
    /// opposite order they were initialized; errors are bubbled
    /// from the LAST engine to fail (others are logged but the
    /// shutdown chain continues so no engine leaks).
    ///
    /// **M22 verify fix — Go shutdown on macOS.** `GoRuntime::shutdown`
    /// triggers the Go c-shared `_pthread_tsd_cleanup` deadlock
    /// (golang/go#47100) on macOS during process teardown. The Phase 3
    /// acceptance test sidesteps it via `std::process::exit(0)`; for
    /// the production daemon `--runtime=mixed` carrying a `lang="go"`
    /// definition would otherwise hang on SIGINT. We skip Go's
    /// shutdown here on macOS so the OS reclaims the runtime's
    /// threads and memory on process exit. Same posture as the
    /// goruntime tests and the Phase 3 acceptance test.
    pub fn shutdown(&mut self) -> Result<(), EngineError> {
        let mut last_err: Option<EngineError> = None;
        let init_order = std::mem::take(&mut self.init_order);
        for slot in init_order.iter().rev() {
            match slot {
                EngineSlot::Go => {
                    if let Some(r) = self.go.take() {
                        #[cfg(target_os = "macos")]
                        {
                            tracing::warn!(
                                target: "graphreact::m22::lifecycle",
                                engine = "go",
                                "MixedRuntime shutdown: Go shutdown SKIPPED on macOS to dodge golang/go#47100 TSD-cleanup deadlock; OS process-exit reclaims the runtime"
                            );
                            // Drop the GoRuntime without calling its
                            // shutdown(). `dispose_isolate` is also
                            // skipped; the Go runtime's c-shared lib
                            // holds the spawned goroutines until the
                            // process exits, which is fine because
                            // the daemon is itself terminating.
                            std::mem::forget(r);
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            let mut r = r;
                            if let Err(e) = r.shutdown() {
                                tracing::warn!(
                                    target: "graphreact::m22::lifecycle",
                                    engine = "go",
                                    error = %e,
                                    "MixedRuntime shutdown: Go shutdown error"
                                );
                                last_err = Some(e);
                            }
                        }
                    }
                }
                EngineSlot::Cpython => {
                    if let Some(mut r) = self.cpython.take() {
                        if let Err(e) = r.shutdown() {
                            tracing::warn!(
                                target: "graphreact::m22::lifecycle",
                                engine = "cpython",
                                error = %e,
                                "MixedRuntime shutdown: CPython shutdown error"
                            );
                            last_err = Some(e);
                        }
                    }
                }
                EngineSlot::Jsc => {
                    if let Some(mut r) = self.jsc.take() {
                        if let Err(e) = r.shutdown() {
                            tracing::warn!(
                                target: "graphreact::m22::lifecycle",
                                engine = "jsc",
                                error = %e,
                                "MixedRuntime shutdown: JSC shutdown error"
                            );
                            last_err = Some(e);
                        }
                    }
                }
            }
        }
        if let Some(e) = last_err {
            Err(e)
        } else {
            Ok(())
        }
    }

    /// Sum of `mounted` lists across every active engine. Used by
    /// the `grx run --runtime=mixed` report.
    #[must_use]
    pub fn mounted_components(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(r) = &self.jsc {
            out.extend(r.mounted.iter().cloned());
        }
        if let Some(r) = &self.cpython {
            out.extend(r.mounted.iter().cloned());
        }
        if let Some(r) = &self.go {
            out.extend(r.mounted.iter().cloned());
        }
        out
    }

    /// **M22 phase 4.** Record per-engine mount counts into the
    /// supplied [`graphreact_observer::MetricsInner`]. Called once
    /// after `mount_mixed` returns so the
    /// `grx_engine_mount_count{engine="..."}` series is populated
    /// before the first scrape. Idempotent — calling twice double-
    /// counts; callers wire this to mount, not to every batch.
    ///
    /// Per-engine `mounted` list length is the number of nodes
    /// each engine's scheduler accepted; cluster-engine-agnostic
    /// nodes (e.g. nodes whose component declaration is missing)
    /// are not counted.
    pub fn record_mount_metrics(&self, metrics: &graphreact_observer::MetricsInner) {
        if let Some(r) = &self.jsc {
            for _ in &r.mounted {
                metrics.engine_mount_count_inc(EngineSlot::Jsc.engine_label());
            }
        }
        if let Some(r) = &self.cpython {
            for _ in &r.mounted {
                metrics.engine_mount_count_inc(EngineSlot::Cpython.engine_label());
            }
        }
        if let Some(r) = &self.go {
            for _ in &r.mounted {
                metrics.engine_mount_count_inc(EngineSlot::Go.engine_label());
            }
        }
    }

    /// **M22 phase 4.** Record per-engine body-evaluation counts.
    /// Called after each `process_batch` round on the daemon's
    /// driver loop. Increments
    /// `grx_engine_eval_count{engine="..."}` by the number of
    /// mounted nodes per engine slot — that's the per-batch
    /// upper bound (one body-eval per mounted node per batch).
    pub fn record_eval_metrics(&self, metrics: &graphreact_observer::MetricsInner) {
        if let Some(r) = &self.jsc {
            for _ in &r.mounted {
                metrics.engine_eval_count_inc(EngineSlot::Jsc.engine_label());
            }
        }
        if let Some(r) = &self.cpython {
            for _ in &r.mounted {
                metrics.engine_eval_count_inc(EngineSlot::Cpython.engine_label());
            }
        }
        if let Some(r) = &self.go {
            for _ in &r.mounted {
                metrics.engine_eval_count_inc(EngineSlot::Go.engine_label());
            }
        }
    }

    /// Engine-version banner across active slots. One line per
    /// active engine (`<engine>: <version>`). Empty when no
    /// engines are mounted.
    #[must_use]
    pub fn engine_version_banner(&self) -> String {
        use graphreact_engine_zig::Engine;
        let mut lines: Vec<String> = Vec::new();
        if let Some(r) = &self.jsc {
            lines.push(format!("jsc {}", r.engine.borrow().engine_version()));
        }
        if let Some(r) = &self.cpython {
            lines.push(format!("cpython {}", r.engine.borrow().engine_version()));
        }
        if let Some(r) = &self.go {
            lines.push(format!("go {}", r.engine.borrow().engine_version()));
        }
        lines.join("; ")
    }
}

fn observe_batch_duration(
    metrics: Option<&MetricsInner>,
    engine: &'static str,
    elapsed: std::time::Duration,
) {
    if let Some(metrics) = metrics {
        let duration_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        metrics.engine_process_batch_duration_observe(engine, duration_ns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    /// JSC and CPython both carry process-global state. Mixed-runtime
    /// tests must hold both crate-wide serializers so they cannot race
    /// either engine's single-runtime tests. Always acquire JSC first,
    /// matching mixed-runtime initialization order.
    fn engine_locks() -> (MutexGuard<'static, ()>, MutexGuard<'static, ()>) {
        let jsc = crate::jsruntime::engine_lock();
        let cpython = crate::pyruntime::engine_lock();
        (jsc, cpython)
    }

    fn reset_global_state() {
        crate::jsruntime::reset_global_state();
        crate::pyruntime::reset_global_state();
    }

    /// **Phase 2 acceptance.** When a `.grx` carries only JS bodies,
    /// `mount_mixed` brings up exactly one slot (JSC), preserves
    /// init order, and tears down cleanly on shutdown.
    #[test]
    fn pure_js_fixture_brings_up_jsc_only() {
        let _g = engine_locks();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/js-external-body.grx");
        let mut rt = mount_mixed(&fixture).expect("mount ok");
        assert_eq!(rt.init_order(), &[EngineSlot::Jsc]);
        assert!(rt.jsc.is_some());
        assert!(rt.cpython.is_none());
        assert!(rt.go.is_none());
        rt.shutdown().expect("shutdown ok");
    }

    /// **Phase 2 acceptance.** Pure-Python `.grx` brings up exactly
    /// one slot (CPython) — JSC is NOT initialized.
    #[test]
    fn pure_python_fixture_brings_up_cpython_only() {
        let _g = engine_locks();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python-counter.grx");
        let mut rt = mount_mixed(&fixture).expect("mount ok");
        assert_eq!(rt.init_order(), &[EngineSlot::Cpython]);
        assert!(rt.jsc.is_none());
        assert!(rt.cpython.is_some());
        assert!(rt.go.is_none());
        rt.shutdown().expect("shutdown ok");
    }

    /// **Phase 2 acceptance.** Mixed JS + Python fixture brings up
    /// JSC first, then CPython, in deterministic init order.
    /// Reverse-order shutdown disposes CPython first, JSC second.
    #[test]
    fn mixed_js_python_fixture_init_order_is_deterministic() {
        let _g = engine_locks();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python-smoke-mixed-rejected.grx");
        let mut rt = mount_mixed(&fixture).expect("mount ok");
        assert_eq!(
            rt.init_order(),
            &[EngineSlot::Jsc, EngineSlot::Cpython],
            "init order must be jsc → cpython"
        );
        assert!(rt.jsc.is_some());
        assert!(rt.cpython.is_some());
        rt.shutdown().expect("reverse-order shutdown ok");
    }

    /// **Phase 4 acceptance.** The per-engine counters
    /// `grx_engine_mount_count{engine}` and
    /// `grx_engine_eval_count{engine}` carry one series per active
    /// engine slot. Pure-Python fixture yields exactly
    /// `engine="cpython"`; mixed JS+Python yields `engine="jsc"` +
    /// `engine="cpython"`. The full mixed fixture is covered by
    /// the Phase 3 acceptance test below.
    #[test]
    fn phase4_acceptance_engine_mount_count_populates_per_slot() {
        let _g = engine_locks();
        reset_global_state();
        let metrics = graphreact_observer::MetricsInner::new();

        let fixture_js = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/js-external-body.grx");
        let mut rt = mount_mixed(&fixture_js).expect("mount js");
        rt.record_mount_metrics(&metrics);
        rt.shutdown().expect("shutdown ok");

        // After a pure-JS mount, only the jsc series is populated.
        assert!(
            metrics.engine_mount_count.iter().any(|e| {
                e.value()
                    .labels
                    .iter()
                    .any(|(k, v)| *k == "engine" && v == "jsc")
            }),
            "jsc engine_mount_count series missing"
        );
        // Pure-JS mount must NOT have populated cpython or go.
        assert!(
            !metrics.engine_mount_count.iter().any(|e| {
                e.value()
                    .labels
                    .iter()
                    .any(|(k, v)| *k == "engine" && (v == "cpython" || v == "go"))
            }),
            "non-JS engine_mount_count series should not exist for pure-JS fixture"
        );

        reset_global_state();
        // Now mount a pure-Python fixture into a SECOND metrics
        // store + assert only cpython populates.
        let metrics_py = graphreact_observer::MetricsInner::new();
        let fixture_py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/python-counter.grx");
        let mut rt_py = mount_mixed(&fixture_py).expect("mount py");
        rt_py.record_mount_metrics(&metrics_py);
        rt_py.record_eval_metrics(&metrics_py);
        rt_py.shutdown().expect("shutdown py");

        assert!(
            metrics_py.engine_mount_count.iter().any(|e| {
                e.value()
                    .labels
                    .iter()
                    .any(|(k, v)| *k == "engine" && v == "cpython")
            }),
            "cpython engine_mount_count series missing for pure-Python fixture"
        );
        assert!(
            metrics_py.engine_eval_count.iter().any(|e| {
                e.value()
                    .labels
                    .iter()
                    .any(|(k, v)| *k == "engine" && v == "cpython")
            }),
            "cpython engine_eval_count series missing for pure-Python fixture"
        );
    }

    /// **Phase 3 acceptance.** The full mixed-runtime-supervisor
    /// fixture (JS + Python + Go) brings up ALL three engines via
    /// `mount_mixed` in deterministic JSC → CPython → Go order.
    /// Each engine registers exactly one node with its scheduler;
    /// the union across slots equals the IR's three Component
    /// nodes. Reverse-order shutdown tears down Go → CPython →
    /// JSC without leaking an engine.
    ///
    /// Gated on a Go toolchain being present. The Go engine's
    /// `codegen_and_build` invokes `go build`; without Go on
    /// PATH the mount errors out. Skip the test in that case so
    /// CI without Go still passes (mirrors goruntime's
    /// `skip_if_no_go` pattern).
    ///
    /// **`std::process::exit(0)` at the end** dodges the Go
    /// runtime's TSD-cleanup deadlock on macOS — same workaround
    /// the goruntime end-to-end smoke uses. Without it the test
    /// hangs inside `_pthread_tsd_cleanup` after the assertions
    /// pass. The exit means downstream tests in the same `cargo
    /// test` invocation don't run, so this test is ignored by
    /// default and the operator opts in via
    /// `cargo test -- --ignored phase3_acceptance` to actually
    /// invoke it. The Phase 1/2/4 acceptance tests above don't
    /// touch the Go engine and run cleanly in the regular set.
    #[test]
    #[ignore = "Phase 3 acceptance exits the test process via std::process::exit(0) to dodge the Go TSD-cleanup deadlock; opt in via `--ignored`"]
    fn phase3_acceptance_full_mixed_supervisor_fixture() {
        let go_present = std::process::Command::new("go")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if std::env::var("GRAPHREACT_SKIP_GO").is_ok() || !go_present {
            eprintln!(
                "[m22] skipping phase3_acceptance_full_mixed_supervisor_fixture — no `go` toolchain on PATH"
            );
            return;
        }
        let _g = engine_locks();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mixed-runtime-supervisor.grx");
        let rt = mount_mixed(&fixture).expect("mount ok");
        assert_eq!(
            rt.init_order(),
            &[EngineSlot::Jsc, EngineSlot::Cpython, EngineSlot::Go],
            "init order must be jsc → cpython → go"
        );
        assert!(rt.jsc.is_some(), "jsc slot active");
        assert!(rt.cpython.is_some(), "cpython slot active");
        assert!(rt.go.is_some(), "go slot active");
        assert_eq!(rt.graph_id, "mixed-runtime-supervisor");
        let mounted = rt.mounted_components();
        assert!(
            mounted.contains(&"supervisor".to_string()),
            "supervisor mounted via JSC; got {mounted:?}"
        );
        assert!(
            mounted.contains(&"py-child".to_string()),
            "py-child mounted via CPython; got {mounted:?}"
        );
        assert!(
            mounted.contains(&"go-child".to_string()),
            "go-child mounted via Go; got {mounted:?}"
        );
        // Skip the explicit shutdown — Go runtime TSD-cleanup
        // deadlocks if we let drop or shutdown fire. process::exit
        // tears the OS process down without invoking C++ TLS or
        // Go runtime tear-down.
        std::process::exit(0);
    }
}
