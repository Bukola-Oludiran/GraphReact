//! Intermediate representation: the runtime-ingestion shape of a
//! parsed `.grx` file.
//!
//! Produced by [`build_ir`] from an [`ast::GrxFile`]. Mirrors the
//! `(components, edges, channels, supervisors)` data the TypeScript
//! [`compiler.ts`] feeds into `CommunicationGraph` + `Scheduler`,
//! minus the `NodeInstance` runtime shape — that lives in
//! [`graphreact_core`].
//!
//! Source-map preservation: every IR node and edge carries the
//! [`SourceLoc`] of its originating tag, so runtime errors can
//! point back at the `.grx` line and column.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::ast::{AttrMap, ComponentLang, GrxFile, GrxNode, NodeDecl, SubgraphDecl};
use crate::error::{GrxParseError, GrxResult, SourceLoc};

/// PCBPnR Phase 5-D §2.3 — lookup table for Subsystem splice
/// resolution. Built from a [`GrxFile`]'s `subgraphs` (and, for the
/// multi-file case, any imported files' subgraphs that the consumer
/// rolls up with alias qualification before calling [`build_ir`]).
#[derive(Debug, Default)]
struct SubgraphLookup<'a> {
    by_qualified_name: rustc_hash::FxHashMap<String, &'a SubgraphDecl>,
}

impl<'a> SubgraphLookup<'a> {
    fn from_file(file: &'a GrxFile) -> Self {
        let mut by_qualified_name = rustc_hash::FxHashMap::default();
        for sg in &file.subgraphs {
            // First-write-wins on duplicates (within-file duplicates
            // are caught at parse time via DuplicateSubgraphName; this
            // is purely defensive).
            by_qualified_name.entry(sg.name.clone()).or_insert(sg);
        }
        Self { by_qualified_name }
    }
}

/// Tracks the first declaration of a given node id so subsequent
/// occurrences can be validated as attribute-free multi-parent
/// references (PCBPnR Phase 5-D §2.4 reconciliation). See the
/// rationale comment on [`GrxParseError::IdConflictWithAttrs`].
///
/// `pub(crate)` so the tree-sitter bridge ([`crate::treesitter`])
/// can share the type and stay in lockstep on the reconciled rule.
#[derive(Debug, Clone)]
pub(crate) struct FirstOccurrence {
    pub(crate) loc: SourceLoc,
    pub(crate) component: String,
}

/// Built `.grx` IR. Suitable for handing to a runtime that expects:
/// node ids + per-node attributes + edges (parent-child + explicit).
///
/// **M15 Phase 10**: serialised over the wire as the payload of
/// `PacketType::ReloadModule` so cluster-wide hot reload can ship
/// origin-authoritative IR to peers without each peer re-parsing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrxIr {
    pub graph_id: String,
    pub graph_loc: SourceLoc,
    pub nodes: Vec<IrNode>,
    pub edges: Vec<IrEdge>,
    #[serde(default)]
    pub entrypoints: Vec<IrEntrypoint>,
    /// Component-body code blocks (Phase-7 `<ComponentName>...code...</ComponentName>`).
    pub definitions: Vec<IrDefinition>,
    /// Imported file paths in declaration order (already resolved by
    /// the multi-file resolver, if any).
    pub imports: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrEntrypoint {
    pub name: String,
    pub target: String,
    pub request: String,
    pub reply: String,
    pub source_loc: SourceLoc,
}

