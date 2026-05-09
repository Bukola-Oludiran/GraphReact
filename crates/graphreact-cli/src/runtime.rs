//! Mounting a parsed `.grx` IR onto an engine.
//!
//! In M12.1 this is a "dry-mount" — we walk the IR, eval each
//! definition block on the engine, register placeholder host
//! callbacks, and call each component's `init` function once. With
//! the null engine this exercises every trait method without
//! executing JS. With JSC (M12.3) this same path runs the components
//! end-to-end.

use thiserror::Error;

use graphreact_engine_zig::{
    BoxCallback, Engine, EngineError, HostValue, IsolateHandle, IsolateOptions,
};
use graphreact_grx_parser::{
    multi_file::{resolve_multi_file, resolve_multi_file_cached, MultiFileResolved, ParseCache},
    GrxFile, GrxIr,
};

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("engine error: {0}")]
    Engine(#[from] EngineError),
    #[error("parser error: {0}")]
    Parser(#[from] graphreact_grx_parser::GrxParseError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("graph in `{path}` has no <Graph> block")]
    NoGraph { path: String },
    #[error("scheduler error: {0}")]
    Scheduler(#[from] graphreact_core::RuntimeError),
    #[error(
        "component `<{component}>` declares `lang=\"{lang}\"` but the active engine is `{engine}`. \
         Use `--runtime={lang}` (or remove `lang` to default to JS) to mount it."
    )]
    EngineMisroute {
        component: String,
        lang: String,
        engine: String,
    },
}

pub type RuntimeResult<T> = Result<T, RuntimeError>;

/// **M15 Phase 9.** Assemble a [`GrxIr`] snapshot from a resolved
/// multi-file `.grx` tree. The same shape of IR construction lives
/// inline inside [`crate::jsruntime::mount_with_scheduler`] /
/// [`crate::pyruntime::mount_with_scheduler`]; this helper lets the
/// hot-reload watcher take a pre-mount IR snapshot from disk for
/// `classify(before, after, ...)` without rebuilding the
/// engine.
///
/// `MultiFileResolved` may have no `<Graph>` block — in which case
/// `RuntimeError::NoGraph` is returned, mirroring the mount-path
/// error.
pub fn assemble_resolved_ir(resolved: &MultiFileResolved) -> RuntimeResult<GrxIr> {
    let graph = resolved
        .root
        .graph
        .as_ref()
        .ok_or_else(|| RuntimeError::NoGraph {
            path: resolved.root_path.display().to_string(),
        })?;

    let mut definitions: Vec<graphreact_grx_parser::ir::IrDefinition> = Vec::new();
    for d in &resolved.root.definitions {
        definitions.push(graphreact_grx_parser::ir::IrDefinition {
            component: d.component.clone(),
            code: d.code.clone(),
            lang: d.lang,
            source_loc: graphreact_grx_parser::SourceLoc::default(),
        });
    }
    for imp in &resolved.imports {
        for d in &imp.file.definitions {
            definitions.push(graphreact_grx_parser::ir::IrDefinition {
                component: d.component.clone(),
                code: d.code.clone(),
                lang: d.lang,
                source_loc: graphreact_grx_parser::SourceLoc::default(),
            });
        }
    }

    let nodes_edges_ir = graphreact_grx_parser::ir::build_ir(&GrxFile {
        imports: resolved.root.imports.clone(),
        graph: Some(graph.clone()),
        definitions: resolved.root.definitions.clone(),
    });

    Ok(GrxIr {
        graph_id: graph.id.clone(),
        graph_loc: graph.loc,
        nodes: nodes_edges_ir.nodes,
        edges: nodes_edges_ir.edges,
        definitions,
        imports: resolved
            .imports
            .iter()
            .map(|i| i.path.display().to_string())
            .collect(),
    })
}

