//! Composition layer for **M17 Go bodies** — phases 3a/3b-real/3c.
//!
//! Mirrors the role of [`crate::pyruntime`] (M13 CPython) and
//! [`crate::jsruntime`] (M12.3 JSC) for `lang="go"` definitions:
//! parses a `.grx` file, drives [`graphreact_codegen_go`] to emit the
//! prelude + body files + dispatch.go + invoke `go build
//! -buildmode=c-shared`, then constructs a
//! [`graphreact_engine_go::GoEngine`] pointed at the produced artifact
//! and (in [`mount_with_scheduler`]) registers each `<C lang="go">`
//! definition with a [`Scheduler`] that drives the body on every
//! render.
//!
//! # Phase status
//!
//! - **Phase 3a / 3b-real ([`build_and_load`]):** SHIPPED. Codegen +
//!   build + load + initialize + create_isolate, no scheduler. Used
//!   by the bare-runtime smoke that proves real body dispatch end-to-
//!   end ([`tests::scenario_build_and_load_body_dispatch`]).
//! - **Phase 3c ([`mount_with_scheduler`]):** SHIPPED. Full
//!   scheduler-integrated mount — registers host_emit + host_message
//!   callbacks, evaluates each Go body's module, resolves a `body`
//!   handle, registers a [`NodeDefinition`] with the scheduler. The
//!   resulting [`GoRuntime::process_batch`] drives the cycle:
//!   scheduler runs each dirty node's body → body's [`UseEmit`]
//!   pushes a message into the scheduler's queue → scheduler
//!   delivers to recipients → recipient's [`UseMessage`] handler
//!   fires via [`GoEngine::invoke_handler`]. Mirrors
//!   [`crate::pyruntime::mount_with_scheduler`] at deliberately
//!   reduced scope — `use_state` / `use_effect` / etc. land in M17
//!   phase 3b-real-state / 3d-h.
//!
//! Per [ADR-0005](../../../docs/decisions/0005-third-runtime-is-go-not-jvm.md)
//! and the [later-milestone strategic plan](../../../../../.claude/plans/graphreact-rust-later-milestones.md)
//! §M17.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustc_hash::FxHashMap;
use serde_json::Value;

use graphreact_codegen_go::{BuildArtifact, Codegen};
use graphreact_core::{
    use_emit as core_use_emit, use_message as core_use_message, Attrs, CommunicationGraph,
    EmitOptions, IncomingMessage, NodeContext, NodeDefinition, PermissionManifest, Scheduler,
};
use graphreact_engine_go::GoEngine;
use graphreact_engine_zig::{
    BoxCallback, Engine, EngineError, FunctionHandle, HostValue, IsolateHandle, IsolateOptions,
};
use graphreact_grx_parser::{
    ast::ComponentLang, ir::IrNode, multi_file::resolve_multi_file,
};

use crate::runtime::RuntimeError;

// ---------------------------------------------------------------------
// Per-thread registries (mirroring pyruntime.rs phase 3b TLS)
// ---------------------------------------------------------------------

thread_local! {
    /// The engine currently driving `process_batch`. Installed by
    /// [`GoEngineGuard::new`] around the batch; read by
    /// [`build_host_message_callback`] when it registers per-message
    /// dispatch closures so the closures can re-enter
    /// [`GoEngine::invoke_handler`] on every delivery.
    static CURRENT_ENGINE: RefCell<Option<Rc<RefCell<GoEngine>>>> =
        const { RefCell::new(None) };

    /// The isolate the current batch is running against. Read along
    /// with [`CURRENT_ENGINE`] by the host_message callback.
    static CURRENT_ISO: Cell<Option<IsolateHandle>> = const { Cell::new(None) };
}

fn current_engine() -> Option<Rc<RefCell<GoEngine>>> {
    CURRENT_ENGINE.with(|c| c.borrow().clone())
}

fn current_iso() -> Option<IsolateHandle> {
    CURRENT_ISO.with(Cell::get)
}

/// RAII guard that installs `(engine, iso)` into the TLS slots for
/// the duration of a `process_batch` call. Mirrors
/// `pyruntime::PyEngineGuard`. Drop clears the slots so a later
/// host callback fired outside a batch surfaces a clean "engine TLS
/// unset" error rather than dispatching against a stale handle.
struct GoEngineGuard {
    _phantom: (),
}