/// One node in the IR. Hierarchy info (`depth`, `parent_path`) and
/// component name come from the AST; `attrs` is the verbatim
/// attribute map *plus* injected `component`, `_depth`, and
/// `_parentPath` keys to mirror the TS compiler exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrNode {
    pub id: String,
    pub component: String,
    pub attrs: AttrMap,
    pub depth: usize,
    pub parent_path: Vec<String>,
    pub source_loc: SourceLoc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrEdge {
    pub source: String,
    pub target: String,
    pub attrs: AttrMap,
    pub source_loc: SourceLoc,
    /// Distinguishes structural edges (created from `<NodeA>`-inside-
    /// `<NodeB>` nesting) from explicit `<edge source="..." target="..." />`.
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    /// Edge implied by parent-child nesting in the markup.
    Structural,
    /// Edge declared via `<edge source="..." target="..." ... />`.
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrDefinition {
    pub component: String,
    pub code: String,
    /// Body language. Defaults to [`ComponentLang::Js`] for fixtures
    /// that omit the `lang` attribute (M13-phase3a back-compat).
    pub lang: ComponentLang,
    pub source_loc: SourceLoc,
}

/// Build the IR from a parsed file.
///
/// # Errors
///
/// Returns [`GrxParseError::IdConflictWithAttrs`] when a node id is
/// declared twice within the same graph and the subsequent
/// occurrence carries any attribute beyond `id` (PCBPnR Phase 5-D
/// §2.4 reconciliation). Returns [`GrxParseError::IdComponentMismatch`]
/// when a subsequent occurrence uses a different component tag than
/// the first. Attribute-free re-mentions remain the load-bearing
/// multi-parent pattern and are NOT errors. Returns
/// [`GrxParseError::SubsystemUnknownRef`] / [`GrxParseError::SpliceCycle`]
/// / etc. for malformed or unresolvable `<Subsystem>` splice
/// references (PCBPnR Phase 5-D §2.3).
pub fn build_ir(file: &GrxFile) -> GrxResult<GrxIr> {
    let graph = file.graph.as_ref();
    let graph_id = graph.map(|g| g.id.clone()).unwrap_or_default();
    let graph_loc = graph.map(|g| g.loc).unwrap_or_default();

    let subgraphs = SubgraphLookup::from_file(file);

    let mut nodes: Vec<IrNode> = Vec::new();
    let mut edges: Vec<IrEdge> = Vec::new();
    let mut seen_ids: rustc_hash::FxHashMap<String, FirstOccurrence> =
        rustc_hash::FxHashMap::default();

    if let Some(g) = graph {
        let mut splice_stack: Vec<String> = Vec::new();
        process_children(
            &g.children,
            None,
            0,
            &[],
            "",
            &subgraphs,
            &mut splice_stack,
            &mut nodes,
            &mut edges,
            &mut seen_ids,
        )?;
    }

    let definitions = file
        .definitions
        .iter()
        .map(|d| IrDefinition {
            component: d.component.clone(),
            code: d.code.clone(),
            lang: d.lang,
            source_loc: d.loc,
        })
        .collect();

    let imports = file.imports.iter().map(|i| i.src.clone()).collect();
    let entrypoints: Vec<IrEntrypoint> = graph
        .map(|g| {
            g.entrypoints
                .iter()
                .map(|entry| IrEntrypoint {
                    name: entry.name.clone(),
                    target: entry.target.clone(),
                    request: entry.request.clone(),
                    reply: entry.reply.clone(),
                    source_loc: entry.loc,
                })
                .collect()
        })
        .unwrap_or_default();

    let mut names = std::collections::BTreeSet::new();
    for entry in &entrypoints {
        if !names.insert(entry.name.clone()) {
            return Err(GrxParseError::parse(
                format!("duplicate entrypoint `{}`", entry.name),
                entry.source_loc,
            ));
        }
        if !nodes.iter().any(|node| node.id == entry.target) {
            return Err(GrxParseError::parse(
                format!(
                    "entrypoint `{}` targets unknown node `{}`",
                    entry.name, entry.target
                ),
                entry.source_loc,
            ));
        }
    }

    Ok(GrxIr {
        graph_id,
        graph_loc,
        nodes,
        edges,
        entrypoints,
        definitions,
        imports,
    })
}

/// Convenience: parse + build IR in one step.
pub fn parse_to_ir(input: &str) -> crate::GrxResult<GrxIr> {
    let file = crate::parser::Parser::parse_file(input)?;
    build_ir(&file)
}

// `id_prefix` is the splice prefix — empty at root scope, `"<local-id>."`
// (with trailing dot) inside a spliced subgraph. Concatenated with each
// node's `id` and source/target endpoints to produce fully-qualified
// ids in the outer namespace.
#[allow(clippy::too_many_arguments)]
fn process_children(
    children: &[GrxNode],
    parent_id: Option<&str>,
    depth: usize,
    parent_path: &[String],
    id_prefix: &str,
    subgraphs: &SubgraphLookup<'_>,
    splice_stack: &mut Vec<String>,
    nodes: &mut Vec<IrNode>,
    edges: &mut Vec<IrEdge>,
    seen_ids: &mut rustc_hash::FxHashMap<String, FirstOccurrence>,
) -> GrxResult<()> {
    for child in children {
        match child {
            GrxNode::Node(node) if node.component == "Subsystem" => {
                splice_subsystem(
                    node,
                    parent_id,
                    depth,
                    parent_path,
                    id_prefix,
                    subgraphs,
                    splice_stack,
                    nodes,
                    edges,
                    seen_ids,
                )?;
            }
            GrxNode::Node(node) => {
                let qualified_id = qualify_id(id_prefix, &node.id);
                let mut node_path = parent_path.to_vec();
                node_path.push(qualified_id.clone());

                // PCBPnR Phase 5-D §3 — desugar `style="…"` on this
                // node before any further IR processing. Operates on
                // an owned clone so the source AST stays intact for
                // `grx fmt` round-trips.
                let desugared_attrs = if node.attrs.contains_key("style") {
                    let mut cloned = node.attrs.clone();
                    if let Err(style_err) =
                        crate::style::desugar_style(&node.component, &mut cloned)
                    {
                        return Err(style_err.into_parse_error(node.loc));
                    }
                    cloned
                } else {
                    node.attrs.clone()
                };

                match seen_ids.get(&qualified_id) {
                    None => {
                        // First occurrence — declares the node with attrs.
                        let mut full_attrs = AttrMap::new();
                        full_attrs.insert(
                            "component".to_string(),
                            JsonValue::String(node.component.clone()),
                        );
                        full_attrs.insert(
                            "_depth".to_string(),
                            JsonValue::Number(serde_json::Number::from(depth)),
                        );
                        full_attrs.insert(
                            "_parentPath".to_string(),
                            JsonValue::Array(
                                node_path.iter().cloned().map(JsonValue::String).collect(),
                            ),
                        );
                        for (k, v) in &desugared_attrs {
                            full_attrs.insert(k.clone(), v.clone());
                        }
                        nodes.push(IrNode {
                            id: qualified_id.clone(),
                            component: node.component.clone(),
                            attrs: full_attrs,
                            depth,
                            parent_path: node_path.clone(),
                            source_loc: node.loc,
                        });
                        seen_ids.insert(
                            qualified_id.clone(),
                            FirstOccurrence {
                                loc: node.loc,
                                component: node.component.clone(),
                            },
                        );
                    }
                    Some(first) => {
                        // Subsequent occurrence — must be attribute-free
                        // re-mention with matching component tag (PCBPnR
                        // §2.4 reconciliation; preserves the multi-parent
                        // pattern at canonical fixtures like
                        // counter-network.grx + multi-file/main.grx).
                        // Note: collision check uses the desugared attrs
                        // so `style="…"` on a multi-parent reference is
                        // correctly flagged as an attrs-bearing
                        // occurrence even though the source attribute
                        // count is 1.
                        if !desugared_attrs.is_empty() {
                            return Err(GrxParseError::IdConflictWithAttrs {
                                id: qualified_id.clone(),
                                first_loc: first.loc,
                                second_loc: node.loc,
                            });
                        }
                        if first.component != node.component {
                            return Err(GrxParseError::IdComponentMismatch {
                                id: qualified_id.clone(),
                                first_component: first.component.clone(),
                                first_loc: first.loc,
                                second_component: node.component.clone(),
                                second_loc: node.loc,
                            });
                        }
                        // Valid multi-parent reference — fall through to
                        // edge emission below without re-creating the node.
                    }
                }

                if let Some(parent) = parent_id {
                    edges.push(IrEdge {
                        source: parent.to_string(),
                        target: qualified_id.clone(),
                        attrs: AttrMap::new(),
                        source_loc: node.loc,
                        kind: EdgeKind::Structural,
                    });
                }

                process_children(
                    &node.children,
                    Some(&qualified_id),
                    depth + 1,
                    &node_path,
                    id_prefix,
                    subgraphs,
                    splice_stack,
                    nodes,
                    edges,
                    seen_ids,
                )?;
            }
            GrxNode::Edge(edge) => {
                // PCBPnR §2.3 — explicit-edge endpoints get the same
                // splice prefix as nodes so spliced subgraphs' inner
                // <edge>s reach the right qualified ids.
                edges.push(IrEdge {
                    source: qualify_id(id_prefix, &edge.source),
                    target: qualify_id(id_prefix, &edge.target),
                    attrs: edge.attrs.clone(),
                    source_loc: edge.loc,
                    kind: EdgeKind::Explicit,
                });
            }
        }
    }
    Ok(())
}

/// PCBPnR §2.3 — concatenate a splice prefix with a leaf id. The
/// prefix is empty at the root scope and `"<local>."` (with trailing
/// dot) inside a spliced subgraph; chained for nested splices
/// (`"rf.bandpass.L1"`).
fn qualify_id(prefix: &str, id: &str) -> String {
    if prefix.is_empty() {
        id.to_string()
    } else {
        format!("{prefix}{id}")
    }
}

/// PCBPnR §2.3 — splice a `<Subsystem ref=... id=...>` placeholder
/// into the IR by looking up its referenced subgraph and recursively
/// processing its children with a dot-prefixed id namespace.
#[allow(clippy::too_many_arguments)]
fn splice_subsystem(
    node: &NodeDecl,
    parent_id: Option<&str>,
    depth: usize,
    parent_path: &[String],
    id_prefix: &str,
    subgraphs: &SubgraphLookup<'_>,
    splice_stack: &mut Vec<String>,
    nodes: &mut Vec<IrNode>,
    edges: &mut Vec<IrEdge>,
    seen_ids: &mut rustc_hash::FxHashMap<String, FirstOccurrence>,
) -> GrxResult<()> {
    let local_id = qualify_id(id_prefix, &node.id);
    // PCBPnR Phase 5-D §3 — desugar `style="…"` on the Subsystem
    // placeholder before splice resolution so `bbox`-shaped placement
    // attrs (etc.) ride through to the spliced IR.
    let desugared_attrs = if node.attrs.contains_key("style") {
        let mut cloned = node.attrs.clone();
        if let Err(style_err) = crate::style::desugar_style("Subsystem", &mut cloned) {
            return Err(style_err.into_parse_error(node.loc));
        }
        cloned
    } else {
        node.attrs.clone()
    };
    let ref_value = match desugared_attrs.get("ref") {
        Some(JsonValue::String(s)) => s.clone(),
        _ => return Err(GrxParseError::SubsystemMissingRef { loc: node.loc }),
    };
    // <Subsystem id="..." /> already required at parse time; defensive
    // check here would re-validate but node.id is always present (the
    // parser enforces it via MissingNodeId).

    // Reject malformed `ref` — both halves required when `:` present,
    // otherwise treated as a bare (in-file) name.
    let qualified_lookup = if let Some(colon_idx) = ref_value.find(':') {
        let (alias, name) = ref_value.split_at(colon_idx);
        let name = &name[1..]; // skip ':'
        if alias.is_empty() || name.is_empty() {
            return Err(GrxParseError::SubsystemMalformedRef {
                ref_value: ref_value.clone(),
                loc: node.loc,
            });
        }
        ref_value.clone()
    } else {
        if ref_value.is_empty() {
            return Err(GrxParseError::SubsystemMalformedRef {
                ref_value: ref_value.clone(),
                loc: node.loc,
            });
        }
        ref_value.clone()
    };

    // Cycle detection — mirrors multi_file::Resolver's DFS-with-stack
    // pattern; chain in error message records the splice path.
    if splice_stack.iter().any(|q| q == &qualified_lookup) {
        let mut chain = splice_stack.join(" → ");
        chain.push_str(" → ");
        chain.push_str(&qualified_lookup);
        return Err(GrxParseError::SpliceCycle {
            chain,
            loc: node.loc,
        });
    }

    let subgraph = subgraphs
        .by_qualified_name
        .get(&qualified_lookup)
        .copied()
        .ok_or_else(|| GrxParseError::SubsystemUnknownRef {
            qualified_name: qualified_lookup.clone(),
            loc: node.loc,
        })?;

    splice_stack.push(qualified_lookup.clone());

    // The spliced subsystem's local-id becomes the qualified prefix
    // for inner ids: `<local_id>.<inner_id>`. If a parent edge needs
    // to attach, it goes from the parent to the LOCAL id (the splice
    // boundary), not to any inner id.
    let new_prefix = format!("{local_id}.");

    // Optionally emit a structural edge from parent → splice-boundary,
    // mirroring how a normal nested NodeDecl would. The splice boundary
    // itself does NOT appear as an IrNode (the splice resolves it
    // away); instead, the subsystem's inner children all link to the
    // parent through their own structural relationships in the
    // subgraph. But callers who want the local-id materialized in IR
    // (e.g., to read `<Subsystem id="rf" bbox=...>`-shaped placement
    // attrs from outer scope) can do so by emitting a placeholder.
    //
    // For v1 we DO materialize the subsystem as an IrNode at its
    // local id so that:
    //   (a) <Net><Pin component="rf"/></Net> references it
    //   (b) layout/placement attrs (bbox, layers, …) survive into IR
    //       for downstream PCB consumers
    // The IrNode component is "Subsystem"; consumers that care about
    // splice-internal children walk via id-prefix.
    let mut placeholder_attrs = AttrMap::new();
    placeholder_attrs.insert(
        "component".to_string(),
        JsonValue::String("Subsystem".to_string()),
    );
    placeholder_attrs.insert(
        "_depth".to_string(),
        JsonValue::Number(serde_json::Number::from(depth)),
    );
    let mut placeholder_path = parent_path.to_vec();
    placeholder_path.push(local_id.clone());
    placeholder_attrs.insert(
        "_parentPath".to_string(),
        JsonValue::Array(
            placeholder_path
                .iter()
                .cloned()
                .map(JsonValue::String)
                .collect(),
        ),
    );
    for (k, v) in &desugared_attrs {
        // skip `ref` — it's a splice-resolution attribute, not a
        // domain attribute
        if k == "ref" {
            continue;
        }
        placeholder_attrs.insert(k.clone(), v.clone());
    }
    // Collision-check the placeholder id like any other node
    match seen_ids.get(&local_id) {
        None => {
            nodes.push(IrNode {
                id: local_id.clone(),
                component: "Subsystem".to_string(),
                attrs: placeholder_attrs,
                depth,
                parent_path: placeholder_path.clone(),
                source_loc: node.loc,
            });
            seen_ids.insert(
                local_id.clone(),
                FirstOccurrence {
                    loc: node.loc,
                    component: "Subsystem".to_string(),
                },
            );
        }
        Some(first) => {
            return Err(GrxParseError::IdConflictWithAttrs {
                id: local_id.clone(),
                first_loc: first.loc,
                second_loc: node.loc,
            });
        }
    }

    if let Some(parent) = parent_id {
        edges.push(IrEdge {
            source: parent.to_string(),
            target: local_id.clone(),
            attrs: AttrMap::new(),
            source_loc: node.loc,
            kind: EdgeKind::Structural,
        });
    }

    // Recurse into the subgraph's children with the new prefix. The
    // subgraph's children parent under the splice boundary (local_id).
    process_children(
        &subgraph.children,
        Some(&local_id),
        depth + 1,
        &placeholder_path,
        &new_prefix,
        subgraphs,
        splice_stack,
        nodes,
        edges,
        seen_ids,
    )?;

    splice_stack.pop();
    Ok(())
}

/// Serialise the IR to a stable JSON shape suitable for `grx parse
/// --json` (M12) or for golden-test comparison against the TS
/// compiler's output.
pub fn ir_to_json(ir: &GrxIr) -> JsonValue {
    let nodes: Vec<JsonValue> = ir
        .nodes
        .iter()
        .map(|n| {
            let mut obj = serde_json::Map::new();
            obj.insert("id".to_string(), JsonValue::String(n.id.clone()));
            obj.insert(
                "component".to_string(),
                JsonValue::String(n.component.clone()),
            );
            obj.insert("depth".to_string(), JsonValue::Number(n.depth.into()));
            obj.insert(
                "parentPath".to_string(),
                JsonValue::Array(
                    n.parent_path
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            );
            obj.insert("attrs".to_string(), JsonValue::Object(n.attrs.clone()));
            obj.insert("loc".to_string(), loc_to_json(n.source_loc));
            JsonValue::Object(obj)
        })
        .collect();

    let edges: Vec<JsonValue> = ir
        .edges
        .iter()
        .map(|e| {
            let mut obj = serde_json::Map::new();
            obj.insert("source".to_string(), JsonValue::String(e.source.clone()));
            obj.insert("target".to_string(), JsonValue::String(e.target.clone()));
            obj.insert(
                "kind".to_string(),
                JsonValue::String(
                    match e.kind {
                        EdgeKind::Structural => "structural",
                        EdgeKind::Explicit => "explicit",
                    }
                    .to_string(),
                ),
            );
            obj.insert("attrs".to_string(), JsonValue::Object(e.attrs.clone()));
            obj.insert("loc".to_string(), loc_to_json(e.source_loc));
            JsonValue::Object(obj)
        })
        .collect();

    let definitions: Vec<JsonValue> = ir
        .definitions
        .iter()
        .map(|d| {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "component".to_string(),
                JsonValue::String(d.component.clone()),
            );
            obj.insert("code".to_string(), JsonValue::String(d.code.clone()));
            obj.insert(
                "lang".to_string(),
                JsonValue::String(d.lang.as_attr_str().to_string()),
            );
            obj.insert("loc".to_string(), loc_to_json(d.source_loc));
            JsonValue::Object(obj)
        })
        .collect();

    let mut top = serde_json::Map::new();
    top.insert(
        "graphId".to_string(),
        JsonValue::String(ir.graph_id.clone()),
    );
    top.insert("graphLoc".to_string(), loc_to_json(ir.graph_loc));
    top.insert(
        "imports".to_string(),
        JsonValue::Array(ir.imports.iter().cloned().map(JsonValue::String).collect()),
    );
    top.insert("nodes".to_string(), JsonValue::Array(nodes));
    top.insert("edges".to_string(), JsonValue::Array(edges));
    top.insert("definitions".to_string(), JsonValue::Array(definitions));
    JsonValue::Object(top)
}

fn loc_to_json(loc: SourceLoc) -> JsonValue {
    let mut o = serde_json::Map::new();
    o.insert("line".to_string(), JsonValue::Number(loc.line.into()));
    o.insert("column".to_string(), JsonValue::Number(loc.column.into()));
    JsonValue::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_entrypoint_is_captured_and_validated() {
        let ir = parse_to_ir(
            r#"<Graph id="agents">
                <Agent id="agent" />
                <entrypoint name="summarize" target="agent" request="prompt" reply="completion" />
            </Graph>"#,
        )
        .unwrap();
        assert_eq!(ir.entrypoints.len(), 1);
        assert_eq!(ir.entrypoints[0].name, "summarize");
        assert_eq!(ir.entrypoints[0].target, "agent");
    }

    #[test]
    fn durable_entrypoint_rejects_unknown_target() {
        let error = parse_to_ir(
            r#"<Graph id="agents">
                <entrypoint name="summarize" target="missing" request="prompt" reply="completion" />
            </Graph>"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown node `missing`"));
    }
    use crate::parser::Parser;

    #[test]
    fn ir_captures_nesting_as_structural_edges() {
        let f = Parser::parse_file(
            "<Graph id=\"g\"><A id=\"a\"><B id=\"b\"><C id=\"c\" /></B></A></Graph>",
        )
        .expect("ok");
        let ir = build_ir(&f).expect("build_ir ok");
        assert_eq!(ir.graph_id, "g");
        assert_eq!(ir.nodes.len(), 3);
        assert_eq!(ir.edges.len(), 2);
        assert!(ir.edges.iter().all(|e| e.kind == EdgeKind::Structural));
    }

    #[test]
    fn ir_distinguishes_explicit_edges() {
        let f = Parser::parse_file(
            "<Graph id=\"g\"><A id=\"a\" /><B id=\"b\" /><edge source=\"a\" target=\"b\" ttl={5} /></Graph>",
        )
        .expect("ok");
        let ir = build_ir(&f).expect("build_ir ok");
        let explicit: Vec<_> = ir
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Explicit)
            .collect();
        assert_eq!(explicit.len(), 1);
        assert_eq!(explicit[0].source, "a");
        assert_eq!(explicit[0].target, "b");
        assert!(explicit[0].attrs.contains_key("ttl"));
    }

    #[test]
    fn ir_node_attrs_inject_component_and_depth() {
        let f = Parser::parse_file("<Graph id=\"g\"><Foo id=\"f\" v={1} /></Graph>").expect("ok");
        let ir = build_ir(&f).expect("build_ir ok");
        let n = &ir.nodes[0];
        assert_eq!(
            n.attrs.get("component"),
            Some(&JsonValue::String("Foo".to_string()))
        );
        assert_eq!(n.attrs.get("_depth"), Some(&JsonValue::Number(0.into())));
        assert_eq!(n.attrs.get("v"), Some(&JsonValue::Number(1.into())));
        assert!(n.attrs.contains_key("_parentPath"));
    }

    #[test]
    fn ir_multiparent_creates_one_node_two_edges() {
        // Same node id appears twice (multi-parent pattern).
        let f = Parser::parse_file(
            "<Graph id=\"g\"><A id=\"a\"><Shared id=\"s\" /></A><B id=\"b\"><Shared id=\"s\" /></B></Graph>",
        )
        .expect("ok");
        let ir = build_ir(&f).expect("build_ir ok");
        // 3 nodes: a, s, b — `s` is created once.
        assert_eq!(ir.nodes.len(), 3);
        let sources: Vec<_> = ir.edges.iter().map(|e| e.source.as_str()).collect();
        assert!(sources.contains(&"a"));
        assert!(sources.contains(&"b"));
    }

    #[test]
    fn ir_preserves_source_locations() {
        let src = "<Graph id=\"g\">\n  <A id=\"a\" />\n</Graph>";
        let f = Parser::parse_file(src).expect("ok");
        let ir = build_ir(&f).expect("build_ir ok");
        assert_eq!(ir.graph_loc.line, 1);
        assert_eq!(ir.nodes[0].source_loc.line, 2);
    }

    #[test]
    fn ir_to_json_round_trips_basic_shape() {
        let ir = parse_to_ir("<Graph id=\"g\"><A id=\"a\"><B id=\"b\" /></A></Graph>").expect("ok");
        let json = ir_to_json(&ir);
        let s = serde_json::to_string(&json).expect("serialise");
        assert!(s.contains("\"graphId\":\"g\""));
        assert!(s.contains("\"id\":\"a\""));
        assert!(s.contains("\"id\":\"b\""));
        assert!(s.contains("\"kind\":\"structural\""));
    }

    #[test]
    fn ir_includes_definitions_and_imports() {
        let src = r#"<import src="./a.grx" />
<Foo>
const x = 1;
</Foo>
<Graph id="g"><Foo id="f" /></Graph>"#;
        let ir = parse_to_ir(src).expect("ok");
        assert_eq!(ir.imports, vec!["./a.grx"]);
        assert_eq!(ir.definitions.len(), 1);
        assert_eq!(ir.definitions[0].component, "Foo");
        assert!(ir.definitions[0].code.contains("const x = 1"));
        assert_eq!(ir.definitions[0].lang, ComponentLang::Js);
    }

    /// **M15 Phase 10 wire-substrate gate.** `GrxIr` plus every
    /// nested struct gained `Serialize` / `Deserialize` derives
    /// so the dist crate can ship origin-authoritative IR over
    /// the wire. The inner encoding is `serde_json` — consistent
    /// with how `Message.payload` is `serde_json::to_vec` bytes
    /// inside a postcard outer envelope. Postcard itself can't
    /// serialise `serde_json::Value` (which `AttrMap` uses) —
    /// hitting `WontImplement` on the dynamic `Number` variant.
    /// Pure-postcard would require migrating `AttrMap` away from
    /// `serde_json::Value`, which is a substrate-wide refactor;
    /// JSON encoding for the inner payload + postcard for the
    /// outer wire envelope is the cleaner split.
    #[test]
    fn grx_ir_json_round_trips_for_phase10_wire() {
        let src = r#"<Logger>
const tag = "ok";
</Logger>
<Sensor lang="python">
def init():
    return {'accepts': []}
</Sensor>
<Graph id="g">
  <Logger id="l" />
  <Sensor id="s" />
  <edge source="s" target="l" ttl={3} />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse");
        let bytes = serde_json::to_vec(&ir).expect("encode");
        let decoded: GrxIr = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(decoded, ir, "GrxIr should round-trip through serde_json");
        // Substrate sanity: ensure structurally interesting fields
        // survived (edges, definitions with code, language tags).
        assert_eq!(decoded.edges.len(), 1);
        assert_eq!(decoded.definitions.len(), 2);
        assert!(decoded
            .definitions
            .iter()
            .any(|d| d.lang == ComponentLang::Python));
    }

    // -- PCBPnR Phase 5-D §2.4 reconciliation regression tests ---------
    // Multi-parent (duplicate id) is the load-bearing pattern at canonical
    // fixtures (counter-network.grx, multi-file/main.grx) per TS-documented
    // design (compiler.ts:6,98). These tests pin the reconciled rule that
    // catches the latent silent-attrs-drop footgun without breaking the
    // attribute-free re-mention convention.

    #[test]
    fn multiparent_attr_free_reference_with_first_occurrence_attrs_is_valid() {
        // First occurrence carries attrs; second occurrence is id-only —
        // the canonical counter-network.grx shape. Must remain valid.
        let f = Parser::parse_file(
            "<Graph id=\"g\">\
               <Agg id=\"a\"><Counter id=\"c\" initialCount={0} label=\"Alpha\" /></Agg>\
               <Logger id=\"l\"><Counter id=\"c\" /></Logger>\
             </Graph>",
        )
        .expect("parse ok");
        let ir = build_ir(&f).expect("build_ir ok");
        // 4 nodes: a, c, l. Wait — that's 3. Let me recount: Agg, Counter, Logger.
        assert_eq!(ir.nodes.len(), 3, "Counter `c` should be created once");
        // Counter's attrs come from the FIRST occurrence (initialCount=0, label="Alpha").
        let counter = ir
            .nodes
            .iter()
            .find(|n| n.id == "c")
            .expect("counter present");
        assert_eq!(
            counter.attrs.get("initialCount"),
            Some(&JsonValue::Number(0.into()))
        );
        assert_eq!(
            counter.attrs.get("label"),
            Some(&JsonValue::String("Alpha".to_string()))
        );
        // Structural edges: Agg→Counter and Logger→Counter (multi-parent).
        let sources: Vec<_> = ir.edges.iter().map(|e| e.source.as_str()).collect();
        assert!(sources.contains(&"a"), "edge from Agg expected");
        assert!(sources.contains(&"l"), "edge from Logger expected");
    }

    #[test]
    fn multiparent_with_attrs_on_second_occurrence_errors() {
        // Second occurrence carries divergent attrs — was silently
        // dropped in the pre-reconciliation behavior. Now a loud error.
        let f = Parser::parse_file(
            "<Graph id=\"g\">\
               <Agg id=\"a\"><Counter id=\"c\" initialCount={0} /></Agg>\
               <Logger id=\"l\"><Counter id=\"c\" initialCount={99} /></Logger>\
             </Graph>",
        )
        .expect("parse ok");
        let err = build_ir(&f).expect_err("collision must error");
        match err {
            crate::error::GrxParseError::IdConflictWithAttrs {
                id,
                first_loc,
                second_loc,
            } => {
                assert_eq!(id, "c");
                assert_ne!(first_loc, second_loc, "both locs must be distinct");
                assert!(
                    first_loc.line <= second_loc.line,
                    "first_loc should precede second_loc"
                );
            }
            other => panic!("expected IdConflictWithAttrs, got {other:?}"),
        }
    }

    #[test]
    fn multiparent_with_different_component_tag_errors() {
        // Same id, different component tag — was silently treated as
        // multi-parent using first component's definition. Now a loud
        // error.
        let f = Parser::parse_file(
            "<Graph id=\"g\">\
               <Agg id=\"a\"><Counter id=\"x\" /></Agg>\
               <Logger id=\"l\"><Logger id=\"x\" /></Logger>\
             </Graph>",
        )
        .expect("parse ok");
        let err = build_ir(&f).expect_err("component mismatch must error");
        match err {
            crate::error::GrxParseError::IdComponentMismatch {
                id,
                first_component,
                second_component,
                ..
            } => {
                assert_eq!(id, "x");
                assert_eq!(first_component, "Counter");
                assert_eq!(second_component, "Logger");
            }
            other => panic!("expected IdComponentMismatch, got {other:?}"),
        }
    }

    // -- PCBPnR Phase 5-D §2.3 splice acceptance tests ----------------

    #[test]
    fn splice_resolves_in_file_subgraph_with_dot_prefix() {
        // Single-file splice: a <Subgraph> defined in the same file
        // is spliced via <Subsystem ref="<bare-name>" id="<local>"/>.
        let src = r#"<Subgraph name="RfFrontEnd">
  <Component id="ANT1" footprint="ChipAnt" />
  <Component id="L1" footprint="0402" />
</Subgraph>
<Graph id="board">
  <Subsystem ref="RfFrontEnd" id="rf" />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse + build ir");
        // Spliced placeholder + the two inner components.
        let ids: Vec<&str> = ir.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"rf"), "splice boundary materializes: {ids:?}");
        assert!(ids.contains(&"rf.ANT1"), "inner id prefixed: {ids:?}");
        assert!(ids.contains(&"rf.L1"), "inner id prefixed: {ids:?}");
        // Edge from splice boundary to each inner component.
        let from_rf: Vec<&str> = ir
            .edges
            .iter()
            .filter(|e| e.source == "rf")
            .map(|e| e.target.as_str())
            .collect();
        assert!(from_rf.contains(&"rf.ANT1"));
        assert!(from_rf.contains(&"rf.L1"));
    }

    #[test]
    fn splice_missing_ref_errors() {
        let src = r#"<Subgraph name="X"><Component id="A" /></Subgraph>
<Graph id="g">
  <Subsystem id="x" />
</Graph>"#;
        let err = parse_to_ir(src).expect_err("missing ref must error");
        assert!(
            matches!(err, GrxParseError::SubsystemMissingRef { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn splice_unknown_ref_errors() {
        let src = r#"<Graph id="g">
  <Subsystem ref="NoSuchThing" id="x" />
</Graph>"#;
        let err = parse_to_ir(src).expect_err("unknown ref must error");
        match err {
            GrxParseError::SubsystemUnknownRef { qualified_name, .. } => {
                assert_eq!(qualified_name, "NoSuchThing");
            }
            other => panic!("expected SubsystemUnknownRef, got {other:?}"),
        }
    }

    #[test]
    fn splice_malformed_ref_with_empty_alias_errors() {
        let src = r#"<Graph id="g">
  <Subsystem ref=":Bad" id="x" />
</Graph>"#;
        let err = parse_to_ir(src).expect_err("empty alias half must error");
        assert!(matches!(err, GrxParseError::SubsystemMalformedRef { .. }));
    }

    #[test]
    fn splice_malformed_ref_with_empty_name_errors() {
        let src = r#"<Graph id="g">
  <Subsystem ref="rf:" id="x" />
</Graph>"#;
        let err = parse_to_ir(src).expect_err("empty name half must error");
        assert!(matches!(err, GrxParseError::SubsystemMalformedRef { .. }));
    }

    #[test]
    fn splice_nested_subsystem_resolves() {
        // A subgraph that itself splices another subgraph. Result IDs
        // are dot-chained: `outer.inner.ANT1`.
        let src = r#"<Subgraph name="Inner">
  <Component id="ANT1" />
</Subgraph>
<Subgraph name="Outer">
  <Subsystem ref="Inner" id="inner" />
</Subgraph>
<Graph id="g">
  <Subsystem ref="Outer" id="outer" />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse + build ir");
        let ids: Vec<&str> = ir.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"outer"));
        assert!(ids.contains(&"outer.inner"));
        assert!(ids.contains(&"outer.inner.ANT1"), "got: {ids:?}");
    }

    #[test]
    fn splice_cycle_detected() {
        let src = r#"<Subgraph name="A">
  <Subsystem ref="B" id="b1" />
</Subgraph>
<Subgraph name="B">
  <Subsystem ref="A" id="a1" />
</Subgraph>
<Graph id="g">
  <Subsystem ref="A" id="root" />
</Graph>"#;
        let err = parse_to_ir(src).expect_err("cycle must be detected");
        match err {
            GrxParseError::SpliceCycle { chain, .. } => {
                assert!(chain.contains('A'), "chain should name A: {chain}");
                assert!(chain.contains('B'), "chain should name B: {chain}");
            }
            other => panic!("expected SpliceCycle, got {other:?}"),
        }
    }

    #[test]
    fn splice_preserves_placement_attrs() {
        // PCB use case: `bbox`-shaped placement attrs on the splice
        // boundary must survive into IR so PCB consumers can read them
        // from outer scope. The splice doesn't drop them — they ride
        // through `ref`-stripped placeholder attrs.
        let src = r#"<Subgraph name="X"><Component id="A" /></Subgraph>
<Graph id="g">
  <Subsystem ref="X" id="r" bbox={{x:30,y:5,w:12,h:10}} layers="F.Cu,In1.Cu" />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse + build ir");
        let r = ir.nodes.iter().find(|n| n.id == "r").expect("r present");
        assert_eq!(r.component, "Subsystem");
        assert!(r.attrs.contains_key("bbox"));
        assert_eq!(
            r.attrs.get("layers"),
            Some(&JsonValue::String("F.Cu,In1.Cu".to_string()))
        );
        // `ref` is consumed by the splice resolver; it should NOT
        // appear on the placeholder.
        assert!(!r.attrs.contains_key("ref"));
    }

    #[test]
    fn splice_explicit_edges_prefix_endpoints() {
        // <edge>s inside a spliced subgraph must reach the prefixed
        // inner IDs, not the bare inner IDs (which don't exist in the
        // outer namespace).
        let src = r#"<Subgraph name="Chain">
  <Component id="A" />
  <Component id="B" />
  <edge source="A" target="B" ttl={3} />
</Subgraph>
<Graph id="g">
  <Subsystem ref="Chain" id="c" />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse + build ir");
        let explicit: Vec<_> = ir
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Explicit)
            .collect();
        assert_eq!(explicit.len(), 1);
        assert_eq!(explicit[0].source, "c.A");
        assert_eq!(explicit[0].target, "c.B");
    }

    // -- PCBPnR Phase 5-D §3 Part 2 acceptance: CSS-shape attribute
    // -- desugaring end-to-end through parse → build_ir.

    #[test]
    fn part2_css_style_desugars_into_ir_attrs() {
        let src = r#"<Graph id="board">
  <Component id="ANT1" footprint="ChipAnt"
    style="position:absolute; left:30mm; top:5mm; rotate:90deg; layer:F.Cu" />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse + ir ok");
        let ant = ir
            .nodes
            .iter()
            .find(|n| n.id == "ANT1")
            .expect("ant present");
        // The style key is consumed; the desugared form appears.
        assert!(!ant.attrs.contains_key("style"), "style key consumed");
        let at = ant.attrs.get("at").expect("at present");
        assert_eq!(at["x"], 30.0);
        assert_eq!(at["y"], 5.0);
        assert_eq!(at["rot"], 90.0);
        assert_eq!(
            ant.attrs.get("layer"),
            Some(&JsonValue::String("F.Cu".to_string()))
        );
        // Source-level `footprint` attribute rides through unchanged.
        assert_eq!(
            ant.attrs.get("footprint"),
            Some(&JsonValue::String("ChipAnt".to_string()))
        );
    }

    #[test]
    fn part2_pad_clip_path_round_rect_round_trip() {
        let src = r#"<Graph id="board">
  <Pad id="p1" net="GND"
    style="position:absolute; left:0mm; top:0mm; width:0.5mm; height:0.5mm; clip-path: inset(0 0 0 0 round 0.05mm)" />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse + ir ok");
        let p = ir.nodes.iter().find(|n| n.id == "p1").expect("pad present");
        let size = p.attrs.get("size").expect("size present");
        assert_eq!(size["w"], 0.5);
        assert_eq!(size["h"], 0.5);
        let geom = p.attrs.get("geometry").expect("geometry present");
        assert_eq!(geom["kind"], "round_rect");
        assert_eq!(geom["w"], 0.5);
        assert_eq!(geom["h"], 0.5);
        assert_eq!(geom["r"], 0.05);
    }

    #[test]
    fn part2_subsystem_style_to_bbox() {
        let src = r#"<Subgraph name="X"><Component id="A" /></Subgraph>
<Graph id="board">
  <Subsystem ref="X" id="rf"
    style="position:absolute; left:30mm; top:5mm; width:12mm; height:10mm; layer-band:F.Cu,In1.Cu" />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse + ir ok");
        let rf = ir.nodes.iter().find(|n| n.id == "rf").expect("rf present");
        let bbox = rf.attrs.get("bbox").expect("bbox present");
        assert_eq!(bbox["x"], 30.0);
        assert_eq!(bbox["y"], 5.0);
        assert_eq!(bbox["w"], 12.0);
        assert_eq!(bbox["h"], 10.0);
        assert_eq!(
            rf.attrs.get("layers"),
            Some(&JsonValue::String("F.Cu,In1.Cu".to_string()))
        );
        // Inner component still spliced under rf.
        assert!(ir.nodes.iter().any(|n| n.id == "rf.A"));
    }

    #[test]
    fn part2_via_drill_and_diameter_to_dim_objects() {
        let src = r#"<Graph id="board">
  <Via id="v1"
    style="position:absolute; left:2mm; top:2mm; clip-path: circle(0.3mm at 0mm 0mm); from-layer:F.Cu; to-layer:In1.Cu; drill:0.3mm; diameter:0.6mm" />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse + ir ok");
        let v = ir.nodes.iter().find(|n| n.id == "v1").expect("v1 present");
        assert_eq!(v.attrs["drill"]["d"], 0.3);
        assert_eq!(v.attrs["diameter"]["d"], 0.6);
        assert_eq!(v.attrs["from-layer"], "F.Cu");
        assert_eq!(v.attrs["to-layer"], "In1.Cu");
        let geom = &v.attrs["geometry"];
        assert_eq!(geom["kind"], "circle");
        assert_eq!(geom["r"], 0.3);
    }

    #[test]
    fn part2_keepout_polygon_geometry() {
        let src = r#"<Graph id="board">
  <Keepout id="k1"
    style="position:absolute; left:0mm; top:-1mm; clip-path: polygon(0mm 0mm, 14mm 0mm, 14mm 5mm, 0mm 5mm); layer-band:In1.Cu,In2.Cu; scope:net-class(ground)" />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse + ir ok");
        let k = ir.nodes.iter().find(|n| n.id == "k1").expect("k1 present");
        let geom = &k.attrs["geometry"];
        assert_eq!(geom["kind"], "polygon");
        assert_eq!(geom["points"].as_array().unwrap().len(), 4);
        assert_eq!(k.attrs["scope"], "net-class(ground)");
    }

    #[test]
    fn part2_net_impedance_and_pin_layer_strict() {
        let src = r#"<Graph id="board">
  <Net id="ANT_FEED"
    style="impedance:50; tolerance:10; preferred-layer:F.Cu; pin-layer:strict" />
</Graph>"#;
        let ir = parse_to_ir(src).expect("parse + ir ok");
        let n = ir
            .nodes
            .iter()
            .find(|n| n.id == "ANT_FEED")
            .expect("net present");
        assert_eq!(n.attrs["impedance"]["z0"], 50.0);
        assert_eq!(n.attrs["impedance"]["tol"], 10.0);
        assert_eq!(n.attrs["preferred-layer"], "F.Cu");
        assert_eq!(n.attrs["layer-strict"], true);
    }

    #[test]
    fn part2_bad_unit_errors_with_loc() {
        // Bad unit on `left:` surfaces as a Parse error with the
        // node's source location attached. (`pt` is not on the
        // accepted list mm/cm/in/px — see m115-css-units; `px` was
        // historically rejected but now parses to a CSS reference px.)
        let src = "<Graph id=\"g\">\n  <Component id=\"x\" style=\"position:absolute; left:30pt\" />\n</Graph>";
        let err = parse_to_ir(src).expect_err("must error");
        // Error type is GrxParseError::Parse with the style error
        // message embedded; loc points at the component.
        let loc = err.source_loc().expect("loc present");
        assert_eq!(loc.line, 2);
    }

    #[test]
    fn part2_unknown_property_surfaces_in_error_message() {
        let src = "<Graph id=\"g\">\n  <Component id=\"x\" style=\"imprdance:50\" />\n</Graph>";
        let err = parse_to_ir(src).expect_err("must error");
        let msg = err.to_string();
        assert!(msg.contains("imprdance"), "raw prop name in error: {msg}");
        assert!(msg.contains("impedance"), "suggestion in error: {msg}");
    }

    #[test]
    fn part2_both_forms_conflict_errors() {
        let src = r#"<Graph id="g">
  <Component id="x" footprint="0402" layer="F.Cu" style="layer:B.Cu" />
</Graph>"#;
        let err = parse_to_ir(src).expect_err("must conflict");
        let msg = err.to_string();
        assert!(msg.contains("layer"), "conflict cites layer: {msg}");
    }

    #[test]
    fn ir_carries_lang_python_for_python_definitions() {
        // M13-phase3a: lang flows from AST → IR. Phase 3+ (PyRuntime)
        // dispatches definitions to the right engine based on this
        // tag.
        let src = "<Sensor lang=\"python\">\ndef init():\n    return {'accepts': ['poll']}\n</Sensor>\n<Graph id=\"g\"><Sensor id=\"s\" /></Graph>";
        let ir = parse_to_ir(src).expect("ok");
        assert_eq!(ir.definitions.len(), 1);
        assert_eq!(ir.definitions[0].lang, ComponentLang::Python);
        assert!(ir.definitions[0].code.contains("def init"));
        // JSON form carries the lang tag so `grx parse --json` shows
        // language assignment for tooling / debugging.
        let json = ir_to_json(&ir);
        let s = serde_json::to_string(&json).expect("serialise");
        assert!(
            s.contains("\"lang\":\"python\""),
            "expected lang in json: {s}"
        );
    }
}
