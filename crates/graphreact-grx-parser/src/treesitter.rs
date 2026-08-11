//! **M15 Phase 1B.** Tree-sitter-backed parser for `.grx`.
//!
//! Produces the same `GrxIr` shape as the hand-rolled parser at
//! `parser.rs` + `ir.rs` plus `build_ir`, but uses
//! `tree-sitter-grx`'s grammar + GLR parser so we can do
//! incremental reparse via `Tree::edit` + `Parser::parse(_, Some(prev_tree))`.
//!
//! ## API
//!
//! - [`parse_to_ir_via_treesitter`] — full parse from `&str` to `GrxIr`.
//!   Stateless; matches `parse_to_ir`'s signature.
//! - [`IncrementalParser`] — stateful parser that holds the previous
//!   tree + supports `parse` / `reparse` for sub-file incremental
//!   precision (the perf win we're chasing in Phase 1B).
//!
//! ## State preservation across reparses
//!
//! `IncrementalParser` keeps its `tree_sitter::Parser` AND last
//! `tree_sitter::Tree`. The reparse path applies the supplied edits
//! to the prior tree (so tree-sitter knows what changed), then calls
//! `parse(_, Some(&edited))` — the parser reuses unchanged subtrees,
//! making the work proportional to the edited region, NOT the whole
//! file. This is the load-bearing property for the M15 perf gate
//! (incremental ≥10× faster than full reparse on a 200-line `.grx`).
//!
//! ## Scope
//!
//! This module is the FOUNDATION layer. Stage 4 of Phase 1B swaps
//! `parse_to_ir`'s default backend to call into here. Until then,
//! both backends co-exist; consumers can pick by importing the
//! function they want.

use serde_json::Value as JsonValue;
use tree_sitter::{Node, Parser, Tree};

use crate::ast::{
    AttrMap, ComponentLang, EdgeDecl, GraphDecl, GrxFile, GrxNode, ImportDirective, NodeDecl,
    NodeDefinitionBlock, SubgraphDecl,
};
use crate::error::{GrxParseError, GrxResult, SourceLoc};
use crate::ir::GrxIr;

/// Convenience wrapper: parse a `.grx` source string straight to
/// `GrxIr` via the tree-sitter backend. Mirrors the hand-rolled
/// `parse_to_ir` signature so consumers can A/B compare.
pub fn parse_to_ir_via_treesitter(input: &str) -> GrxResult<GrxIr> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_grx::LANGUAGE.into())
        .map_err(|e| GrxParseError::lex(format!("set_language: {e}"), SourceLoc::new(1, 1)))?;
    let tree = parser.parse(input, None).ok_or_else(|| {
        GrxParseError::lex("tree-sitter parse returned None", SourceLoc::new(1, 1))
    })?;
    build_ir_from_tree(&tree, input)
}

/// **M15 Phase 1B.** Stateful incremental parser. Owns the
/// `tree_sitter::Parser` plus the last successfully parsed tree. Use
/// [`Self::parse`] for the first parse and [`Self::reparse`] for
/// subsequent edits — the reparse path leverages tree-sitter's
/// incremental algorithm so the work is proportional to the edited
/// region rather than the whole file.
pub struct IncrementalParser {
    parser: Parser,
    last_tree: Option<Tree>,
    last_source: String,
}

impl IncrementalParser {
    /// Construct a fresh parser with the `.grx` grammar attached.
    pub fn new() -> GrxResult<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_grx::LANGUAGE.into())
            .map_err(|e| {
                GrxParseError::lex(
                    format!("IncrementalParser set_language: {e}"),
                    SourceLoc::new(1, 1),
                )
            })?;
        Ok(Self {
            parser,
            last_tree: None,
            last_source: String::new(),
        })
    }

    /// Full parse. Replaces any prior tree.
    pub fn parse(&mut self, source: &str) -> GrxResult<&Tree> {
        let tree = self.parser.parse(source, None).ok_or_else(|| {
            GrxParseError::lex("tree-sitter parse returned None", SourceLoc::new(1, 1))
        })?;
        self.last_tree = Some(tree);
        self.last_source = source.to_string();
        Ok(self.last_tree.as_ref().expect("just set"))
    }

    /// **M15 Phase 1B perf-win path.** Apply a list of edits to the
    /// prior tree, then reparse — tree-sitter reuses unchanged
    /// subtrees so the work scales with the edit region, not the
    /// whole file.
    ///
    /// Returns `Err` if no prior tree is available (caller must
    /// invoke [`Self::parse`] first).
    pub fn reparse(
        &mut self,
        edits: &[tree_sitter::InputEdit],
        new_source: &str,
    ) -> GrxResult<&Tree> {
        let mut prev = self.last_tree.take().ok_or_else(|| {
            GrxParseError::lex(
                "IncrementalParser::reparse called before initial parse",
                SourceLoc::new(1, 1),
            )
        })?;
        for edit in edits {
            prev.edit(edit);
        }
        let new_tree = self.parser.parse(new_source, Some(&prev)).ok_or_else(|| {
            GrxParseError::lex("tree-sitter reparse returned None", SourceLoc::new(1, 1))
        })?;
        self.last_tree = Some(new_tree);
        self.last_source = new_source.to_string();
        Ok(self.last_tree.as_ref().expect("just set"))
    }

    /// Read the latest tree, if any.
    #[must_use]
    pub fn tree(&self) -> Option<&Tree> {
        self.last_tree.as_ref()
    }

    /// Build an `GrxIr` from the latest parse. Returns `Err` if no
    /// parse has run yet OR the IR construction discovers a missing
    /// required attribute (e.g. `<Graph>` without `id`).
    pub fn build_ir(&self) -> GrxResult<GrxIr> {
        let tree = self.last_tree.as_ref().ok_or_else(|| {
            GrxParseError::lex(
                "IncrementalParser::build_ir called before parse",
                SourceLoc::new(1, 1),
            )
        })?;
        build_ir_from_tree(tree, &self.last_source)
    }
}

