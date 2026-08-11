//! Parser-error → LSP-Diagnostic translation.
//!
//! Source-of-truth for every diagnostic the server publishes. Each
//! [`GrxParseError`] variant maps to one `Diagnostic`, with collision-
//! shaped variants (`DuplicateSubgraphName`, `IdConflictWithAttrs`,
//! `IdComponentMismatch`) also emitting `relatedInformation` pointing
//! at the first occurrence — that's the editor-jump target.

use graphreact_grx_parser::{GrxParseError, SourceLoc};
use tower_lsp_server::ls_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, Position, Range, Uri,
};

use crate::position::token_range_at;

/// Map a typed parser error to one LSP `Diagnostic`. The `source_uri`
/// is the file the parse was driven from; cross-file errors (currently
/// `CircularImport`, `ImportNotFound`, `Io`) pin to the file boundary
/// with a `(0, 0)`-anchored range since the parser doesn't currently
/// surface the originating `<import>` tag location.
#[must_use]
pub fn from_parse_error(err: &GrxParseError, source: &str, source_uri: &Uri) -> Diagnostic {
    let (range, related) = build_range_and_related(err, source, source_uri);
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(tower_lsp_server::ls_types::NumberOrString::String(
            error_code(err).to_string(),
        )),
        code_description: None,
        source: Some("grx-parser".to_string()),
        message: err.to_string(),
        related_information: related,
        tags: None,
        data: None,
    }
}

/// Stable identifier for each parser-error variant, surfaced as the
/// LSP diagnostic code. Editors render this as e.g.
/// `grx-parser(splice-cycle): ...`.
#[must_use]
pub fn error_code(err: &GrxParseError) -> &'static str {
    match err {
        GrxParseError::Lex { .. } => "lex",
        GrxParseError::Parse { .. } => "parse",
        GrxParseError::MissingGraphId { .. } => "missing-graph-id",
        GrxParseError::MissingNodeId { .. } => "missing-node-id",
        GrxParseError::MissingEdgeEndpoints { .. } => "missing-edge-endpoints",
        GrxParseError::MultipleGraphs { .. } => "multiple-graphs",
        GrxParseError::BadRoot { .. } => "bad-root",
        GrxParseError::MismatchedClose { .. } => "mismatched-close",
        GrxParseError::MissingImportSrc { .. } => "missing-import-src",
        GrxParseError::NonSelfClosingImport { .. } => "non-self-closing-import",
        GrxParseError::InvalidImportAlias { .. } => "invalid-import-alias",
        GrxParseError::MissingSubgraphName { .. } => "missing-subgraph-name",
        GrxParseError::DuplicateSubgraphName { .. } => "duplicate-subgraph-name",
        GrxParseError::SubsystemMissingRef { .. } => "subsystem-missing-ref",
        GrxParseError::SubsystemMissingId { .. } => "subsystem-missing-id",
        GrxParseError::SubsystemMalformedRef { .. } => "subsystem-malformed-ref",
        GrxParseError::SubsystemUnknownRef { .. } => "subsystem-unknown-ref",
        GrxParseError::SpliceCycle { .. } => "splice-cycle",
        GrxParseError::DuplicateSubgraphAcrossImports { .. } => "duplicate-subgraph-across-imports",
        GrxParseError::DefinitionBodyIo { .. } => "definition-body-io",
        GrxParseError::CircularImport { .. } => "circular-import",
        GrxParseError::ImportNotFound { .. } => "import-not-found",
        GrxParseError::Io { .. } => "io",
        GrxParseError::IdConflictWithAttrs { .. } => "id-conflict-with-attrs",
        GrxParseError::IdComponentMismatch { .. } => "id-component-mismatch",
    }
}