/// **M15 Phase 9.** Convenience: resolve from disk plus assemble in
/// one shot. Used by the watch driver to take initial-IR plus
/// post-edit-IR snapshots from a path.
pub fn resolve_and_assemble_ir(
    path: &std::path::Path,
) -> RuntimeResult<(MultiFileResolved, GrxIr)> {
    let resolved = resolve_multi_file(path)?;
    let ir = assemble_resolved_ir(&resolved)?;
    Ok((resolved, ir))
}

/// **M15 Phase 1 slice 1A.** Cache-aware variant of
/// [`resolve_and_assemble_ir`]. The watch driver calls this on every
/// reload event so unchanged imports skip reparse — only the
/// edited file is re-read plus re-parsed; the rest are cache hits.
///
/// Same return shape as the non-cached variant; differs only in
/// performance. The cache is caller-supplied so watch-loop ownership
/// is explicit.
pub fn resolve_and_assemble_ir_cached(
    path: &std::path::Path,
    cache: &mut ParseCache,
) -> RuntimeResult<(MultiFileResolved, GrxIr)> {
    let resolved = resolve_multi_file_cached(path, cache)?;
    let ir = assemble_resolved_ir(&resolved)?;
    Ok((resolved, ir))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryMountReport {
    pub engine_name: String,
    pub engine_version: String,
    pub graph_id: String,
    pub n_nodes: usize,
    pub n_edges: usize,
    pub n_definitions: usize,
    pub mounted_components: Vec<String>,
}

/// Parse + dry-mount a `.grx` file using the given engine.
pub fn dry_mount(
    path: &std::path::Path,
    engine: &mut dyn Engine,
) -> RuntimeResult<DryMountReport> {
    let resolved = resolve_multi_file(path)?;

    // Concatenate root + imported definition blocks.
    let mut definitions: Vec<(String, String, String, graphreact_grx_parser::ast::ComponentLang)> =
        Vec::new();
    for d in &resolved.root.definitions {
        definitions.push((
            resolved.root_path.display().to_string(),
            d.component.clone(),
            d.code.clone(),
            d.lang,
        ));
    }
    for imp in &resolved.imports {
        for d in &imp.file.definitions {
            definitions.push((
                imp.path.display().to_string(),
                d.component.clone(),
                d.code.clone(),
                d.lang,
            ));
        }
    }

    let graph = resolved.root.graph.as_ref().ok_or_else(|| RuntimeError::NoGraph {
        path: resolved.root_path.display().to_string(),
    })?;

    let ir = GrxIr {
        graph_id: graph.id.clone(),
        graph_loc: graph.loc,
        nodes: graphreact_grx_parser::ir::build_ir(&graphreact_grx_parser::GrxFile {
            imports: resolved.root.imports.clone(),
            graph: Some(graph.clone()),
            definitions: resolved.root.definitions.clone(),
        })
        .nodes,
        edges: graphreact_grx_parser::ir::build_ir(&graphreact_grx_parser::GrxFile {
            imports: resolved.root.imports.clone(),
            graph: Some(graph.clone()),
            definitions: resolved.root.definitions.clone(),
        })
        .edges,
        definitions: definitions
            .iter()
            .map(|(_, comp, code, lang)| graphreact_grx_parser::ir::IrDefinition {
                component: comp.clone(),
                code: code.clone(),
                lang: *lang,
                source_loc: graphreact_grx_parser::SourceLoc::default(),
            })
            .collect(),
        imports: resolved.imports.iter().map(|i| i.path.display().to_string()).collect(),
    };

    engine.initialize()?;
    let iso = engine.create_isolate(IsolateOptions::default())?;

    register_host_callbacks(engine, iso)?;

    let mut mounted: Vec<String> = Vec::new();
    for (origin, component, source, lang) in &definitions {
        // Trait-level dry_mount routes through the generic
        // `Engine::eval_module` which doesn't know about lang. Skip
        // non-JS bodies on this path; the cpython arm has its own
        // mount routine that handles `lang="python"`.
        if *lang != graphreact_grx_parser::ast::ComponentLang::Js {
            continue;
        }
        let module_name = format!("{origin}::{component}");
        let module = engine.eval_module(iso, &module_name, source)?;
        // Mount = "the body evaluated cleanly". Calling `init` is
        // best-effort: most fixtures use the implicit
        // `return { accepts, emits }` form (no separate `init`
        // property on the manifest), so resolve_function will return
        // InvalidFunction — that's fine, we still count the
        // component as mounted because eval_module succeeded.
        mounted.push(component.clone());
        if let Ok(init_fn) = engine.resolve_function(iso, module, "init") {
            let _ = engine.call_function(iso, init_fn, &[]);
        }
    }

    engine.drain_microtasks(iso)?;
    let report = DryMountReport {
        engine_name: engine.engine_name().to_string(),
        engine_version: engine.engine_version(),
        graph_id: ir.graph_id.clone(),
        n_nodes: ir.nodes.len(),
        n_edges: ir.edges.len(),
        n_definitions: ir.definitions.len(),
        mounted_components: mounted,
    };
    engine.dispose_isolate(iso)?;
    engine.shutdown()?;
    Ok(report)
}

fn register_host_callbacks(engine: &mut dyn Engine, iso: IsolateHandle) -> RuntimeResult<()> {
    let host_send: BoxCallback = Box::new(|_args| Ok(HostValue::Null));
    let host_log: BoxCallback = Box::new(|_args| Ok(HostValue::Null));
    let host_dispatch: BoxCallback = Box::new(|_args| Ok(HostValue::Null));
    engine.register_callback(iso, "host_send", host_send)?;
    engine.register_callback(iso, "host_log", host_log)?;
    engine.register_callback(iso, "host_dispatch", host_dispatch)?;
    Ok(())
}

/// Phase-3a JSC mount path. Diverges from [`dry_mount`] (which uses
/// `eval_module`'s IIFE one-shot pattern) by:
///
/// 1. Registering each component body as a **re-callable function**
///    via [`graphreact_engine_jsc::JscEngine::eval_component`], and
/// 2. Calling each component function once per node with a
///    **per-instance ctx** (`id`, `props`) marshalled from the IR.
///
/// This is the eval pattern phase 3 hook semantics need: top-level
/// `useState` / `useEmit` / `useEffect` calls inside a component body
/// fire on **every** render (= every call), not once at module-eval
/// time. Phase 3a doesn't yet wire those hooks to the scheduler — they
/// remain no-op stubs in `HOOK_PRELUDE`. The integration shape lives
/// here so phase 3b can layer real scheduler routing on top without
/// re-architecting the mount path.
#[cfg(target_os = "macos")]
pub fn dry_mount_jsc(
    path: &std::path::Path,
    engine: &mut graphreact_engine_jsc::JscEngine,
) -> RuntimeResult<DryMountReport> {
    use graphreact_engine_zig::FunctionHandle;
    use std::collections::HashMap;

    let resolved = resolve_multi_file(path)?;

    // Concatenate root + imported definition blocks. Same as the
    // legacy path — definitions are component-name-keyed bodies, not
    // per-node.
    let mut definitions: Vec<(String, String, String, graphreact_grx_parser::ast::ComponentLang)> =
        Vec::new();
    for d in &resolved.root.definitions {
        definitions.push((
            resolved.root_path.display().to_string(),
            d.component.clone(),
            d.code.clone(),
            d.lang,
        ));
    }
    for imp in &resolved.imports {
        for d in &imp.file.definitions {
            definitions.push((
                imp.path.display().to_string(),
                d.component.clone(),
                d.code.clone(),
                d.lang,
            ));
        }
    }

    let graph = resolved.root.graph.as_ref().ok_or_else(|| RuntimeError::NoGraph {
        path: resolved.root_path.display().to_string(),
    })?;

    let ir = GrxIr {
        graph_id: graph.id.clone(),
        graph_loc: graph.loc,
        nodes: graphreact_grx_parser::ir::build_ir(&graphreact_grx_parser::GrxFile {
            imports: resolved.root.imports.clone(),
            graph: Some(graph.clone()),
            definitions: resolved.root.definitions.clone(),
        })
        .nodes,
        edges: graphreact_grx_parser::ir::build_ir(&graphreact_grx_parser::GrxFile {
            imports: resolved.root.imports.clone(),
            graph: Some(graph.clone()),
            definitions: resolved.root.definitions.clone(),
        })
        .edges,
        definitions: definitions
            .iter()
            .map(|(_, comp, code, lang)| graphreact_grx_parser::ir::IrDefinition {
                component: comp.clone(),
                code: code.clone(),
                lang: *lang,
                source_loc: graphreact_grx_parser::SourceLoc::default(),
            })
            .collect(),
        imports: resolved.imports.iter().map(|i| i.path.display().to_string()).collect(),
    };

    Engine::initialize(engine)?;
    let iso = Engine::create_isolate(engine, IsolateOptions::default())?;

    register_host_callbacks(engine, iso)?;
    // Register stub host_* so the prelude's hook delegations don't
    // throw under the dry-mount path. Real semantics live in
    // [`crate::jsruntime::mount_with_scheduler`].
    Engine::register_callback(
        engine,
        iso,
        "host_state",
        crate::jsruntime::build_host_state_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_state_set",
        crate::jsruntime::build_host_state_set_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_effect",
        crate::jsruntime::build_host_effect_stub_callback(),
    )?;
    // M12.3b-phase1: useEffectAsync stubs.
    Engine::register_callback(
        engine,
        iso,
        "host_effect_async",
        crate::jsruntime::build_host_effect_async_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_effect_async_complete",
        crate::jsruntime::build_host_effect_async_complete_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_emit",
        crate::jsruntime::build_host_emit_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_message",
        crate::jsruntime::build_host_message_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_memo_check",
        crate::jsruntime::build_host_memo_check_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_memo_store",
        crate::jsruntime::build_host_memo_store_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_neighbors",
        crate::jsruntime::build_host_neighbors_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_topology",
        crate::jsruntime::build_host_topology_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_accept_edge",
        crate::jsruntime::build_host_accept_edge_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_spawn_child",
        crate::jsruntime::build_host_spawn_child_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_unmount_self",
        crate::jsruntime::build_host_unmount_self_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_compose",
        crate::jsruntime::build_host_compose_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_directory_find_by_id",
        crate::jsruntime::build_host_directory_find_by_id_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_directory_find_by_role",
        crate::jsruntime::build_host_directory_find_by_role_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_directory_find_by_depth",
        crate::jsruntime::build_host_directory_find_by_depth_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_directory_find_in_subtree",
        crate::jsruntime::build_host_directory_find_in_subtree_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_directory_is_ancestor",
        crate::jsruntime::build_host_directory_is_ancestor_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_graph_neighbors",
        crate::jsruntime::build_host_graph_neighbors_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_graph_has_node",
        crate::jsruntime::build_host_graph_has_node_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_graph_has_edge",
        crate::jsruntime::build_host_graph_has_edge_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_graph_node_count",
        crate::jsruntime::build_host_graph_node_count_stub_callback(),
    )?;
    // M12.3a-followon-B: algorithm-surface accessors (stubs in dry-mount).
    Engine::register_callback(
        engine,
        iso,
        "host_graph_shortest_path",
        crate::jsruntime::build_host_graph_shortest_path_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_graph_all_paths",
        crate::jsruntime::build_host_graph_all_paths_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_graph_a_star_path",
        crate::jsruntime::build_host_graph_a_star_path_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_graph_bfs",
        crate::jsruntime::build_host_graph_bfs_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_graph_dfs",
        crate::jsruntime::build_host_graph_dfs_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_graph_connected_components",
        crate::jsruntime::build_host_graph_connected_components_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_graph_cycle_detection",
        crate::jsruntime::build_host_graph_cycle_detection_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_use_source",
        crate::jsruntime::build_host_use_source_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_monitor",
        crate::jsruntime::build_host_monitor_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_demonitor",
        crate::jsruntime::build_host_demonitor_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_link",
        crate::jsruntime::build_host_link_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_unlink",
        crate::jsruntime::build_host_unlink_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_group_join",
        crate::jsruntime::build_host_group_join_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_group_leave",
        crate::jsruntime::build_host_group_leave_stub_callback(),
    )?;
    // M12.3a-followon-D: stub variant for the dry-mount path. The JS
    // prelude's `__registerCallback` short-circuits when the callback
    // is missing, but exposing a stub keeps the registry shape uniform
    // and avoids host_track_token-undefined warnings if dry-mount JS
    // ever exercises hooks that call `__registerCallback`.
    Engine::register_callback(
        engine,
        iso,
        "host_track_token",
        crate::jsruntime::build_host_track_token_stub_callback(),
    )?;

    // M12.3a-followon-E: stub variants of the 9 Tier 2 callbacks. The
    // dry-mount path doesn't have a manifest index to query against,
    // so each stub returns the neutral default that the JS proxy
    // already returns when its host callback is missing. Keeps the
    // registry shape uniform and lets dry-mount JS bodies use
    // `g.tier2(...)` without ReferenceErrors.
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_node_count",
        crate::jsruntime::build_host_tier2_node_count_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_has_node",
        crate::jsruntime::build_host_tier2_has_node_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_neighbors",
        crate::jsruntime::build_host_tier2_neighbors_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_has_edge",
        crate::jsruntime::build_host_tier2_has_edge_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_bfs",
        crate::jsruntime::build_host_tier2_bfs_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_dfs",
        crate::jsruntime::build_host_tier2_dfs_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_shortest_path",
        crate::jsruntime::build_host_tier2_shortest_path_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_topological_sort",
        crate::jsruntime::build_host_tier2_topological_sort_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_a_star_path",
        crate::jsruntime::build_host_tier2_a_star_path_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_all_paths",
        crate::jsruntime::build_host_tier2_all_paths_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_connected_components",
        crate::jsruntime::build_host_tier2_connected_components_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier2_cycle_detection",
        crate::jsruntime::build_host_tier2_cycle_detection_stub_callback(),
    )?;
    // M12.3a-followon-F: Tier 3 stubs (mirror tier2 for the dry-mount
    // path). Same registry-uniformity argument as tier2 stubs.
    Engine::register_callback(
        engine,
        iso,
        "host_edge_accept_handler",
        crate::jsruntime::build_host_edge_accept_handler_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_node_count",
        crate::jsruntime::build_host_tier3_node_count_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_has_node",
        crate::jsruntime::build_host_tier3_has_node_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_neighbors",
        crate::jsruntime::build_host_tier3_neighbors_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_has_edge",
        crate::jsruntime::build_host_tier3_has_edge_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_bfs",
        crate::jsruntime::build_host_tier3_bfs_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_dfs",
        crate::jsruntime::build_host_tier3_dfs_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_shortest_path",
        crate::jsruntime::build_host_tier3_shortest_path_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_topological_sort",
        crate::jsruntime::build_host_tier3_topological_sort_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_a_star_path",
        crate::jsruntime::build_host_tier3_a_star_path_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_all_paths",
        crate::jsruntime::build_host_tier3_all_paths_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_connected_components",
        crate::jsruntime::build_host_tier3_connected_components_stub_callback(),
    )?;
    Engine::register_callback(
        engine,
        iso,
        "host_tier3_cycle_detection",
        crate::jsruntime::build_host_tier3_cycle_detection_stub_callback(),
    )?;

    // Eval each component definition as a re-callable function.
    // Component-name → function handle. One handle per component;
    // multiple nodes of the same component share the same handle and
    // are differentiated by ctx.
    let mut component_handles: HashMap<String, FunctionHandle> = HashMap::new();
    for (origin, component, source, lang) in &definitions {
        // dry_mount_jsc routes everything through the JSC engine; a
        // non-JS lang here is a fixture-runtime mismatch (e.g.,
        // `lang="python"` body fed into the JSC eval path). Fail
        // loudly with a clear diagnostic — phase 3+ adds the
        // PyRuntime path that handles the multi-engine case.
        if *lang != graphreact_grx_parser::ast::ComponentLang::Js {
            return Err(RuntimeError::EngineMisroute {
                component: component.clone(),
                lang: lang.as_attr_str().to_string(),
                engine: "jsc".to_string(),
            });
        }
        let module_name = format!("{origin}::{component}");
        let handle = engine.eval_component(iso, &module_name, source)?;
        component_handles.insert(component.clone(), handle);
    }

    // For each node in the IR, call its component function with a
    // per-instance ctx. The body returns its manifest each call.
    // Phase 3a logs nodes as "mounted" once their function returns
    // without throwing. Phase 3b will route this through
    // graphreact-core's Scheduler for real hook state preservation.
    //
    // Two `.grx` author conventions are supported:
    // 1. `<TempSensor id="..." />` — the tag IS the component name;
    //    `node.component == "TempSensor"`.
    // 2. `<NodeType id="A" component="Pinger" />` — the component
    //    name lives on `attrs.component`; `node.component` is the
    //    abstract tag (`"NodeType"`).
    // Prefer the attrs override, fall back to the tag.
    let mut mounted: Vec<String> = Vec::new();
    for node in &ir.nodes {
        let component_name = node
            .attrs
            .get("component")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| node.component.clone());
        let Some(&fn_handle) = component_handles.get(&component_name) else {
            // No matching definition — definitionless node. The
            // legacy path would skip silently; keep that behavior so
            // mount stays loose for nodes that intentionally have no
            // body (e.g. structural-only).
            continue;
        };
        let ctx = build_node_ctx(node);
        let _manifest = Engine::call_function(engine, iso, fn_handle, &[ctx])?;
        mounted.push(node.id.clone());
    }

    Engine::drain_microtasks(engine, iso)?;
    let report = DryMountReport {
        engine_name: Engine::engine_name(engine).to_string(),
        engine_version: Engine::engine_version(engine),
        graph_id: ir.graph_id.clone(),
        n_nodes: ir.nodes.len(),
        n_edges: ir.edges.len(),
        n_definitions: ir.definitions.len(),
        mounted_components: mounted,
    };
    Engine::dispose_isolate(engine, iso)?;
    Engine::shutdown(engine)?;
    Ok(report)
}

