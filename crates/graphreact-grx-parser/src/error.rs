//! Error types for the `.grx` parser.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Source location: 1-indexed line and column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SourceLoc {
    pub line: usize,
    pub column: usize,
}

impl SourceLoc {
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

impl std::fmt::Display for SourceLoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

#[derive(Debug, Error)]
pub enum GrxParseError {
    #[error("[{loc}] {message}")]
    Lex { message: String, loc: SourceLoc },

    #[error("[{loc}] {message}")]
    Parse { message: String, loc: SourceLoc },

    #[error("[{loc}] graph requires an `id` attribute")]
    MissingGraphId { loc: SourceLoc },

    #[error("[{loc}] node `<{component}>` requires an `id` attribute")]
    MissingNodeId { component: String, loc: SourceLoc },

    #[error("[{loc}] edge requires `source` and `target` attributes")]
    MissingEdgeEndpoints { loc: SourceLoc },

    #[error("[{loc}] only one `<Graph>` block per file is allowed")]
    MultipleGraphs { loc: SourceLoc },

    #[error("[{loc}] root element must be `<Graph>`, got `<{got}>`")]
    BadRoot { got: String, loc: SourceLoc },

    #[error("[{loc}] expected `</{expected}>`, got `</{got}>`")]
    MismatchedClose {
        expected: String,
        got: String,
        loc: SourceLoc,
    },

    #[error("[{loc}] `<import>` requires a `src` attribute")]
    MissingImportSrc { loc: SourceLoc },

    #[error("[{loc}] `<import>` must be self-closing")]
    NonSelfClosingImport { loc: SourceLoc },

    // PCBPnR Phase 5-D §2.1. Alias must be a Rust identifier
    // (`[a-zA-Z_][a-zA-Z0-9_]*`); stricter than `id` because
    // dot-prefix splice addressing (`rf.ANT1`) makes hyphens
    // ambiguous in `{expr}` values.
    #[error("[{loc}] `<import as=\"{alias}\">` is not a valid alias \
             — must match `[a-zA-Z_][a-zA-Z0-9_]*` \
             (note: stricter than `id` attributes because dot-prefix \
             splice addressing makes hyphens ambiguous in `{{expr}}` values)")]
    InvalidImportAlias { alias: String, loc: SourceLoc },

    // PCBPnR Phase 5-D §2.2. `<Subgraph name="...">` requires a name
    // attribute (the export key).
    #[error("[{loc}] `<Subgraph>` requires a `name` attribute")]
    MissingSubgraphName { loc: SourceLoc },

    // PCBPnR Phase 5-D §2.2. Within a single file, two `<Subgraph>`
    // blocks may not share a name.
    #[error("[{second_loc}] subgraph `{name}` already declared at {first_loc} \
             — names must be unique within a file")]
    DuplicateSubgraphName {
        name: String,
        first_loc: SourceLoc,
        second_loc: SourceLoc,
    },

    // PCBPnR Phase 5-D §2.3 — Subsystem splice errors.
    #[error("[{loc}] `<Subsystem>` requires a `ref` attribute (form: `<alias>:<name>` or `<name>`)")]
    SubsystemMissingRef { loc: SourceLoc },

    #[error("[{loc}] `<Subsystem>` requires an `id` attribute (the local id the splice is bound to)")]
    SubsystemMissingId { loc: SourceLoc },

