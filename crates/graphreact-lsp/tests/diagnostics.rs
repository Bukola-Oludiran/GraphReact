//! Phase 2 integration — typed parser errors → LSP `Diagnostic`.
//!
//! Drives the diagnostics module directly against fixture files so we
//! lock in the (code, range, related_information) shape for each
//! representative variant.

use graphreact_grx_parser::ir::parse_to_ir;
use graphreact_lsp::diagnostics::{error_code, from_parse_error};
use tower_lsp_server::ls_types::{DiagnosticSeverity, NumberOrString, Uri};

fn uri(s: &str) -> Uri {
    s.parse().expect("uri")
}

#[test]
fn missing_graph_id_surfaces_as_error_diagnostic() {
    let src = include_str!("fixtures/missing-graph-id.grx");
    let err = parse_to_ir(src).expect_err("must fail");
    let diag = from_parse_error(&err, src, &uri("file:///tmp/x.grx"));
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    match diag.code {
        Some(NumberOrString::String(ref code)) => assert_eq!(code, "missing-graph-id"),
        other => panic!("expected stable string code, got {other:?}"),
    }
    // Range starts on the first line (line 0).
    assert_eq!(diag.range.start.line, 0);
    // Message contains the canonical render from thiserror.
    assert!(
        diag.message.contains("graph requires an `id` attribute"),
        "got: {}",
        diag.message
    );
}

#[test]
fn well_formed_fixture_emits_no_errors() {
    let src = include_str!("fixtures/well-formed.grx");
    let ir = parse_to_ir(src).expect("parses ok");
    assert_eq!(ir.graph_id, "counter-net");
    assert_eq!(ir.nodes.len(), 3);
    assert_eq!(ir.edges.iter().filter(|e| matches!(e.kind, graphreact_grx_parser::ir::EdgeKind::Explicit)).count(), 2);
}

#[test]
fn duplicate_subgraph_carries_related_information() {
    let src = "<Subgraph name=\"X\"><A id=\"a\"/></Subgraph>\n<Subgraph name=\"X\"><B id=\"b\"/></Subgraph>\n<Graph id=\"g\"/>";
    let err = parse_to_ir(src).expect_err("must fail");
    assert_eq!(error_code(&err), "duplicate-subgraph-name");
    let diag = from_parse_error(&err, src, &uri("file:///tmp/x.grx"));
    let related = diag.related_information.expect("related set");
    assert_eq!(related.len(), 1);
    assert!(related[0].message.contains("first declaration"));
}
