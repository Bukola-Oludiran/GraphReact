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

use crate::ast::{AttrMap, ComponentLang, GrxFile, GrxNode};
use crate::error::SourceLoc;

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
    /// Component-body code blocks (Phase-7 `<ComponentName>...code...</ComponentName>`).
    pub definitions: Vec<IrDefinition>,
    /// Imported file paths in declaration order (already resolved by
    /// the multi-file resolver, if any).
    pub imports: Vec<String>,
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
pub fn build_ir(file: &GrxFile) -> GrxIr {
    let graph = file.graph.as_ref();
    let graph_id = graph.map(|g| g.id.clone()).unwrap_or_default();
    let graph_loc = graph.map(|g| g.loc).unwrap_or_default();

    let mut nodes: Vec<IrNode> = Vec::new();
    let mut edges: Vec<IrEdge> = Vec::new();
    let mut seen_ids: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();

    if let Some(g) = graph {
        process_children(&g.children, None, 0, &[], &mut nodes, &mut edges, &mut seen_ids);
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

    GrxIr {
        graph_id,
        graph_loc,
        nodes,
        edges,
        definitions,
        imports,
    }
}

/// Convenience: parse + build IR in one step.
pub fn parse_to_ir(input: &str) -> crate::GrxResult<GrxIr> {
    let file = crate::parser::Parser::parse_file(input)?;
    Ok(build_ir(&file))
}

fn process_children(
    children: &[GrxNode],
    parent_id: Option<&str>,
    depth: usize,
    parent_path: &[String],
    nodes: &mut Vec<IrNode>,
    edges: &mut Vec<IrEdge>,
    seen_ids: &mut rustc_hash::FxHashSet<String>,
) {
    for child in children {
        match child {
            GrxNode::Node(node) => {
                let mut node_path = parent_path.to_vec();
                node_path.push(node.id.clone());

                if !seen_ids.contains(&node.id) {
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
                    for (k, v) in &node.attrs {
                        full_attrs.insert(k.clone(), v.clone());
                    }
                    nodes.push(IrNode {
                        id: node.id.clone(),
                        component: node.component.clone(),
                        attrs: full_attrs,
                        depth,
                        parent_path: node_path.clone(),
                        source_loc: node.loc,
                    });
                    seen_ids.insert(node.id.clone());
                }

                if let Some(parent) = parent_id {
                    edges.push(IrEdge {
                        source: parent.to_string(),
                        target: node.id.clone(),
                        attrs: AttrMap::new(),
                        source_loc: node.loc,
                        kind: EdgeKind::Structural,
                    });
                }

                process_children(
                    &node.children,
                    Some(&node.id),
                    depth + 1,
                    &node_path,
                    nodes,
                    edges,
                    seen_ids,
                );
            }
            GrxNode::Edge(edge) => {
                edges.push(IrEdge {
                    source: edge.source.clone(),
                    target: edge.target.clone(),
                    attrs: edge.attrs.clone(),
                    source_loc: edge.loc,
                    kind: EdgeKind::Explicit,
                });
            }
        }
    }
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
                    n.parent_path.iter().cloned().map(JsonValue::String).collect(),
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
    use crate::parser::Parser;

    #[test]
    fn ir_captures_nesting_as_structural_edges() {
        let f = Parser::parse_file(
            "<Graph id=\"g\"><A id=\"a\"><B id=\"b\"><C id=\"c\" /></B></A></Graph>",
        )
        .expect("ok");
        let ir = build_ir(&f);
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
        let ir = build_ir(&f);
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
        let ir = build_ir(&f);
        let n = &ir.nodes[0];
        assert_eq!(n.attrs.get("component"), Some(&JsonValue::String("Foo".to_string())));
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
        let ir = build_ir(&f);
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
        let ir = build_ir(&f);
        assert_eq!(ir.graph_loc.line, 1);
        assert_eq!(ir.nodes[0].source_loc.line, 2);
    }

    #[test]
    fn ir_to_json_round_trips_basic_shape() {
        let ir = parse_to_ir(
            "<Graph id=\"g\"><A id=\"a\"><B id=\"b\" /></A></Graph>",
        )
        .expect("ok");
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
        assert!(s.contains("\"lang\":\"python\""), "expected lang in json: {s}");
    }
}