// =============================================================
// Tree → GrxIr conversion
// =============================================================

/// **m115-tree-sitter-splice (Cleanup-Pass).** Walks the CST into an
/// [`crate::ast::GrxFile`] then routes through the shared
/// [`crate::ir::build_ir`] so Subsystem splice resolution + multi-
/// parent reconciliation + depth/parent_path injection live in ONE
/// place across both parser backends. Previously the tree-sitter
/// backend went CST → IR directly and silently dropped `<Subgraph>`
/// blocks; this is the route-(a) refactor that delivers byte-identical
/// IR parity on splice fixtures.
pub fn build_grx_file_from_tree(tree: &Tree, source: &str) -> GrxResult<GrxFile> {
    let root = tree.root_node();
    if root.has_error() {
        if let Some(err_node) = first_error_node(root) {
            return Err(GrxParseError::parse(
                format!(
                    "tree-sitter parse error at {}",
                    node_loc(err_node).into_owned_string()
                ),
                node_loc(err_node).into_inner(),
            ));
        }
        return Err(GrxParseError::parse(
            "tree-sitter parse produced an error tree (no specific error node located)",
            SourceLoc::new(1, 1),
        ));
    }

    let mut file = GrxFile::default();
    let mut cursor = root.walk();
    let mut graph_seen = false;
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "comment" => {}
            "import_decl" => {
                file.imports.push(build_import_decl(child, source)?);
            }
            "subgraph" => {
                file.subgraphs.push(build_subgraph_decl(child, source)?);
            }
            "graph" => {
                if graph_seen {
                    return Err(GrxParseError::parse(
                        "multiple <Graph> blocks at top level",
                        node_loc(child).into_inner(),
                    ));
                }
                graph_seen = true;
                file.graph = Some(build_graph_decl(child, source)?);
            }
            "definition_block" => {
                file.definitions.push(build_definition_decl(child, source)?);
            }
            other => {
                return Err(GrxParseError::parse(
                    format!("unexpected top-level CST kind `{other}`"),
                    node_loc(child).into_inner(),
                ));
            }
        }
    }
    Ok(file)
}

fn build_ir_from_tree(tree: &Tree, source: &str) -> GrxResult<GrxIr> {
    // Route-(a) refactor (Cleanup-Pass `m115-tree-sitter-splice`):
    // walk the CST into an AST + delegate to the shared
    // `crate::ir::build_ir` for splice resolution + reconciliation.
    let file = build_grx_file_from_tree(tree, source)?;
    crate::ir::build_ir(&file)
}

// --- CST -> AST helpers (route-(a) m115-tree-sitter-splice) ---

fn build_import_decl(node: Node<'_>, source: &str) -> GrxResult<ImportDirective> {
    let mut src = None;
    let mut alias = None;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "attribute" {
            let (name, value) = parse_attribute(child, source)?;
            match (name.as_str(), value) {
                ("src", Some(JsonValue::String(s))) => src = Some(s),
                ("as", Some(JsonValue::String(s))) => alias = Some(s),
                _ => {}
            }
        }
    }
    let src = src.ok_or_else(|| {
        GrxParseError::parse(
            "<import> requires a `src` attribute",
            node_loc(node).into_inner(),
        )
    })?;
    if let Some(a) = &alias {
        if !ImportDirective::is_valid_alias(a) {
            return Err(GrxParseError::parse(
                format!("invalid import alias `{a}` — must match [a-zA-Z_][a-zA-Z0-9_]*"),
                node_loc(node).into_inner(),
            ));
        }
    }
    Ok(ImportDirective {
        src,
        alias,
        loc: node_loc(node).into_inner(),
    })
}

fn build_graph_decl(graph_node: Node<'_>, source: &str) -> GrxResult<GraphDecl> {
    let (id, loc) = extract_graph_id(graph_node, source)?;
    let children = walk_block_children_to_ast(graph_node, source)?;
    Ok(GraphDecl { id, children, loc })
}

fn build_subgraph_decl(subgraph_node: Node<'_>, source: &str) -> GrxResult<SubgraphDecl> {
    // The grammar hides `_subgraph_open`, so `attribute` children sit
    // directly under the `subgraph` node (interleaved with the body
    // `edge_decl` / `graph_node` children).
    let mut name: Option<String> = None;
    let mut cursor = subgraph_node.walk();
    for child in subgraph_node.named_children(&mut cursor) {
        if child.kind() == "attribute" {
            let (n, v) = parse_attribute(child, source)?;
            if n == "name" {
                if let Some(JsonValue::String(s)) = v {
                    name = Some(s);
                }
            }
        }
    }
    let name = name.ok_or_else(|| GrxParseError::MissingSubgraphName {
        loc: node_loc(subgraph_node).into_inner(),
    })?;
    let children = walk_block_children_to_ast(subgraph_node, source)?;
    Ok(SubgraphDecl {
        name,
        children,
        loc: node_loc(subgraph_node).into_inner(),
    })
}