impl GoEngineGuard {
    fn new(engine: Rc<RefCell<GoEngine>>, iso: IsolateHandle) -> Self {
        CURRENT_ENGINE.with(|c| *c.borrow_mut() = Some(engine));
        CURRENT_ISO.with(|c| c.set(Some(iso)));
        Self { _phantom: () }
    }
}

impl Drop for GoEngineGuard {
    fn drop(&mut self) {
        CURRENT_ENGINE.with(|c| *c.borrow_mut() = None);
        CURRENT_ISO.with(|c| c.set(None));
    }
}

// ---------------------------------------------------------------------
// host_* callbacks (M17 phase 3c port from pyruntime phase 3c)
// ---------------------------------------------------------------------

/// Build the `host_emit(type, payload, opts)` callback. Go-side
/// [`UseEmit`] returns a closure that calls this; the closure runs on
/// every emit.
///
/// Engine-agnostic body — identical shape to
/// [`crate::pyruntime::build_host_emit_callback`]. No TLS read needed:
/// `core_use_emit()` reads `HOOK_CTX` internally to resolve the per-
/// node emit closure.
///
/// [`UseEmit`]: graphreact_codegen_go
fn build_host_emit_callback() -> BoxCallback {
    Box::new(|args| {
        let r#type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_emit: expected String type, got {other:?}"
                )))
            }
        };
        let payload: Value = args
            .get(1)
            .cloned()
            .map(HostValue::into_json)
            .unwrap_or(Value::Null);
        let opts = parse_emit_options(args.get(2));
        let emit_fn = core_use_emit();
        emit_fn(&r#type, payload, opts);
        Ok(HostValue::Null)
    })
}