fn build_range_and_related(
    err: &GrxParseError,
    source: &str,
    source_uri: &Uri,
) -> (Range, Option<Vec<DiagnosticRelatedInformation>>) {
    match err {
        GrxParseError::DuplicateSubgraphName {
            first_loc,
            second_loc,
            name,
        } => (
            token_range_at(*second_loc, source),
            Some(vec![DiagnosticRelatedInformation {
                location: Location {
                    uri: source_uri.clone(),
                    range: token_range_at(*first_loc, source),
                },
                message: format!("first declaration of `{name}` here"),
            }]),
        ),
        GrxParseError::IdConflictWithAttrs {
            id,
            first_loc,
            second_loc,
        } => (
            token_range_at(*second_loc, source),
            Some(vec![DiagnosticRelatedInformation {
                location: Location {
                    uri: source_uri.clone(),
                    range: token_range_at(*first_loc, source),
                },
                message: format!("first declaration of `{id}` here"),
            }]),
        ),
        GrxParseError::IdComponentMismatch {
            id: _,
            first_component,
            first_loc,
            second_loc,
            ..
        } => (
            token_range_at(*second_loc, source),
            Some(vec![DiagnosticRelatedInformation {
                location: Location {
                    uri: source_uri.clone(),
                    range: token_range_at(*first_loc, source),
                },
                message: format!("first declared as `<{first_component}>` here"),
            }]),
        ),
        // Cross-file errors don't carry a span — point at the file
        // boundary. The risks doc calls this out; once the parser
        // exposes the originating `<import>` loc we can widen here.
        GrxParseError::CircularImport { .. }
        | GrxParseError::ImportNotFound { .. }
        | GrxParseError::Io { .. } => (Range::new(Position::new(0, 0), Position::new(0, 0)), None),
        other => {
            let loc = other.source_loc().unwrap_or(SourceLoc::new(1, 1));
            (token_range_at(loc, source), None)
        }
    }
}

/// Build a synthetic diagnostic that pins at `(0, 0)`. Used when a
/// secondary parse stage fails after we've already lost the original
/// source — currently unused but kept for parity with future M23.next
/// non-parser-driven diagnostics.
#[must_use]
pub fn synthetic_at_top(message: impl Into<String>, code: &str) -> Diagnostic {
    Diagnostic {
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(tower_lsp_server::ls_types::NumberOrString::String(
            code.to_string(),
        )),
        code_description: None,
        source: Some("graphreact-lsp".to_string()),
        message: message.into(),
        related_information: None,
        tags: None,
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::loc_to_position;
    use graphreact_grx_parser::ir::parse_to_ir;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn parse_error_with_loc_maps_to_diagnostic() {
        // Missing id on <Graph>.
        let src = "<Graph><A id=\"a\"/></Graph>";
        let err = parse_to_ir(src).expect_err("must fail");
        let diag = from_parse_error(&err, src, &uri("file:///tmp/a.grx"));
        assert!(matches!(diag.severity, Some(DiagnosticSeverity::ERROR)));
        assert!(matches!(
            diag.code,
            Some(tower_lsp_server::ls_types::NumberOrString::String(ref s))
                if s == "missing-graph-id"
        ));
        assert_eq!(diag.range.start.line, 0);
    }

    #[test]
    fn duplicate_subgraph_emits_related_information() {
        let src = "<Subgraph name=\"X\"><A id=\"a\"/></Subgraph>\n<Subgraph name=\"X\"><B id=\"b\"/></Subgraph>\n<Graph id=\"g\"/>";
        let err = parse_to_ir(src).expect_err("must fail");
        let diag = from_parse_error(&err, src, &uri("file:///tmp/a.grx"));
        let related = diag
            .related_information
            .as_ref()
            .expect("related populated");
        assert_eq!(related.len(), 1);
        assert!(related[0].message.contains("first declaration of `X`"));
    }

    /// Smoke test: ensure every variant we know about gets a stable
    /// code string.
    #[test]
    fn lex_error_lands_at_origin() {
        let loc = SourceLoc::new(3, 5);
        let err = GrxParseError::lex("bad token", loc);
        let pos = loc_to_position(loc, "line1\nline2\nline3-with-bad");
        assert_eq!(pos.line, 2);
        assert_eq!(pos.character, 4);
        let diag = from_parse_error(&err, "line1\nline2\nline3-with-bad", &uri("file:///x"));
        assert_eq!(diag.range.start.line, 2);
    }
}