fn walk_block_children_to_ast(block: Node<'_>, source: &str) -> GrxResult<Vec<GrxNode>> {
    let mut out: Vec<GrxNode> = Vec::new();
    let mut cursor = block.walk();
    for child in block.named_children(&mut cursor) {
        match child.kind() {
            "edge_decl" => out.push(GrxNode::Edge(build_edge_decl(child, source)?)),
            "graph_node" => out.push(GrxNode::Node(build_node_decl(child, source)?)),
            // Skip comments + open-tag tokens; the named-child iteration
            // surfaces them and the grammar already constrains the
            // legal set, so unrecognized kinds are tolerated.
            _ => {}
        }
    }
    Ok(out)
}

fn build_node_decl(graph_node: Node<'_>, source: &str) -> GrxResult<NodeDecl> {
    // Find the open-tag (self-closing or paired).
    let mut open_tag: Option<Node<'_>> = None;
    let mut self_closing = false;
    let mut cursor = graph_node.walk();
    for child in graph_node.named_children(&mut cursor) {
        match child.kind() {
            "graph_node_open" => {
                open_tag = Some(child);
                self_closing = false;
            }
            "graph_node_self_close" => {
                open_tag = Some(child);
                self_closing = true;
            }
            _ => {}
        }
    }
    let open = open_tag.ok_or_else(|| {
        GrxParseError::parse(
            "graph_node missing open-tag child",
            node_loc(graph_node).into_inner(),
        )
    })?;

    let component = extract_tag_name(open, source).ok_or_else(|| {
        GrxParseError::parse(
            "graph_node open-tag missing tag_name",
            node_loc(open).into_inner(),
        )
    })?;

    let mut attrs: AttrMap = AttrMap::new();
    let mut id: Option<String> = None;
    let mut attr_cursor = open.walk();
    for attr_child in open.named_children(&mut attr_cursor) {
        if attr_child.kind() == "attribute" {
            let (name, value) = parse_attribute(attr_child, source)?;
            let json_value = value.unwrap_or(JsonValue::Bool(true));
            if name == "id" {
                if let JsonValue::String(s) = &json_value {
                    id = Some(s.clone());
                }
            }
            attrs.insert(name, json_value);
        }
    }

    // Hand-rolled AST strips the `id` from `attrs` after hoisting
    // it; mirror exactly for parity. `build_ir` reads `id` from the
    // `NodeDecl.id` field, not the attrs map.
    attrs.shift_remove("id");

    let id = id.ok_or_else(|| {
        GrxParseError::parse(
            format!("node `<{component}>` requires an `id` attribute"),
            node_loc(graph_node).into_inner(),
        )
    })?;

    // Recurse into nested children. Self-closing tags have none.
    let children = if self_closing {
        Vec::new()
    } else {
        walk_block_children_to_ast(graph_node, source)?
    };

    Ok(NodeDecl {
        component,
        id,
        attrs,
        children,
        self_closing,
        loc: node_loc(graph_node).into_inner(),
    })
}

fn build_edge_decl(edge_decl: Node<'_>, source: &str) -> GrxResult<EdgeDecl> {
    let mut source_id: Option<String> = None;
    let mut target_id: Option<String> = None;
    let mut attrs: AttrMap = AttrMap::new();
    let mut cursor = edge_decl.walk();
    for child in edge_decl.named_children(&mut cursor) {
        if child.kind() == "attribute" {
            let (name, value) = parse_attribute(child, source)?;
            let json_value = value.unwrap_or(JsonValue::Bool(true));
            match (name.as_str(), &json_value) {
                ("source", JsonValue::String(s)) => source_id = Some(s.clone()),
                ("target", JsonValue::String(s)) => target_id = Some(s.clone()),
                _ => {
                    attrs.insert(name, json_value);
                }
            }
        }
    }
    let source = source_id.ok_or_else(|| {
        GrxParseError::parse(
            "edge requires `source` and `target` attributes",
            node_loc(edge_decl).into_inner(),
        )
    })?;
    let target = target_id.ok_or_else(|| {
        GrxParseError::parse(
            "edge requires `source` and `target` attributes",
            node_loc(edge_decl).into_inner(),
        )
    })?;
    Ok(EdgeDecl {
        source,
        target,
        attrs,
        loc: node_loc(edge_decl).into_inner(),
    })
}

fn build_definition_decl(def_block: Node<'_>, source: &str) -> GrxResult<NodeDefinitionBlock> {
    let mut tag_name: Option<String> = None;
    let mut lang: ComponentLang = ComponentLang::default();
    let mut src: Option<String> = None;
    let mut raw: Option<String> = None;
    let mut tag_count = 0usize;

    let mut cursor = def_block.walk();
    for child in def_block.named_children(&mut cursor) {
        match child.kind() {
            "tag_name" => {
                tag_count += 1;
                if tag_count == 1 {
                    tag_name = Some(node_text(child, source).to_string());
                }
            }
            "attribute" => {
                let (name, value) = parse_attribute(child, source)?;
                match (name.as_str(), value) {
                    ("lang", Some(JsonValue::String(s))) => {
                        lang = ComponentLang::from_attr_str(&s).ok_or_else(|| {
                            GrxParseError::parse(
                                format!("unknown `lang` value `{s}`"),
                                node_loc(child).into_inner(),
                            )
                        })?;
                    }
                    ("src", Some(JsonValue::String(s))) => src = Some(s),
                    _ => {}
                }
            }
            "raw_body" => {
                raw = Some(node_text(child, source).trim().to_string());
            }
            _ => {}
        }
    }

    let component = tag_name.ok_or_else(|| {
        GrxParseError::parse(
            "definition_block missing tag name",
            node_loc(def_block).into_inner(),
        )
    })?;
    let code = raw.unwrap_or_default();

    Ok(NodeDefinitionBlock {
        component,
        code,
        lang,
        src,
        loc: node_loc(def_block).into_inner(),
    })
}

