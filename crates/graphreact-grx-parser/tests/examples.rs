//! Integration tests against the live `.grx` examples shipped with the
//! TypeScript reference parser at
//! `GraphReact/graphreact/src/examples/`.
//!
//! These exercise the end-to-end pipeline (parse → IR) on the same
//! inputs the TS compiler is regressed against. They form the
//! "byte-identical IR (modulo deterministic ordering) to TS parser"
//! acceptance gate's local half — direct cross-implementation parity
//! waits on Python/JS golden fixtures (same situation as M9/M10).

use std::path::PathBuf;

use graphreact_grx_parser::{
    ir::{build_ir, ir_to_json, EdgeKind},
    multi_file::resolve_multi_file,
    parser::Parser,
};

fn fixture_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

#[test]
fn parses_ping_pong_example() {
    let src = read_fixture("ping-pong.grx");
    let file = Parser::parse_file(&src).expect("parse");
    let g = file.graph.expect("graph");
    assert_eq!(g.id, "ping-pong");
    let ir = build_ir(&graphreact_grx_parser::GrxFile {
        imports: file.imports,
        graph: Some(g),
        definitions: file.definitions,
    });
    assert_eq!(ir.nodes.len(), 2);
    let ids: Vec<&str> = ir.nodes.iter().map(|n| n.id.as_str()).collect();
    assert!(ids.contains(&"ping"));
    assert!(ids.contains(&"pong"));
    let explicit_edges: Vec<_> = ir
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Explicit)
        .collect();
    assert_eq!(explicit_edges.len(), 1);
    assert_eq!(explicit_edges[0].source, "ping");
    assert_eq!(explicit_edges[0].target, "pong");
    assert_eq!(
        explicit_edges[0].attrs.get("ttl"),
        Some(&serde_json::json!(5))
    );
}

#[test]
fn parses_counter_network_example() {
    let src = read_fixture("counter-network.grx");
    let file = Parser::parse_file(&src).expect("parse");
    let ir = build_ir(&file);
    assert_eq!(ir.graph_id, "counter-network");
    // Nodes: agg, counter-a, counter-b, logger (and the 2 counters
    // appear twice, once under each parent — but they're the same
    // unique nodes).
    let mut ids: Vec<&str> = ir.nodes.iter().map(|n| n.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["agg", "counter-a", "counter-b", "logger"]);
    // Each counter has two parents (agg + logger) → 2 edges per counter.
    let counter_a_in: Vec<_> =
        ir.edges.iter().filter(|e| e.target == "counter-a").collect();
    assert_eq!(counter_a_in.len(), 2);
    let parents: std::collections::HashSet<&str> =
        counter_a_in.iter().map(|e| e.source.as_str()).collect();
    assert!(parents.contains("agg"));
    assert!(parents.contains("logger"));
    // Counter-a's `initialCount` and `label` attrs should be on the
    // first occurrence (under agg).
    let counter_a = ir.nodes.iter().find(|n| n.id == "counter-a").unwrap();
    assert_eq!(counter_a.attrs.get("initialCount"), Some(&serde_json::json!(0)));
    assert_eq!(
        counter_a.attrs.get("label"),
        Some(&serde_json::json!("Alpha"))
    );
}

#[test]
fn parses_pipeline_example() {
    let src = read_fixture("pipeline.grx");
    let file = Parser::parse_file(&src).expect("parse");
    let ir = build_ir(&file);
    assert_eq!(ir.graph_id, "pipeline");
    let ids: Vec<&str> = ir.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, vec!["source", "transform", "sink"]);
    // Linear chain: source → transform → sink (2 structural edges).
    assert_eq!(ir.edges.len(), 2);
    assert_eq!(ir.edges[0].source, "source");
    assert_eq!(ir.edges[0].target, "transform");
    assert_eq!(ir.edges[1].source, "transform");
    assert_eq!(ir.edges[1].target, "sink");
}

#[test]
fn parses_multi_file_greenhouse_root() {
    let src = read_fixture("multi-file/main.grx");
    let file = Parser::parse_file(&src).expect("parse");
    assert_eq!(file.imports.len(), 2);
    assert_eq!(file.imports[0].src, "./sensors.grx");
    assert_eq!(file.imports[1].src, "./outputs.grx");
    let g = file.graph.expect("graph");
    assert_eq!(g.id, "greenhouse");
    let ir = build_ir(&graphreact_grx_parser::GrxFile {
        imports: file.imports,
        graph: Some(g),
        definitions: file.definitions,
    });
    let ids: Vec<&str> = ir.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, vec!["agg", "temp-a", "temp-b", "humid-a", "log"]);
}

#[test]
fn resolves_multi_file_greenhouse_transitively() {
    let path = fixture_path("multi-file/main.grx");
    let resolved = resolve_multi_file(&path).expect("resolve");
    assert_eq!(resolved.imports.len(), 2);
    let import_names: Vec<_> = resolved
        .imports
        .iter()
        .map(|i| i.path.file_name().unwrap().to_str().unwrap())
        .collect();
    assert!(import_names.contains(&"sensors.grx"));
    assert!(import_names.contains(&"outputs.grx"));

    // Definitions: 2 from sensors.grx (TempSensor, HumiditySensor) +
    // 2 from outputs.grx (Logger, Aggregator) = 4 total.
    let total_defs: usize =
        resolved.imports.iter().map(|i| i.file.definitions.len()).sum();
    assert_eq!(total_defs, 4);
}

#[test]
fn ir_json_serialisation_is_stable_for_pipeline() {
    let src = read_fixture("pipeline.grx");
    let ir = graphreact_grx_parser::ir::parse_to_ir(&src).expect("parse");
    let json = ir_to_json(&ir);
    let s = serde_json::to_string(&json).expect("serialise");
    // Sanity-check the shape; full byte-comparison waits on TS golden
    // outputs being captured to a fixture.
    assert!(s.contains("\"graphId\":\"pipeline\""));
    assert!(s.contains("\"id\":\"source\""));
    assert!(s.contains("\"id\":\"transform\""));
    assert!(s.contains("\"id\":\"sink\""));
}

#[test]
fn parser_recovers_source_locations_for_diagnostics() {
    let src = read_fixture("ping-pong.grx");
    let file = Parser::parse_file(&src).expect("parse");
    let g = file.graph.expect("graph");
    // The `<Graph id="ping-pong">` opens on line 3 in the fixture.
    assert_eq!(g.loc.line, 3);
}

#[test]
fn definition_block_code_is_extracted_verbatim() {
    let src = read_fixture("multi-file/sensors.grx");
    let file = Parser::parse_file(&src).expect("parse");
    assert_eq!(file.definitions.len(), 2);
    let temp = file.definitions.iter().find(|d| d.component == "TempSensor").unwrap();
    assert!(temp.code.contains("useState(0)"));
    assert!(temp.code.contains("emit('sensor-data'"));
    let humid = file
        .definitions
        .iter()
        .find(|d| d.component == "HumiditySensor")
        .unwrap();
    assert!(humid.code.contains("setReading"));
}