    #[error(
        "[{loc}] `<Subsystem ref=\"{ref_value}\">` is malformed \
         — expected `<alias>:<name>` or `<name>` with both halves non-empty"
    )]
    SubsystemMalformedRef { ref_value: String, loc: SourceLoc },

    #[error("[{loc}] no subgraph found for `<Subsystem ref=\"{qualified_name}\">` — check the `as=` alias and `<Subgraph name=...>` keys")]
    SubsystemUnknownRef {
        qualified_name: String,
        loc: SourceLoc,
    },

    #[error("[{loc}] splice cycle detected: {chain}")]
    SpliceCycle { chain: String, loc: SourceLoc },

    // PCBPnR Phase 5-D §2.2 — when two imports both register the
    // same qualified subgraph key (e.g., both aliased as the same
    // name OR both flattened without aliasing and exporting the
    // same name).
    #[error(
        "[{loc}] subgraph `{qualified_name}` declared by multiple imports \
         — aliases must be distinct, or use `as=` to disambiguate"
    )]
    DuplicateSubgraphAcrossImports {
        qualified_name: String,
        loc: SourceLoc,
    },

    // PCBPnR Phase 5-D follow-on — external definition body file
    // (`<Component src="./body.py"/>`) couldn't be read.
    #[error(
        "[{loc}] failed to read external body for `<{component}>` from `{path}`: {source}"
    )]
    DefinitionBodyIo {
        component: String,
        path: String,
        #[source]
        source: std::io::Error,
        loc: SourceLoc,
    },

    #[error("circular import detected: {chain}")]
    CircularImport { chain: String },

    #[error("import `{src}` not found (resolved to `{resolved}`)")]
    ImportNotFound { src: String, resolved: String },

    #[error("io error reading `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    // PCBPnR Phase 5-D §2.4 reconciliation. Within-graph duplicate IDs
    // are the load-bearing multi-parent pattern (TS canonical at
    // `compiler.ts:6,98`; tested at `ir.rs::ir_multiparent_creates_one_node_two_edges`).
    // First occurrence declares attrs + component tag; subsequent
    // occurrences must be attribute-free re-mentions using the same
    // component tag. Cross-file collisions (after multi-file +
    // aliasing + splicing) surface here too, since both sides carry
    // attrs.
    #[error("[{second_loc}] node `{id}` already declared at {first_loc} \
             — multi-parent references must be attribute-free \
             (only `id=` is allowed on the re-mention)")]
    IdConflictWithAttrs {
        id: String,
        first_loc: SourceLoc,
        second_loc: SourceLoc,
    },

    #[error("[{second_loc}] node `{id}` redeclared as `<{second_component}>` \
             but first declared as `<{first_component}>` at {first_loc} \
             — multi-parent references must use the same component tag")]
    IdComponentMismatch {
        id: String,
        first_component: String,
        first_loc: SourceLoc,
        second_component: String,
        second_loc: SourceLoc,
    },
}

impl GrxParseError {
    pub fn lex(message: impl Into<String>, loc: SourceLoc) -> Self {
        Self::Lex {
            message: message.into(),
            loc,
        }
    }

    pub fn parse(message: impl Into<String>, loc: SourceLoc) -> Self {
        Self::Parse {
            message: message.into(),
            loc,
        }
    }

    /// Source location for variants that carry one. CLI uses this to
    /// render an inline source-snippet with a caret pointing at the
    /// column. Returns `None` for cross-file errors where the location
    /// is the file boundary itself, not a line/column.
    #[must_use]
    pub const fn source_loc(&self) -> Option<SourceLoc> {
        match self {
            Self::Lex { loc, .. }
            | Self::Parse { loc, .. }
            | Self::MissingGraphId { loc }
            | Self::MissingNodeId { loc, .. }
            | Self::MissingEdgeEndpoints { loc }
            | Self::MultipleGraphs { loc }
            | Self::BadRoot { loc, .. }
            | Self::MismatchedClose { loc, .. }
            | Self::MissingImportSrc { loc }
            | Self::NonSelfClosingImport { loc }
            | Self::InvalidImportAlias { loc, .. }
            | Self::MissingSubgraphName { loc }
            | Self::SubsystemMissingRef { loc }
            | Self::SubsystemMissingId { loc }
            | Self::SubsystemMalformedRef { loc, .. }
            | Self::SubsystemUnknownRef { loc, .. }
            | Self::SpliceCycle { loc, .. }
            | Self::DuplicateSubgraphAcrossImports { loc, .. }
            | Self::DefinitionBodyIo { loc, .. } => Some(*loc),
            // For collision-style errors, the "primary" loc is the
            // problematic (second) occurrence — that's what the
            // editor should jump to. The first occurrence's loc
            // appears in the `Display` message body for context.
            Self::DuplicateSubgraphName { second_loc, .. }
            | Self::IdConflictWithAttrs { second_loc, .. }
            | Self::IdComponentMismatch { second_loc, .. } => Some(*second_loc),
            Self::CircularImport { .. } | Self::ImportNotFound { .. } | Self::Io { .. } => None,
        }
    }
}

pub type GrxResult<T> = Result<T, GrxParseError>;
