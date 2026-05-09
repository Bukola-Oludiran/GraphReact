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
            | Self::NonSelfClosingImport { loc } => Some(*loc),
            Self::CircularImport { .. } | Self::ImportNotFound { .. } | Self::Io { .. } => None,
        }
    }
}

pub type GrxResult<T> = Result<T, GrxParseError>;