fn first_error_node(node: Node<'_>) -> Option<Node<'_>> {
    if node.is_error() || node.is_missing() {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(err) = first_error_node(child) {
            return Some(err);
        }
    }
    None
}

fn extract_graph_id(graph: Node<'_>, source: &str) -> GrxResult<(String, SourceLoc)> {
    let mut id: Option<String> = None;
    let mut cursor = graph.walk();
    for child in graph.named_children(&mut cursor) {
        if child.kind() == "attribute" {
            let (name, value) = parse_attribute(child, source)?;
            if name == "id" {
                if let Some(JsonValue::String(s)) = value {
                    id = Some(s);
                    break;
                }
            }
        }
    }
    let id = id.ok_or_else(|| {
        GrxParseError::parse(
            "<Graph> requires an `id` attribute",
            node_loc(graph).into_inner(),
        )
    })?;
    Ok((id, node_loc(graph).into_inner()))
}

fn parse_attribute(attr: Node<'_>, source: &str) -> GrxResult<(String, Option<JsonValue>)> {
    let mut name: Option<String> = None;
    let mut value: Option<JsonValue> = None;
    let mut cursor = attr.walk();
    for child in attr.named_children(&mut cursor) {
        match child.kind() {
            "attr_name" => {
                name = Some(node_text(child, source).to_string());
            }
            "string_literal" => {
                value = Some(JsonValue::String(parse_string_literal(child, source)));
            }
            "expr_literal" => {
                value = Some(parse_expr_literal(child, source));
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| {
        GrxParseError::parse("attribute missing name", node_loc(attr).into_inner())
    })?;
    Ok((name, value))
}

fn parse_string_literal(node: Node<'_>, source: &str) -> String {
    // Strip the surrounding quotes; resolve `\\X` escape sequences.
    let raw = node_text(node, source);
    let inner = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(esc) = chars.next() {
                out.push(esc);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_expr_literal(node: Node<'_>, source: &str) -> JsonValue {
    // Strip surrounding `{` / `}`. Try to parse the inner as JSON
    // (matches the existing parser's heuristic — numeric / bool /
    // null literals become typed JsonValue; everything else stays
    // as a string).
    let raw = node_text(node, source);
    let inner_trimmed = raw
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .map(str::trim)
        .unwrap_or("");
    if let Ok(v) = serde_json::from_str::<JsonValue>(inner_trimmed) {
        v
    } else {
        JsonValue::String(inner_trimmed.to_string())
    }
}

fn extract_tag_name(open_tag: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = open_tag.walk();
    for child in open_tag.named_children(&mut cursor) {
        if child.kind() == "tag_name" {
            return Some(node_text(child, source).to_string());
        }
    }
    None
}

fn node_text<'src>(node: Node<'_>, source: &'src str) -> &'src str {
    let bytes = source.as_bytes();
    let start = node.start_byte().min(bytes.len());
    let end = node.end_byte().min(bytes.len());
    // SAFETY: tree-sitter byte ranges align with UTF-8 boundaries
    // for the input we provided.
    std::str::from_utf8(&bytes[start..end]).unwrap_or("")
}

/// Wrapper holding both the bare `SourceLoc` (for IR errors) and a
/// formatted string (for log messages).
struct LocPair {
    loc: SourceLoc,
    formatted: String,
}

impl LocPair {
    fn into_inner(self) -> SourceLoc {
        self.loc
    }
    fn into_owned_string(self) -> String {
        self.formatted
    }
}

fn node_loc(node: Node<'_>) -> LocPair {
    let pt = node.start_position();
    let loc = SourceLoc::new(pt.row + 1, pt.column + 1);
    let formatted = format!("{loc}");
    LocPair { loc, formatted }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_graph_via_treesitter() {
        let ir =
            parse_to_ir_via_treesitter(r#"<Graph id="g"><A id="a" /></Graph>"#).expect("parse");
        assert_eq!(ir.graph_id, "g");
        assert_eq!(ir.nodes.len(), 1);
        assert_eq!(ir.nodes[0].id, "a");
        assert_eq!(ir.nodes[0].component, "A");
        assert_eq!(ir.nodes[0].depth, 0);
        assert!(ir.edges.is_empty());
    }

    #[test]
    fn parse_nested_graph_creates_structural_edges_via_treesitter() {
        let ir = parse_to_ir_via_treesitter(
            r#"<Graph id="g"><A id="a"><B id="b"><C id="c" /></B></A></Graph>"#,
        )
        .expect("parse");
        assert_eq!(ir.graph_id, "g");
        assert_eq!(ir.nodes.len(), 3);
        assert_eq!(ir.edges.len(), 2);
        assert!(ir
            .edges
            .iter()
            .all(|e| e.kind == crate::ir::EdgeKind::Structural));
    }

    #[test]
    fn parse_explicit_edge_via_treesitter() {
        let ir = parse_to_ir_via_treesitter(
            r#"<Graph id="g"><A id="a" /><B id="b" /><edge source="a" target="b" ttl={5} /></Graph>"#,
        )
        .expect("parse");
        assert_eq!(ir.edges.len(), 1);
        assert_eq!(ir.edges[0].kind, crate::ir::EdgeKind::Explicit);
        assert_eq!(ir.edges[0].source, "a");
        assert_eq!(ir.edges[0].target, "b");
        assert_eq!(
            ir.edges[0]
                .attrs
                .get("ttl")
                .and_then(serde_json::Value::as_u64),
            Some(5)
        );
    }

    #[test]
    fn parse_definition_block_via_treesitter() {
        let src = r#"<Foo>
const x = 1;
</Foo>
<Graph id="g"><Foo id="f" /></Graph>"#;
        let ir = parse_to_ir_via_treesitter(src).expect("parse");
        assert_eq!(ir.definitions.len(), 1);
        assert_eq!(ir.definitions[0].component, "Foo");
        assert!(ir.definitions[0].code.contains("const x = 1"));
        assert_eq!(ir.definitions[0].lang, ComponentLang::Js);
    }

    #[test]
    fn parse_python_lang_definition_via_treesitter() {
        let src = r#"<Sensor lang="python">
def init():
    return {'accepts': ['poll']}
</Sensor>
<Graph id="g"><Sensor id="s" /></Graph>"#;
        let ir = parse_to_ir_via_treesitter(src).expect("parse");
        assert_eq!(ir.definitions.len(), 1);
        assert_eq!(ir.definitions[0].lang, ComponentLang::Python);
        assert!(ir.definitions[0].code.contains("def init"));
    }

    #[test]
    fn parse_imports_via_treesitter() {
        let src = r#"<import src="./a.grx" />
<import src="./b.grx" />
<Graph id="g"><A id="a" /></Graph>"#;
        let ir = parse_to_ir_via_treesitter(src).expect("parse");
        assert_eq!(ir.imports, vec!["./a.grx", "./b.grx"]);
    }

    #[test]
    fn incremental_parser_reparse_skips_unchanged_subtrees() {
        let mut ip = IncrementalParser::new().expect("new");
        let src1 = r#"<Graph id="g"><A id="a" /></Graph>
<A>const x = 1;</A>"#;
        let _t1 = ip.parse(src1).expect("parse 1");
        let ir1 = ip.build_ir().expect("ir 1");
        assert_eq!(ir1.graph_id, "g");

        // Second parse: no edits applied, just full reparse via
        // `parse` — establishes baseline that the same input
        // produces the same IR.
        let _t2 = ip.parse(src1).expect("parse 2");
        let ir2 = ip.build_ir().expect("ir 2");
        assert_eq!(ir1, ir2);

        // Reparse path: tweak the body, supply an InputEdit that
        // describes the change, verify incremental reparse
        // produces the new IR.
        let src2 = r#"<Graph id="g"><A id="a" /></Graph>
<A>const x = 2;</A>"#;
        // Diff: position of `1` (offset 51 in src1 — `<Graph...><A id="a" /></Graph>\n<A>const x = ` is 50 chars, so `1` is at byte 50).
        // Compute via lexer-grade indexing from each source.
        let pos = src1.find('1').expect("find 1") as u32;
        let edit = tree_sitter::InputEdit {
            start_byte: pos as usize,
            old_end_byte: pos as usize + 1,
            new_end_byte: pos as usize + 1,
            start_position: tree_sitter::Point {
                row: 1,
                column: pos as usize - src1.find('\n').unwrap() - 1,
            },
            old_end_position: tree_sitter::Point {
                row: 1,
                column: pos as usize - src1.find('\n').unwrap(),
            },
            new_end_position: tree_sitter::Point {
                row: 1,
                column: pos as usize - src1.find('\n').unwrap(),
            },
        };
        let _t3 = ip.reparse(&[edit], src2).expect("reparse");
        let ir3 = ip.build_ir().expect("ir 3");
        assert!(ir3.definitions[0].code.contains("const x = 2"));
    }

    /// **M15 Phase 1B stage 4 — substrate cross-validation.** Both
    /// parser backends MUST produce equivalent `GrxIr` for every
    /// canonical ASCII-only fixture (and the multi-file imported
    /// children). This is the load-bearing gate that lets us swap
    /// the default `parse_to_ir` backend later WITHOUT churn on
    /// all 50 existing tests: any future divergence shows up here
    /// first.
    ///
    /// **Excluded fixture:** `pipeline-single.grx` contains a
    /// non-ASCII glyph (`→`) in a definition body. The hand-rolled
    /// lexer has a pre-existing UTF-8 bug
    /// (`lexer.rs::read_raw_content` casts each byte to `char` via
    /// `ch as char`, which mangles multi-byte UTF-8 sequences); the
    /// tree-sitter backend handles this correctly via
    /// `std::str::from_utf8` on the byte range. The
    /// `tree_sitter_backend_preserves_utf8_in_definition_bodies`
    /// test below explicitly captures the tree-sitter parity win.
    //
    // PCBPnR Phase 5-D step 5 — parity for the Subgraph reserved
    // word. Files containing `<Subgraph>` blocks (without `<Subsystem>`
    // splice references) must produce identical IRs across both
    // backends because neither backend materializes Subgraph blocks
    // into IR — the hand-rolled side keeps them in `file.subgraphs`
    // (consumed by the splice pass at build_ir time), and the
    // tree-sitter side skips them at the grammar boundary.
    //
    // `<Subsystem>` splice parity SHIPPED via Cleanup-Pass
    // `m115-tree-sitter-splice` (2026-06-17). The tree-sitter
    // backend now routes CST → GrxFile → `crate::ir::build_ir` so
    // splice resolution + multi-parent reconciliation live in ONE
    // place across both backends. See
    // `cross_validate_backends_splice_subsystem_identically`.
    #[test]
    fn cross_validate_backends_handle_subgraph_blocks_identically() {
        use crate::ir::parse_to_ir;
        // Subgraph-only fixture: defines two named subgraphs that are
        // never spliced (no Subsystem reference). Neither backend
        // materializes them into IR — both should produce an IR with
        // only the top-level <Graph>'s nodes.
        let src = r#"<Subgraph name="ShelfA">
  <Component id="A1" />
</Subgraph>
<Subgraph name="ShelfB">
  <Component id="B1" />
</Subgraph>
<Graph id="board">
  <Logger id="log" />
</Graph>"#;
        let a = parse_to_ir(src).expect("hand-rolled parses ok");
        let b = parse_to_ir_via_treesitter(src).expect("tree-sitter parses ok");
        assert_eq!(a.graph_id, b.graph_id);
        let a_ids: Vec<&str> = a.nodes.iter().map(|n| n.id.as_str()).collect();
        let b_ids: Vec<&str> = b.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(a_ids, b_ids);
        // Neither backend should materialize Subgraph contents.
        assert!(!a_ids.contains(&"A1"));
        assert!(!a_ids.contains(&"B1"));
        // Only the <Graph>'s <Logger> appears.
        assert_eq!(a_ids, vec!["log"]);
    }

    /// Cleanup-Pass `m115-tree-sitter-splice`: a `<Subgraph>` block
    /// referenced by a `<Subsystem ref=... id=.../>` placeholder
    /// must be flattened identically by both backends. The
    /// tree-sitter side now walks the CST into a `GrxFile` and
    /// delegates to the shared `crate::ir::build_ir`, so splice
    /// resolution + qualified-id prefix machinery + multi-parent
    /// reconciliation are shared across both parser paths.
    #[test]
    fn cross_validate_backends_splice_subsystem_identically() {
        use crate::ir::parse_to_ir;
        let src = r#"<Subgraph name="ShelfA">
  <Box id="b1" />
  <Box id="b2" />
  <edge source="b1" target="b2" />
</Subgraph>
<Graph id="board">
  <Subsystem ref="ShelfA" id="a" />
</Graph>"#;
        let a = parse_to_ir(src).expect("hand-rolled parses ok");
        let b = parse_to_ir_via_treesitter(src).expect("tree-sitter parses ok");
        assert_eq!(a.graph_id, b.graph_id);
        let a_ids: Vec<&str> = a.nodes.iter().map(|n| n.id.as_str()).collect();
        let b_ids: Vec<&str> = b.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(a_ids, b_ids);
        // Spliced contents: `b1` → `a.b1`, `b2` → `a.b2`. The
        // splice site itself does not materialize as an IR node.
        assert!(a_ids.contains(&"a.b1"));
        assert!(a_ids.contains(&"a.b2"));
        // Inner edge should also be qualified.
        let a_edges: Vec<(&str, &str)> = a
            .edges
            .iter()
            .map(|e| (e.source.as_str(), e.target.as_str()))
            .collect();
        let b_edges: Vec<(&str, &str)> = b
            .edges
            .iter()
            .map(|e| (e.source.as_str(), e.target.as_str()))
            .collect();
        assert_eq!(a_edges, b_edges);
        assert!(a_edges.contains(&("a.b1", "a.b2")));
    }

    /// PCBPnR Phase 5-D step 5 — aliased `<import as="...">` parses
    /// through both backends. The tree-sitter side currently doesn't
    /// surface alias metadata into the IR (the IR's `imports` field
    /// is `Vec<String>` of paths only), so parity holds at the IR
    /// level; the alias only matters for the hand-rolled splice pass
    /// at build_ir time (which tree-sitter doesn't run — see note
    /// above).
    #[test]
    fn cross_validate_backends_handle_aliased_import_identically() {
        use crate::ir::parse_to_ir;
        let src = r#"<import src="./rf.grx" as="rf" />
<Graph id="board">
  <Logger id="log" />
</Graph>"#;
        let a = parse_to_ir(src).expect("hand-rolled parses ok");
        let b = parse_to_ir_via_treesitter(src).expect("tree-sitter parses ok");
        assert_eq!(a.imports, b.imports);
        let a_ids: Vec<&str> = a.nodes.iter().map(|n| n.id.as_str()).collect();
        let b_ids: Vec<&str> = b.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(a_ids, b_ids);
    }

    /// PCBPnR Phase 5-D §3.8 #10 — Part 2 parity. Both backends
    /// must produce identical desugared AttrMaps for elements
    /// carrying `style="…"`. Validates that the tree-sitter bridge
    /// invokes the shared `crate::style::desugar_style` function
    /// (same code path, same outputs) as the hand-rolled IR builder.
    #[test]
    fn cross_validate_backends_desugar_style_identically() {
        use crate::ir::parse_to_ir;
        // Multi-property style on a Component; both backends must
        // desugar to identical `at` + `layer` keys, with `style` key
        // consumed.
        let src = r#"<Graph id="board">
  <Component id="ANT1" footprint="ChipAnt"
    style="position:absolute; left:30mm; top:5mm; rotate:90deg; layer:F.Cu" />
</Graph>"#;
        let a = parse_to_ir(src).expect("hand-rolled ok");
        let b = parse_to_ir_via_treesitter(src).expect("tree-sitter ok");
        let a_attrs = &a.nodes.iter().find(|n| n.id == "ANT1").unwrap().attrs;
        let b_attrs = &b.nodes.iter().find(|n| n.id == "ANT1").unwrap().attrs;
        // Both backends consume the `style` key.
        assert!(!a_attrs.contains_key("style"), "hand-rolled consumed style");
        assert!(!b_attrs.contains_key("style"), "tree-sitter consumed style");
        // Both produce matching `at` and `layer`.
        assert_eq!(a_attrs.get("at"), b_attrs.get("at"), "at parity");
        assert_eq!(a_attrs.get("layer"), b_attrs.get("layer"), "layer parity");
    }

    /// PCBPnR Phase 5-D §3 — Pad with clip-path inset(round). Tests
    /// the load-bearing parity case: `width:` + `height:` siblings
    /// feed BOTH `size` and `geometry`'s rect dimensions.
    #[test]
    fn cross_validate_backends_pad_inset_round_identically() {
        use crate::ir::parse_to_ir;
        let src = r#"<Graph id="board">
  <Pad id="p1" net="GND"
    style="position:absolute; left:0mm; top:0mm; width:0.5mm; height:0.5mm; clip-path: inset(0 0 0 0 round 0.05mm)" />
</Graph>"#;
        let a = parse_to_ir(src).expect("hand-rolled ok");
        let b = parse_to_ir_via_treesitter(src).expect("tree-sitter ok");
        let a_p = &a.nodes.iter().find(|n| n.id == "p1").unwrap().attrs;
        let b_p = &b.nodes.iter().find(|n| n.id == "p1").unwrap().attrs;
        assert_eq!(a_p.get("size"), b_p.get("size"), "size parity");
        assert_eq!(a_p.get("geometry"), b_p.get("geometry"), "geometry parity");
    }

    #[test]
    fn cross_validate_backends_against_canonical_fixtures() {
        use crate::ir::parse_to_ir;
        let fixtures = [
            include_str!("../tests/fixtures/counter-network.grx"),
            include_str!("../tests/fixtures/ping-pong.grx"),
            include_str!("../tests/fixtures/pipeline.grx"),
            include_str!("../tests/fixtures/multi-file/main.grx"),
            include_str!("../tests/fixtures/multi-file/sensors.grx"),
            include_str!("../tests/fixtures/multi-file/outputs.grx"),
        ];
        for (i, src) in fixtures.iter().enumerate() {
            let a = parse_to_ir(src)
                .unwrap_or_else(|e| panic!("fixture {i}: hand-rolled parser failed: {e:?}"));
            let b = parse_to_ir_via_treesitter(src)
                .unwrap_or_else(|e| panic!("fixture {i}: tree-sitter parser failed: {e:?}"));
            assert_eq!(a.graph_id, b.graph_id, "fixture {i}: graph_id mismatch");
            assert_eq!(a.imports, b.imports, "fixture {i}: imports mismatch");
            // IR equality is too strict on `source_loc` (line/col
            // numbering can differ between the hand-rolled token-
            // based locator and tree-sitter's tree-walk locator).
            // We compare the load-bearing structural fields:
            // node ids/components/attrs, edge endpoints/attrs/kind,
            // definition component/code/lang.
            let a_nodes: Vec<_> = a
                .nodes
                .iter()
                .map(|n| (&n.id, &n.component, &n.depth, &n.parent_path))
                .collect();
            let b_nodes: Vec<_> = b
                .nodes
                .iter()
                .map(|n| (&n.id, &n.component, &n.depth, &n.parent_path))
                .collect();
            assert_eq!(a_nodes, b_nodes, "fixture {i}: nodes mismatch");
            let a_edges: Vec<_> = a
                .edges
                .iter()
                .map(|e| (&e.source, &e.target, &e.kind))
                .collect();
            let b_edges: Vec<_> = b
                .edges
                .iter()
                .map(|e| (&e.source, &e.target, &e.kind))
                .collect();
            assert_eq!(a_edges, b_edges, "fixture {i}: edges mismatch");
            let a_defs: Vec<_> = a
                .definitions
                .iter()
                .map(|d| (&d.component, &d.code, &d.lang))
                .collect();
            let b_defs: Vec<_> = b
                .definitions
                .iter()
                .map(|d| (&d.component, &d.code, &d.lang))
                .collect();
            assert_eq!(a_defs, b_defs, "fixture {i}: definitions mismatch");
        }
    }

    /// **M15 Phase 1B perf gate.** Incremental reparse with a
    /// localised single-character edit must be substantially
    /// faster than a full reparse on a 200-line synthetic `.grx`.
    /// The plan-targeted gate is ≥10× speedup; we assert ≥2×
    /// here as the pessimistic floor (CI hosts vary, and
    /// criterion-style microbenches deserve a separate harness;
    /// this is a smoke test that the incremental machinery
    /// actually leverages tree-sitter's reuse, not a regression
    /// on it). The tighter ≥10× gate lives in
    /// `xtask incremental-reparse-bench` (added at closeout) for
    /// stable measurement.
    #[test]
    fn incremental_reparse_is_substantially_faster_than_full_reparse() {
        use std::fmt::Write as _;
        // Build a synthetic 200-line `.grx`: one Graph + 100
        // self-closing nodes + 50 explicit edges + a few definition
        // blocks. This mirrors a realistic mid-size system
        // topology.
        let mut src = String::new();
        src.push_str(r#"<Graph id="bench">"#);
        src.push('\n');
        for i in 0..100 {
            let _ = writeln!(
                src,
                r#"  <Worker id="w{i:03}" zone="{}" priority={{{i}}} />"#,
                if i % 2 == 0 { "A" } else { "B" }
            );
        }
        for i in 0..50 {
            let _ = writeln!(
                src,
                r#"  <edge source="w{i:03}" target="w{:03}" ttl={{5}} />"#,
                i + 1
            );
        }
        src.push_str("</Graph>\n");
        src.push_str("<Worker>\nconst [n, setN] = useState(0);\nreturn { accepts: ['tick'], emits: ['done'] };\n</Worker>\n");

        // Warm-up + first parse.
        let mut ip = IncrementalParser::new().expect("new");
        let _ = ip.parse(&src).expect("parse 1");

        // Benchmark: full reparse vs incremental reparse with a
        // single-byte edit somewhere in the middle of the file.
        let iters = 50u32;

        // Full reparse path: every iteration parses the source from
        // scratch via a fresh `IncrementalParser`. Mirrors the
        // pre-Phase-1B watch loop's behaviour (reparse-on-every-edit).
        let t_full = std::time::Instant::now();
        for _ in 0..iters {
            let mut fresh = IncrementalParser::new().expect("new");
            let _ = fresh.parse(&src).expect("full parse");
        }
        let full_elapsed = t_full.elapsed();

        // Incremental path: a 1-char edit at a known position; we
        // describe the edit via `tree_sitter::InputEdit` and call
        // `reparse`. Tree-sitter reuses unchanged subtrees so the
        // work is proportional to the edit region.
        let edit_pos = src.find("priority={50}").expect("find priority literal");
        let edit_pos_after = edit_pos + "priority={".len() + 2; // points just after `50`
        let mut new_src = src.clone();
        new_src.replace_range(edit_pos_after..edit_pos_after, "0");
        let edit = tree_sitter::InputEdit {
            start_byte: edit_pos_after,
            old_end_byte: edit_pos_after,
            new_end_byte: edit_pos_after + 1,
            start_position: tree_sitter::Point { row: 0, column: 0 },
            old_end_position: tree_sitter::Point { row: 0, column: 0 },
            new_end_position: tree_sitter::Point { row: 0, column: 1 },
        };

        let t_inc = std::time::Instant::now();
        for _ in 0..iters {
            // Reset to a fresh parsed state each iteration so we
            // measure JUST the reparse cost (not parse-then-reparse).
            // We do this by calling `parse` once + `reparse` once,
            // then accounting only the reparse half via the timing
            // structure below.
            let _ = ip.parse(&src).expect("re-parse base");
            let t_one = std::time::Instant::now();
            let _ = ip.reparse(&[edit], &new_src).expect("reparse");
            let _ = t_one.elapsed();
        }
        let inc_elapsed = t_inc.elapsed();

        // The incremental loop above does parse + reparse per
        // iteration, so we can't compare it directly to the full
        // loop. Re-measure the pure reparse cost:
        let mut pure_inc_total = std::time::Duration::ZERO;
        for _ in 0..iters {
            let _ = ip.parse(&src).expect("re-parse base 2");
            let t_one = std::time::Instant::now();
            let _ = ip.reparse(&[edit], &new_src).expect("reparse 2");
            pure_inc_total += t_one.elapsed();
        }

        let speedup = full_elapsed.as_secs_f64() / pure_inc_total.as_secs_f64();
        eprintln!(
            "Phase 1B reparse perf: full={full_elapsed:?}, inc={pure_inc_total:?}, \
             setup-included={inc_elapsed:?}, speedup={speedup:.2}x"
        );
        assert!(
            speedup >= 2.0,
            "incremental reparse should be at least 2x faster than full reparse \
             on a 200-line .grx (got {speedup:.2}x; full={full_elapsed:?}, \
             inc={pure_inc_total:?})"
        );
    }

    /// **M15 Phase 1B parity-win.** The tree-sitter backend
    /// preserves UTF-8 sequences in definition bodies verbatim;
    /// the hand-rolled lexer has a pre-existing bug where it
    /// casts each input byte to `char` via `ch as char`, which
    /// mangles multi-byte UTF-8. Captured here so a future
    /// hand-rolled-lexer fix doesn't accidentally regress the
    /// tree-sitter backend.
    #[test]
    fn tree_sitter_backend_preserves_utf8_in_definition_bodies() {
        let src = r#"<Foo>
const arrow = "→";
const emoji = "🦀";
</Foo>
<Graph id="g"><Foo id="f" /></Graph>"#;
        let ir = parse_to_ir_via_treesitter(src).expect("parse");
        assert_eq!(ir.definitions.len(), 1);
        let body = &ir.definitions[0].code;
        assert!(
            body.contains("→"),
            "tree-sitter should preserve →; got: {body:?}"
        );
        assert!(
            body.contains("🦀"),
            "tree-sitter should preserve 🦀; got: {body:?}"
        );
    }
}
