//! Behavioral spec for graph serialization — ported from
//! `graphreact/src/__tests__/graph/serialization.test.ts`.

use graphreact_core::{from_json, to_graphml, to_json, Attrs, CommunicationGraph};
use serde_json::json;

fn attrs(value: serde_json::Value) -> Attrs {
    match value {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => panic!("attrs!() requires a JSON object"),
    }
}

#[test]
fn round_trips_a_graph_through_json() {
    let mut g = CommunicationGraph::new();
    g.graph.insert("name".into(), json!("test"));
    g.add_node("a", attrs(json!({ "label": "A" })));
    g.add_node("b", attrs(json!({ "label": "B" })));
    g.add_edge("a", "b", Some("0".into()), attrs(json!({ "weight": 5 })));
    g.add_edge("a", "b", Some("1".into()), attrs(json!({ "weight": 10 })));

    let json = to_json(&g);
    assert!(json.multigraph);
    assert!(!json.directed);
    assert_eq!(json.nodes.len(), 2);
    assert_eq!(json.edges.len(), 2);

    let g2 = from_json(json);
    assert_eq!(g2.order(), 2);
    assert_eq!(g2.size(), 2);
    assert_eq!(
        g2.get_node_data("a").unwrap().get("label"),
        Some(&serde_json::json!("A"))
    );
    assert_eq!(
        g2.get_edge_data("a", "b", Some("0")).unwrap().get("weight"),
        Some(&serde_json::json!(5))
    );
    assert_eq!(g2.graph.get("name"), Some(&serde_json::json!("test")));
}

#[test]
fn handles_empty_graph() {
    let g = CommunicationGraph::new();
    let json = to_json(&g);
    let g2 = from_json(json);
    assert_eq!(g2.order(), 0);
    assert_eq!(g2.size(), 0);
}

#[test]
fn graphml_produces_valid_xml() {
    let mut g = CommunicationGraph::new();
    g.graph.insert("name".into(), json!("test"));
    g.add_node("a", attrs(json!({ "label": "Node A" })));
    g.add_node("b", Attrs::new());
    g.add_edge("a", "b", Some("0".into()), attrs(json!({ "weight": 3 })));

    let xml = to_graphml(&g);
    assert!(xml.contains("<?xml"));
    assert!(xml.contains("<graphml"));
    assert!(xml.contains("<node id=\"a\">"));
    assert!(xml.contains("<node id=\"b\" />"));
    assert!(xml.contains("source=\"a\""));
    assert!(xml.contains("target=\"b\""));
    assert!(xml.contains("<data key=\"weight\">3</data>"));
}

#[test]
fn graphml_escapes_special_xml_characters() {
    let mut g = CommunicationGraph::new();
    g.add_node("a&b", attrs(json!({ "label": "<special>" })));

    let xml = to_graphml(&g);
    assert!(xml.contains("a&amp;b"));
    assert!(xml.contains("&lt;special&gt;"));
}
