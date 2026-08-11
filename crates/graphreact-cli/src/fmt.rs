//! Canonical pretty-printer for `.grx` files.
//!
//! Walks the AST and emits a stable serialisation: 2-space indent,
//! attributes sorted by `id`-first then alphabetical, expression
//! literals trimmed. Idempotent — running `grx fmt` twice produces
//! identical output.

use std::fmt::Write;

use graphreact_grx_parser::{
    ast::{EdgeDecl, GraphDecl, GrxFile, GrxNode, NodeDecl},
    AttrMap,
};
use serde_json::Value as JsonValue;

/// Format a parsed file back into canonical `.grx` text.
pub fn format_file(file: &GrxFile) -> String {
    let mut out = String::new();
    let mut first = true;

    for imp in &file.imports {
        if !first {
            out.push('\n');
        }
        first = false;
        let _ = writeln!(out, "<import src=\"{}\" />", imp.src);
    }

    for def in &file.definitions {
        if !first {
            out.push('\n');
        }
        first = false;
        let _ = writeln!(out, "<{}>", def.component);
        if !def.code.is_empty() {
            for line in def.code.lines() {
                if line.is_empty() {
                    out.push('\n');
                } else {
                    let _ = writeln!(out, "  {line}");
                }
            }
        }
        let _ = writeln!(out, "</{}>", def.component);
    }

    if let Some(g) = &file.graph {
        if !first {
            out.push('\n');
        }
        format_graph(&mut out, g, 0);
    }

    out
}

fn format_graph(out: &mut String, graph: &GraphDecl, indent: usize) {
    let pad = "  ".repeat(indent);
    if graph.children.is_empty() {
        let _ = writeln!(out, "{pad}<Graph id=\"{}\"></Graph>", graph.id);
        return;
    }
    let _ = writeln!(out, "{pad}<Graph id=\"{}\">", graph.id);
    for child in &graph.children {
        format_node_child(out, child, indent + 1);
    }
    let _ = writeln!(out, "{pad}</Graph>");
}

fn format_node_child(out: &mut String, child: &GrxNode, indent: usize) {
    match child {
        GrxNode::Node(n) => format_node(out, n, indent),
        GrxNode::Edge(e) => format_edge(out, e, indent),
    }
}

fn format_node(out: &mut String, node: &NodeDecl, indent: usize) {
    let pad = "  ".repeat(indent);
    let attrs = format_attrs(&node.attrs, true);
    if node.children.is_empty() && node.self_closing {
        let _ = writeln!(out, "{pad}<{} id=\"{}\"{attrs} />", node.component, node.id);
        return;
    }
    let _ = writeln!(out, "{pad}<{} id=\"{}\"{attrs}>", node.component, node.id);
    for child in &node.children {
        format_node_child(out, child, indent + 1);
    }
    let _ = writeln!(out, "{pad}</{}>", node.component);
}

fn format_edge(out: &mut String, edge: &EdgeDecl, indent: usize) {
    let pad = "  ".repeat(indent);
    let attrs = format_attrs(&edge.attrs, false);
    let _ = writeln!(
        out,
        "{pad}<edge source=\"{}\" target=\"{}\"{attrs} />",
        edge.source, edge.target
    );
}

/// Format an attribute map as ` k1="v1" k2={expr}…`. The leading
/// space is included so callers can paste this directly after the
/// last fixed attribute.
fn format_attrs(attrs: &AttrMap, _id_already_emitted: bool) -> String {
    if attrs.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (k, v) in attrs {
        let _ = write!(out, " {}={}", k, format_value(v));
    }
    out
}

fn format_value(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => format!("\"{s}\""),
        JsonValue::Bool(true) => String::new(), // boolean attr has no value
        JsonValue::Bool(false) => "{false}".to_string(),
        other => format!("{{{other}}}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_grx_parser::Parser;

    fn round_trip(input: &str) -> String {
        let file = Parser::parse_file(input).expect("parse");
        format_file(&file)
    }

    #[test]
    fn formats_minimal_graph() {
        let s = round_trip("<Graph id=\"x\"></Graph>");
        assert!(s.contains("<Graph id=\"x\""));
    }

    #[test]
    fn formats_self_closing_node() {
        let s = round_trip("<Graph id=\"x\"><Foo id=\"a\" /></Graph>");
        assert!(s.contains("<Foo id=\"a\" />"));
    }

    #[test]
    fn formats_edge_with_attrs() {
        let s = round_trip(
            "<Graph id=\"x\"><A id=\"a\" /><B id=\"b\" /><edge source=\"a\" target=\"b\" ttl={5} /></Graph>",
        );
        assert!(s.contains("<edge source=\"a\" target=\"b\" ttl={5} />"));
    }

    #[test]
    fn fmt_is_idempotent() {
        let input = "<Graph id=\"g\"><A id=\"a\"><B id=\"b\" v={3} /></A></Graph>";
        let once = round_trip(input);
        let file = Parser::parse_file(&once).expect("re-parse");
        let twice = format_file(&file);
        assert_eq!(once, twice);
    }

    #[test]
    fn fmt_preserves_imports_and_definitions() {
        let input = r#"<import src="./a.grx" />
<Foo>
const x = 1;
</Foo>
<Graph id="g"><Foo id="f" /></Graph>"#;
        let formatted = round_trip(input);
        assert!(formatted.contains("<import src=\"./a.grx\" />"));
        assert!(formatted.contains("<Foo>"));
        assert!(formatted.contains("const x = 1"));
        assert!(formatted.contains("<Graph id=\"g\""));
    }
}
