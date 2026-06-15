//! PCBPnR Phase 5-D §3 — style-desugaring error type.

use crate::error::{GrxParseError, SourceLoc};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StyleError {
    #[error(
        "unknown CSS property `{property}` in `style=\"…\"`{}",
        suggestion.as_ref().map_or(String::new(), |s| format!(" — did you mean `{s}`?"))
    )]
    UnknownProperty {
        property: String,
        suggestion: Option<String>,
    },

    #[error("CSS property `{property}` value `{value}` has unsupported unit \
             — only `mm` is accepted in v1 (cm/inch/px are not supported)")]
    BadUnit { property: String, value: String },

    #[error(
        "element has both `style=\"…{style_prop}: …\"` and the numeric \
         attribute `{numeric_key}` — pick one form per element"
    )]
    ConflictingForms {
        style_prop: String,
        numeric_key: String,
    },

    #[error(
        "CSS `style=\"…\"` is missing `position: absolute` but uses \
         `left:`/`top:`/`width:`/`height:` — add `position: absolute;` first"
    )]
    MissingPositionAbsolute,

    #[error("malformed `clip-path` value `{snippet}` — supported forms: \
             inset(0 0 0 0 [round Rmm]), circle(Rmm at Xmm Ymm), \
             ellipse(Rxmm Rymm at Xmm Ymm), polygon(p1, p2, ...), \
             path('M ... Z'), kicad-shape(\"name\", {{...}})")]
    MalformedClipPath { snippet: String },

    #[error("CSS `transform: {transform_kind}` not supported in v1 \
             — only `rotate(Ndeg)` is recognized")]
    UnknownTransform { transform_kind: String },

    #[error("`clip-path: inset(...)` requires `width:Nmm` and `height:Nmm` \
             sibling properties to fill the rect dimensions")]
    InsetMissingWidthHeight,

    #[error("CSS value `{value}` for property `{property}` could not be \
             parsed as a number")]
    BadNumber { property: String, value: String },
}

impl StyleError {
    /// Convert into a [`GrxParseError`] anchored at the given source
    /// location. The parser's error taxonomy doesn't carry style-
    /// specific variants directly; wrap the readable message as
    /// `Parse` text so editor diagnostics still pinpoint the
    /// problematic element.
    pub fn into_parse_error(self, loc: SourceLoc) -> GrxParseError {
        GrxParseError::parse(self.to_string(), loc)
    }
}
