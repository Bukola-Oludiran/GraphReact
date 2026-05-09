//! Graph serialization — JSON and GraphML import/export.
//!
//! The JSON format is NetworkX's "node-link" shape, adapted to our
//! [`Attrs`] representation. GraphML is a subset of the standard XML
//! schema sufficient for round-tripping our attribute model.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Write as _;

use super::graph::CommunicationGraph;
use super::types::Attrs;

/// JSON wire format (node-link). Matches the TypeScript `JsonGraph` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonGraph {
    pub directed: bool,
    pub multigraph: bool,
    /// Graph-level attributes.
    pub graph: Attrs,
    pub nodes: Vec<JsonNode>,
    pub edges: Vec<JsonEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonNode {
    pub id: String,
    /// Flattened node attributes — collected via `serde_json::Value` so we
    /// preserve arbitrary user fields without enumerating them.
    #[serde(flatten)]
    pub attrs: Attrs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonEdge {
    pub source: String,
    pub target: String,
    pub key: String,
    #[serde(flatten)]
    pub attrs: Attrs,
}

/// Convert a [`CommunicationGraph`] to its JSON node-link representation.
pub fn to_json(graph: &CommunicationGraph) -> JsonGraph {
    let nodes: Vec<JsonNode> = graph
        .nodes()
        .map(|id| JsonNode {
            id: id.to_string(),
            attrs: graph
                .get_node_data(id)
                .expect("node exists during iteration")
                .clone(),
        })
        .collect();

    let edges: Vec<JsonEdge> = graph
        .edges()
        .map(|(u, v, key)| {
            let attrs = graph
                .get_edge_data(&u, &v, Some(&key))
                .expect("edge exists during iteration")
                .clone();
            JsonEdge {
                source: u,
                target: v,
                key,
                attrs,
            }
        })
        .collect();

    JsonGraph {
        directed: false,
        multigraph: true,
        graph: graph.graph.clone(),
        nodes,
        edges,
    }
}

/// Reconstruct a [`CommunicationGraph`] from a JSON node-link payload.
pub fn from_json(data: JsonGraph) -> CommunicationGraph {
    let mut graph = CommunicationGraph::new();
    graph.graph = data.graph;
    for node in data.nodes {
        graph.add_node(node.id, node.attrs);
    }
    for edge in data.edges {
        graph.add_edge(edge.source, edge.target, Some(edge.key), edge.attrs);
    }
    graph
}

/// Render a [`CommunicationGraph`] as a GraphML XML string.
///
/// This is a deliberately small subset of the GraphML spec — sufficient to
/// round-trip our node/edge attribute payloads but not a full schema-aware
/// serializer.
pub fn to_graphml(graph: &CommunicationGraph) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<graphml xmlns=\"http://graphml.graphstruct.org/xmlns\">\n");

    let graph_name = graph
        .graph
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("G");
    let _ = writeln!(
        out,
        "  <graph id=\"{}\" edgedefault=\"undirected\">",
        escape_xml(graph_name)
    );

    for id in graph.nodes() {
        let attrs = graph
            .get_node_data(id)
            .expect("node exists during iteration");
        write_element(&mut out, "node", &[("id", id)], attrs);
    }

    for (u, v, key) in graph.edges() {
        let attrs = graph
            .get_edge_data(&u, &v, Some(&key))
            .expect("edge exists during iteration");
        write_element(
            &mut out,
            "edge",
            &[("source", &u), ("target", &v), ("id", &key)],
            attrs,
        );
    }

    out.push_str("  </graph>\n");
    out.push_str("</graphml>\n");
    out
}

fn write_element(out: &mut String, tag: &str, attrs: &[(&str, &str)], data: &Attrs) {
    let attr_str: String = attrs
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_xml(v)))
        .collect::<Vec<_>>()
        .join(" ");

    if data.is_empty() {
        let _ = writeln!(out, "    <{tag} {attr_str} />");
        return;
    }

    let _ = writeln!(out, "    <{tag} {attr_str}>");
    for (k, v) in data {
        let _ = writeln!(
            out,
            "      <data key=\"{}\">{}</data>",
            escape_xml(k),
            escape_xml(&value_to_string(v))
        );
    }
    let _ = writeln!(out, "    </{tag}>");
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}