/// Build the `host_message(type, token)` callback. Go-side
/// [`UseMessage`] registers its handler in the prelude's
/// `handlerRegistry` under a fresh token and forwards `(msgType,
/// token)` here.
///
/// On every delivery, the closure registered via `core_use_message`
/// marshals the [`IncomingMessage`] into a
/// `{from, type, payload, ttl}` envelope and calls
/// [`GoEngine::invoke_handler`] (the symmetric Rust → Go
/// invocation path, paralleling pyruntime's
/// `invoke_global("__invoke_callback", ...)`).
///
/// TLS read once at registration time (set by [`GoEngineGuard`] around
/// `process_batch`); captured by the per-delivery closure so deferred-
/// fire handlers can re-enter Go without holding the host_message
/// callback's args borrow.
///
/// [`UseMessage`]: graphreact_codegen_go
fn build_host_message_callback() -> BoxCallback {
    Box::new(|args| {
        let r#type: String = match args.first() {
            Some(HostValue::String(s)) => s.clone(),
            other => {
                return Err(EngineError::Internal(format!(
                    "host_message: expected String type, got {other:?}"
                )))
            }
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let token = match args.get(1) {
            Some(HostValue::Number(n)) => *n as u64,
            other => {
                return Err(EngineError::Internal(format!(
                    "host_message: expected Number token, got {other:?}"
                )))
            }
        };

        let engine = current_engine()
            .ok_or_else(|| EngineError::Internal("host_message: engine TLS unset".into()))?;
        // Validate the iso TLS is installed even though the
        // process-global handler registry doesn't key by isolate
        // today; future phases (per-isolate handler scoping) will
        // consume it. Reading here catches "callback fired outside a
        // process_batch" misuse early.
        let _iso = current_iso()
            .ok_or_else(|| EngineError::Internal("host_message: iso TLS unset".into()))?;

        core_use_message(r#type, move |msg: &IncomingMessage| {
            let host_msg = HostValue::Object(vec![
                ("from".into(), HostValue::String(msg.from.clone())),
                ("type".into(), HostValue::String(msg.r#type.clone())),
                ("payload".into(), HostValue::from_json(msg.payload.clone())),
                ("ttl".into(), HostValue::Number(f64::from(msg.ttl))),
            ]);
            let result = engine.borrow().invoke_handler(token, &host_msg);
            if let Err(e) = result {
                eprintln!("[graphreact-cli] use_message handler invocation failed: {e}");
            }
        });
        Ok(HostValue::Null)
    })
}

/// Parse the third argument of `host_emit` into [`EmitOptions`].
/// Recognised keys mirror JSC + CPython: `to` (Array of String),
/// `ttl` (Number), `edgeKey` (String), `direct` (String). Unknown
/// keys are ignored. `null` / missing → defaults.
fn parse_emit_options(arg: Option<&HostValue>) -> EmitOptions {
    let Some(HostValue::Object(pairs)) = arg else {
        return EmitOptions::default();
    };
    let mut opts = EmitOptions::default();
    for (k, v) in pairs {
        match (k.as_str(), v) {
            ("to", HostValue::Array(arr)) => {
                opts.to = Some(
                    arr.iter()
                        .filter_map(|x| {
                            if let HostValue::String(s) = x {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .collect(),
                );
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            ("ttl", HostValue::Number(n)) => opts.ttl = Some(*n as u32),
            ("edgeKey", HostValue::String(s)) => opts.edge_key = Some(s.clone()),
            ("direct", HostValue::String(s)) => opts.direct = Some(s.clone()),
            _ => {}
        }
    }
    opts
}

// ---------------------------------------------------------------------
// GoRuntime — Scheduler + GoEngine + IR mount (phase 3c)
// ---------------------------------------------------------------------

/// Live Go-runtime composition. Holds the [`GoEngine`] wrapped in
/// [`Rc<RefCell<...>>`] so host callbacks can re-enter the engine
/// across `process_batch` boundaries, a single isolate, the
/// codegen-produced [`BuildArtifact`] for debug + hot-reload short-
/// circuiting, and the scheduler (populated only by
/// [`mount_with_scheduler`]; [`build_and_load`] returns a bare
/// runtime without scheduler-registered nodes).
pub struct GoRuntime {
    pub engine: Rc<RefCell<GoEngine>>,
    pub iso: IsolateHandle,
    /// Path to the c-shared artifact produced by
    /// [`Codegen::build_artifact`]. Held for debug + M17 phase 3g
    /// hot-reload short-circuiting.
    pub artifact_path: PathBuf,
    /// SHA-256 of the artifact bytes (lowercase hex). Phase 3g uses
    /// this to skip rebuilds when the input IR + toolchain produce
    /// byte-identical output.
    pub artifact_sha256_hex: String,
    /// Number of `lang="go"` definitions the codegen-emitted package
    /// contained.
    pub n_definitions: usize,
    /// Graph id from the parsed IR.
    pub graph_id: String,
    /// Node count from the parsed IR. Populated by
    /// [`mount_with_scheduler`]; zero on the [`build_and_load`] path.
    pub n_nodes: usize,
    /// Edge count from the parsed IR. Populated by
    /// [`mount_with_scheduler`]; zero on the [`build_and_load`] path.
    pub n_edges: usize,
    /// Scheduler holding the registered Go node definitions. The
    /// [`build_and_load`] entry point returns a runtime with an
    /// empty scheduler; [`mount_with_scheduler`] populates it.
    pub scheduler: Scheduler,
    /// Per-component `body(ctx)` handle. Reused across renders.
    pub component_handles: FxHashMap<String, FunctionHandle>,
    /// Node ids that were successfully registered with the scheduler.
    pub mounted: Vec<String>,
}

impl std::fmt::Debug for GoRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoRuntime")
            .field("graph_id", &self.graph_id)
            .field("n_nodes", &self.n_nodes)
            .field("n_edges", &self.n_edges)
            .field("n_definitions", &self.n_definitions)
            .field("mounted_count", &self.mounted.len())
            .field("component_handle_count", &self.component_handles.len())
            .finish_non_exhaustive()
    }
}

impl GoRuntime {
    /// Drive the scheduler one `process_batch` round. Installs the
    /// engine + iso TLS so deferred host callbacks (message handlers)
    /// can re-enter Go via [`GoEngine::invoke_handler`].
    pub fn process_batch(&mut self) -> Result<(), graphreact_core::RuntimeError> {
        let _engine_guard = GoEngineGuard::new(Rc::clone(&self.engine), self.iso);
        self.scheduler.process_batch()
    }

    /// Tear down — disposes the isolate and shuts the engine. Drains
    /// per-isolate callback ids on the Rust side; the Go runtime
    /// stays alive (single-runtime-per-process; ADR-0005).
    pub fn shutdown(&mut self) -> Result<(), EngineError> {
        let mut e = self.engine.borrow_mut();
        e.dispose_isolate(self.iso)?;
        e.shutdown()?;
        Ok(())
    }
}

/// **M17 phase 3a / 3b-real.** Build the artifact at `out_dir` and
/// dispose the engine on drop. `out_dir` is also where intermediate
/// `grx_prelude.go` / `body_*.go` / `dispatch.go` / `go.mod` land.
///
/// Returns a bare [`GoRuntime`] with the scheduler empty — useful for
/// callers that only need the engine + isolate (e.g. body-dispatch
/// smoke). New code should prefer [`mount_with_scheduler`].
pub fn build_and_load(grx_path: &Path, out_dir: &Path) -> Result<GoRuntime, RuntimeError> {
    let ir = parse_grx_to_ir(grx_path)?;
    let artifact = codegen_and_build(&ir, out_dir)?;

    let mut engine = GoEngine::with_artifact(artifact.library_path.clone());
    engine
        .initialize()
        .map_err(RuntimeError::Engine)?;
    let iso = engine
        .create_isolate(IsolateOptions::default())
        .map_err(RuntimeError::Engine)?;

    let n_definitions = ir
        .definitions
        .iter()
        .filter(|d| d.lang == ComponentLang::Go)
        .count();

    Ok(GoRuntime {
        engine: Rc::new(RefCell::new(engine)),
        iso,
        artifact_path: artifact.library_path,
        artifact_sha256_hex: artifact.library_sha256_hex,
        n_definitions,
        graph_id: ir.graph_id,
        n_nodes: 0,
        n_edges: 0,
        scheduler: Scheduler::new(CommunicationGraph::new()),
        component_handles: FxHashMap::default(),
        mounted: Vec::new(),
    })
}

/// **M17 phase 3c.** Full scheduler-integrated mount. Parses the
/// `.grx`, runs codegen + build + load, brings up the Go runtime,
/// registers `host_emit` + `host_message` host callbacks, evaluates
/// each `lang="go"` definition's body module, resolves a `body`
/// handle, registers a [`NodeDefinition`] with a fresh [`Scheduler`].
///
/// Returns a fully-populated [`GoRuntime`]; the caller drives it via
/// [`GoRuntime::process_batch`] to fire each round of emit/message
/// delivery.
///
/// Single-engine-per-system enforcement: a `lang="js"` /
/// `lang="python"` definition fed here surfaces as
/// [`RuntimeError::EngineMisroute`] with a clear diagnostic.
pub fn mount_with_scheduler(
    grx_path: &Path,
    out_dir: &Path,
) -> Result<GoRuntime, RuntimeError> {
    let ir = parse_grx_to_ir(grx_path)?;

    // Single-engine-per-system check BEFORE codegen + build, so authors
    // get a clean error instead of a misleading "go build failed"
    // when they accidentally route a Python body through --runtime=go.
    for def in &ir.definitions {
        if def.lang != ComponentLang::Go {
            return Err(RuntimeError::EngineMisroute {
                component: def.component.clone(),
                lang: def.lang.as_attr_str().to_string(),
                engine: "go".to_string(),
            });
        }
    }

    let artifact = codegen_and_build(&ir, out_dir)?;

    let engine_rc: Rc<RefCell<GoEngine>> = Rc::new(RefCell::new(GoEngine::with_artifact(
        artifact.library_path.clone(),
    )));
    let iso = {
        let mut e = engine_rc.borrow_mut();
        e.initialize().map_err(RuntimeError::Engine)?;
        let iso = e
            .create_isolate(IsolateOptions::default())
            .map_err(RuntimeError::Engine)?;
        // Register the host_emit / host_message callbacks. The Go side
        // intercepts these names in GrxCallbackRegister and stores
        // the slots so UseEmit / UseMessage helpers can dispatch
        // directly without per-call name lookup.
        e.register_callback(iso, "host_emit", build_host_emit_callback())
            .map_err(RuntimeError::Engine)?;
        e.register_callback(iso, "host_message", build_host_message_callback())
            .map_err(RuntimeError::Engine)?;
        iso
    };

    // Resolve each Go definition's body handle. Component name → handle.
    let mut component_handles: FxHashMap<String, FunctionHandle> = FxHashMap::default();
    for def in &ir.definitions {
        let module = engine_rc
            .borrow_mut()
            .eval_module(iso, &def.component, &def.code)
            .map_err(RuntimeError::Engine)?;
        let handle = engine_rc
            .borrow_mut()
            .resolve_function(iso, module, "body")
            .map_err(RuntimeError::Engine)?;
        component_handles.insert(def.component.clone(), handle);
    }

    // Build the CommunicationGraph from the IR's nodes + edges.
    let mut graph = CommunicationGraph::new();
    for ir_node in &ir.nodes {
        let mut attrs = Attrs::new();
        for (k, v) in &ir_node.attrs {
            attrs.insert(k.clone(), v.clone());
        }
        graph.add_node(ir_node.id.clone(), attrs);
    }
    for ir_edge in &ir.edges {
        let mut attrs = Attrs::new();
        for (k, v) in &ir_edge.attrs {
            attrs.insert(k.clone(), v.clone());
        }
        graph.add_edge(&ir_edge.source, &ir_edge.target, None, attrs);
    }

    let mut scheduler = Scheduler::new(graph);

    // Register each IR node with the scheduler, wrapping its body in a
    // NodeDefinition that re-enters the Go engine on every render.
    let mut mounted: Vec<String> = Vec::new();
    for node in &ir.nodes {
        let component_name = component_name_for_node(node);
        let Some(&fn_handle) = component_handles.get(&component_name) else {
            // Component not in our definition set — skip; phase 3+
            // may surface a diagnostic.
            continue;
        };
        let definition = build_go_node_definition(
            component_name.clone(),
            Rc::clone(&engine_rc),
            iso,
            fn_handle,
        );
        let mut props: Attrs = Attrs::new();
        for (k, v) in &node.attrs {
            props.insert(k.clone(), v.clone());
        }
        #[allow(clippy::cast_possible_truncation)]
        let height: u32 = node.depth as u32;
        scheduler
            .register_node(definition, node.id.clone(), props, height)
            .map_err(|e| RuntimeError::Engine(EngineError::Internal(e.to_string())))?;
        mounted.push(node.id.clone());
    }

    let n_nodes = ir.nodes.len();
    let n_edges = ir.edges.len();
    let n_definitions = ir.definitions.len();

    Ok(GoRuntime {
        engine: engine_rc,
        iso,
        artifact_path: artifact.library_path,
        artifact_sha256_hex: artifact.library_sha256_hex,
        n_definitions,
        graph_id: ir.graph_id,
        n_nodes,
        n_edges,
        scheduler,
        component_handles,
        mounted,
    })
}

/// Build a [`NodeDefinition`] that calls the Go body callable on every
/// render. The closure captures `(engine, iso, fn_handle)` so each
/// render re-enters [`GoEngine::call_function`] with the per-instance
/// ctx; the returned JSON manifest is parsed into a
/// [`PermissionManifest`] by [`host_value_to_manifest`].
fn build_go_node_definition(
    name: String,
    engine: Rc<RefCell<GoEngine>>,
    iso: IsolateHandle,
    fn_handle: FunctionHandle,
) -> NodeDefinition {
    NodeDefinition::new(name, move |ctx: &NodeContext| -> PermissionManifest {
        let host_ctx = build_host_ctx(ctx);
        let result = engine
            .borrow_mut()
            .call_function(iso, fn_handle, &[host_ctx]);
        match result {
            Ok(manifest) => host_value_to_manifest(manifest),
            Err(e) => {
                eprintln!("[graphreact-cli] Go component invocation failed: {e}");
                PermissionManifest::default()
            }
        }
    })
}

/// Build the per-instance Go ctx from a [`NodeContext`]. Shape is
/// `{id, props, neighbors}` — same as pyruntime's `build_host_ctx`.
fn build_host_ctx(ctx: &NodeContext) -> HostValue {
    let mut props_pairs: Vec<(String, HostValue)> = Vec::new();
    for (k, v) in &ctx.props {
        if matches!(k.as_str(), "component" | "_depth" | "_parentPath") {
            continue;
        }
        props_pairs.push((k.clone(), HostValue::from_json(v.clone())));
    }
    let neighbors: Vec<HostValue> = ctx
        .neighbors
        .iter()
        .cloned()
        .map(HostValue::String)
        .collect();
    HostValue::Object(vec![
        ("id".into(), HostValue::String(ctx.id.clone())),
        ("props".into(), HostValue::Object(props_pairs)),
        ("neighbors".into(), HostValue::Array(neighbors)),
    ])
}

/// Convert a Go-returned manifest object into a graphreact-core
/// [`PermissionManifest`]. Mirrors pyruntime::host_value_to_manifest —
/// only `accepts` and `emits` (plus M-Ingress `accepts_external` /
/// `accepts_external_call`) are populated; per-channel policies +
/// per-neighbor overrides land in phase 3d+.
fn host_value_to_manifest(hv: HostValue) -> PermissionManifest {
    let HostValue::Object(pairs) = hv else {
        return PermissionManifest::default();
    };
    let mut accepts: Vec<String> = Vec::new();
    let mut emits: Vec<String> = Vec::new();
    let mut accepts_external: Vec<String> = Vec::new();
    let mut accepts_external_call: Vec<graphreact_core::ExternalCall> = Vec::new();
    for (k, v) in pairs {
        match (k.as_str(), v) {
            ("accepts", HostValue::Array(arr)) => {
                accepts = arr
                    .into_iter()
                    .filter_map(|x| {
                        if let HostValue::String(s) = x {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .collect();
            }
            ("emits", HostValue::Array(arr)) => {
                emits = arr
                    .into_iter()
                    .filter_map(|x| {
                        if let HostValue::String(s) = x {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .collect();
            }
            ("accepts_external", HostValue::Array(arr)) => {
                accepts_external = arr
                    .into_iter()
                    .filter_map(|x| {
                        if let HostValue::String(s) = x {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .collect();
            }
            ("accepts_external_call", HostValue::Array(arr)) => {
                accepts_external_call = arr
                    .into_iter()
                    .filter_map(|entry| {
                        let HostValue::Object(pairs) = entry else {
                            return None;
                        };
                        let mut request_type: Option<String> = None;
                        let mut reply_type: Option<String> = None;
                        for (k, v) in pairs {
                            match (k.as_str(), v) {
                                ("request_type", HostValue::String(s)) => request_type = Some(s),
                                ("reply_type", HostValue::String(s)) => reply_type = Some(s),
                                _ => {}
                            }
                        }
                        match (request_type, reply_type) {
                            (Some(rq), Some(rp)) => Some(graphreact_core::ExternalCall {
                                request_type: rq,
                                reply_type: rp,
                            }),
                            _ => None,
                        }
                    })
                    .collect();
            }
            _ => {}
        }
    }
    let mut manifest = PermissionManifest::new(accepts, emits);
    manifest.accepts_external = accepts_external;
    manifest.accepts_external_call = accepts_external_call;
    manifest
}

fn component_name_for_node(node: &IrNode) -> String {
    node.attrs
        .get("component")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| node.component.clone())
}

/// Run codegen + go build, returning the artifact.
fn codegen_and_build(
    ir: &graphreact_grx_parser::ir::GrxIr,
    out_dir: &Path,
) -> Result<BuildArtifact, RuntimeError> {
    let codegen = Codegen::new();
    codegen.emit_prelude(ir, out_dir).map_err(|e| {
        RuntimeError::Engine(EngineError::Internal(format!(
            "graphreact-codegen-go::emit_prelude: {e}"
        )))
    })?;
    codegen.build_artifact(out_dir, out_dir).map_err(|e| {
        RuntimeError::Engine(EngineError::Internal(format!(
            "graphreact-codegen-go::build_artifact: {e}"
        )))
    })
}

fn parse_grx_to_ir(path: &Path) -> Result<graphreact_grx_parser::ir::GrxIr, RuntimeError> {
    // Mirror pyruntime::mount_with_scheduler: drive the multi-file
    // resolver so `<X src="./body.go" lang="go"/>` external bodies
    // (M11.5 / Phase 5-D substrate) actually populate the IR's `code`
    // field.
    let resolved = resolve_multi_file(path).map_err(|e| {
        RuntimeError::Engine(EngineError::Internal(format!(
            "graphreact-cli::goruntime: multi-file resolver error in {}: {e}",
            path.display()
        )))
    })?;
    let subgraphs = resolved.collect_root_subgraphs().map_err(|e| {
        RuntimeError::Engine(EngineError::Internal(format!(
            "graphreact-cli::goruntime: subgraph rollup error in {}: {e}",
            path.display()
        )))
    })?;
    graphreact_grx_parser::ir::build_ir(&graphreact_grx_parser::GrxFile {
        imports: resolved.root.imports.clone(),
        graph: resolved.root.graph.clone(),
        subgraphs,
        definitions: resolved.root.definitions.clone(),
    })
    .map_err(|e| {
        RuntimeError::Engine(EngineError::Internal(format!(
            "graphreact-cli::goruntime: build_ir error in {}: {e}",
            path.display()
        )))
    })
}

#[cfg(test)]
mod tests {
    //! Phase 3c end-to-end tests. The Go runtime cannot survive a
    //! clean thread exit on macOS (TSD-cleanup deadlock; see
    //! [`crate::goruntime`] module-level doc + the engine-go
    //! `tests/conformance_smoke.rs` integration test). All goruntime
    //! end-to-end scenarios therefore run in a single test function
    //! that exits the process at the end. Scenarios are sequential —
    //! `cargo test` runs each #[test] in its own thread by default,
    //! but a single test on its own is one runner thread, which the
    //! exit(0) dodges cleanly.
    //!
    //! `cargo test ... goruntime::tests::end_to_end` is the canonical
    //! invocation. Adding more `#[test]` functions here without the
    //! single-test pattern WILL deadlock — keep additions inside
    //! `end_to_end` as new scenario fns.
    use super::*;
    use std::path::Path;

    fn skip_if_no_go() -> bool {
        let probe = std::process::Command::new("go").arg("version").output();
        if matches!(&probe, Ok(out) if out.status.success()) {
            false
        } else {
            eprintln!("skipping goruntime smoke: `go` not on PATH");
            true
        }
    }

    fn fresh_out_dir(tag: &str) -> PathBuf {
        let out_dir = std::env::temp_dir().join(format!(
            "grx-goruntime-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&out_dir);
        out_dir
    }

    /// Phase 3a / 3b-real bare-runtime build_and_load + body
    /// dispatch. Invokes the Counter body and verifies a real
    /// Manifest comes back from the dispatched Go function.
    fn scenario_build_and_load_body_dispatch() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go-smoke.grx");
        assert!(fixture.exists(), "fixture missing at {}", fixture.display());

        let out_dir = fresh_out_dir("smoke");
        let runtime = build_and_load(&fixture, &out_dir)
            .expect("build_and_load should succeed for go-smoke.grx");

        assert!(!runtime.graph_id.is_empty(), "graph_id empty");
        assert!(runtime.n_definitions >= 1, "n_definitions < 1");
        assert_eq!(
            runtime.artifact_sha256_hex.len(),
            64,
            "sha256 should be 64 hex chars"
        );
        assert!(
            runtime.artifact_path.exists(),
            "artifact missing at {}",
            runtime.artifact_path.display()
        );

        // M17 phase 3b-real gating check — invoke the body and verify
        // the registered CounterBody function ran end-to-end through
        // the FFI + dispatch table.
        let module = runtime
            .engine
            .borrow_mut()
            .eval_module(runtime.iso, "Counter", "")
            .expect("eval_module Counter");
        let body_fn = runtime
            .engine
            .borrow_mut()
            .resolve_function(runtime.iso, module, "body")
            .expect("resolve body");
        let ctx = HostValue::Object(vec![
            ("id".into(), HostValue::String("counter-1".into())),
            ("props".into(), HostValue::Object(Vec::new())),
            ("neighbors".into(), HostValue::Array(Vec::new())),
        ]);
        let manifest = runtime
            .engine
            .borrow_mut()
            .call_function(runtime.iso, body_fn, &[ctx])
            .expect("call body");
        let HostValue::Object(pairs) = &manifest else {
            panic!("expected Manifest object, got {manifest:?}");
        };
        let id = pairs
            .iter()
            .find_map(|(k, v)| {
                if k == "id" {
                    if let HostValue::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .expect("manifest missing id");
        assert_eq!(id, "counter-1", "got manifest {manifest:?}");
    }

    /// Phase 3c: emit fires on first render → scheduler delivers →
    /// receiver's UseMessage handler runs → observation captured.
    /// Mirrors pyruntime's `phase3c_python_emit_dispatches_to_neighbor`
    /// at reduced scope (no use_state, no use_effect — bodies emit
    /// directly during render and the receiver's handler invokes a
    /// captured `host_observe` channel).
    fn scenario_emit_dispatches_to_neighbor() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go-emit-smoke.grx");
        assert!(fixture.exists(), "fixture missing at {}", fixture.display());

        let out_dir = fresh_out_dir("emit");
        let mut runtime = mount_with_scheduler(&fixture, &out_dir)
            .expect("mount_with_scheduler should succeed for go-emit-smoke.grx");

        // Register a host_observe callback that the receiver's body
        // calls when its handler fires.
        let observations: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observations_for_cb = std::sync::Arc::clone(&observations);
        runtime
            .engine
            .borrow_mut()
            .register_callback(
                runtime.iso,
                "host_observe",
                Box::new(move |args| {
                    if let Some(HostValue::String(s)) = args.first() {
                        observations_for_cb.lock().push(s.clone());
                    }
                    Ok(HostValue::Null)
                }),
            )
            .expect("register host_observe");

        // Initial mount: process_batch evaluates each body once.
        // Pinger emits 'ping' to its neighbor Ponger; Ponger's
        // UseMessage handler observes 'pong-received'.
        runtime.process_batch().expect("batch 1");

        let obs = observations.lock();
        assert!(
            obs.contains(&"pong-received".to_string()),
            "Ponger's UseMessage handler should have observed the ping from Pinger: {obs:?}"
        );
        drop(obs);

        runtime.shutdown().ok();
    }

    /// Phase 3c: feeding a `lang="js"` definition to the Go arm
    /// surfaces a clean EngineMisroute diagnostic. Mirrors
    /// pyruntime::phase3a_rejects_js_lang_definition.
    fn scenario_rejects_non_go_lang() {
        // Build an IR with a single js definition + run through
        // mount_with_scheduler via a fixture that mixes langs.
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go-mixed-rejected.grx");
        assert!(fixture.exists(), "fixture missing at {}", fixture.display());

        let out_dir = fresh_out_dir("misroute");
        let result = mount_with_scheduler(&fixture, &out_dir);
        match result {
            Err(RuntimeError::EngineMisroute { engine, lang, .. }) => {
                assert_eq!(engine, "go");
                assert_eq!(lang, "js");
            }
            other => panic!("expected EngineMisroute, got {other:?}"),
        }
    }

    /// Single-test runner — all goruntime end-to-end scenarios in
    /// one process, exits 0 at the end to dodge the Go TSD-cleanup
    /// deadlock. See module-level docstring.
    #[test]
    fn end_to_end() {
        if skip_if_no_go() {
            return;
        }
        eprintln!("[goruntime smoke 1/3] build_and_load + body dispatch");
        scenario_build_and_load_body_dispatch();
        eprintln!("[goruntime smoke 2/3] EngineMisroute rejects non-Go bodies");
        scenario_rejects_non_go_lang();
        eprintln!("[goruntime smoke 3/3] mount_with_scheduler + emit/message round-trip");
        scenario_emit_dispatches_to_neighbor();
        eprintln!("[goruntime smoke] all scenarios passed");
        // Exit fast — see module-level docs on the Go TSD-cleanup
        // hazard. We leak the GoRuntime instances on purpose; the
        // process is terminating immediately.
        std::process::exit(0);
    }

    /// Negative test — build_and_load on a missing file surfaces a
    /// clean error. Runs in a separate test so it executes WITHOUT
    /// the Go runtime being loaded (no TSD hazard).
    #[test]
    fn build_and_load_rejects_missing_file() {
        let nowhere = Path::new("/tmp/grx-this-file-definitely-does-not-exist.grx");
        let out_dir = std::env::temp_dir().join("grx-goruntime-test");
        let result = build_and_load(nowhere, &out_dir);
        assert!(result.is_err());
    }
}
