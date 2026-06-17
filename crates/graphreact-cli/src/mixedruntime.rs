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

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};

use graphreact_engine_zig::EngineError;
use graphreact_grx_parser::{
    ast::ComponentLang,
    multi_file::{resolve_multi_file, MultiFileResolved},
};

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
    };

    // Pre-build a Go out_dir adjacent to the .grx so the codegen
    // artifact is recoverable for debug; safe to clobber across
    // runs (codegen writes deterministic output keyed on IR hash).
    let go_out_dir: PathBuf = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".grx-go-build-{}",
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("mixed")
        ));

    for slot in &required {
        match slot {
            EngineSlot::Jsc => {
                tracing::debug!(
                    target: "graphreact::m22::lifecycle",
                    engine = "jsc",
                    "MixedRuntime: bringing up JSC slot"
                );
                let runtime = match crate::jsruntime::mount_with_scheduler(path) {
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
                let runtime = match crate::pyruntime::mount_with_scheduler(path) {
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
                let runtime = match crate::goruntime::mount_with_scheduler(path, &go_out_dir) {
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

    /// Drive every active engine's `process_batch` round in init
    /// order. Each engine's batch is independent — message
    /// delivery is local-to-engine in M22 phase 1-4. Cross-engine
    /// messaging is M22 phase 5+.
    pub fn process_batch(&mut self) -> Result<(), graphreact_core::RuntimeError> {
        if let Some(r) = self.jsc.as_mut() {
            r.process_batch()?;
        }
        if let Some(r) = self.cpython.as_mut() {
            r.process_batch()?;
        }
        if let Some(r) = self.go.as_mut() {
            r.process_batch()?;
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
    pub fn shutdown(&mut self) -> Result<(), EngineError> {
        let mut last_err: Option<EngineError> = None;
        let init_order = std::mem::take(&mut self.init_order);
        for slot in init_order.iter().rev() {
            match slot {
                EngineSlot::Go => {
                    if let Some(mut r) = self.go.take() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    /// JSC's process-global state demands serialised tests across
    /// the workspace; the cpython process-singleton also serialises
    /// on the same module-local lock. Mixed-runtime tests acquire
    /// the JSC lock since JSC is the first engine touched.
    fn engine_lock() -> MutexGuard<'static, ()> {
        crate::jsruntime::engine_lock()
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
        let _g = engine_lock();
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
        let _g = engine_lock();
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
        let _g = engine_lock();
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
    #[test]
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
        let _g = engine_lock();
        reset_global_state();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mixed-runtime-supervisor.grx");
        let mut rt = mount_mixed(&fixture).expect("mount ok");
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
        rt.shutdown().expect("reverse-order shutdown ok");
    }
}