/// Build a per-instance JSC `ctx` from an IR node. Carries:
/// - `id` — the node's stable string id
/// - `props` — the node's `attrs` map, minus parser-injected
///   bookkeeping keys (`component`, `_depth`, `_parentPath`)
/// - `directory` — stub object with no-op finders (phase 3b populates)
/// - mutation handles (`spawnChild`/`unmount`/`compose`/`acceptEdge`)
///   — stubs in phase 3a; phase 3b wires them through host callbacks
#[cfg(target_os = "macos")]
fn build_node_ctx(node: &graphreact_grx_parser::ir::IrNode) -> HostValue {
    use graphreact_engine_zig::HostValue as HV;

    // props = attrs minus the IR's bookkeeping keys.
    const HIDDEN_KEYS: &[&str] = &["component", "_depth", "_parentPath"];
    let mut props_pairs: Vec<(String, HV)> = Vec::new();
    for (k, v) in &node.attrs {
        if HIDDEN_KEYS.contains(&k.as_str()) {
            continue;
        }
        props_pairs.push((k.clone(), HV::from_json(v.clone())));
    }

    HV::Object(vec![
        ("id".to_string(), HV::String(node.id.clone())),
        ("props".to_string(), HV::Object(props_pairs)),
        // Stub directory — phase 3b populates findById/findByRole/etc.
        // via host callbacks routed through the graph directory.
        (
            "directory".to_string(),
            HV::Object(vec![
                ("_stub".to_string(), HV::Bool(true)),
            ]),
        ),
    ])
}
