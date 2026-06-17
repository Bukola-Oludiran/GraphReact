//! PCBPnR Phase 5-D §3 — CSS-shape `style="…"` attribute desugarer.
//!
//! Accepts a CSS-shaped property string on any `.grx` element and
//! desugars it to the typed numeric AttrMap form that downstream
//! consumers already read. The LLM authors in CSS shape (`position:
//! absolute; left:30mm; top:5mm; rotate:90deg; layer:F.Cu`); the IR
//! and runtime see only the numeric attributes (`at: {x: 30, y: 5,
//! rot: 90}`, `layer: "F.Cu"`).
//!
//! Module-isolated so PCB-domain semantics don't leak into the
//! parser core. Invoked from `crate::ir::build_ir` as a post-parse
//! pass — `<style>=...` keys are removed from the resulting IR.
//!
//! ## Coordination with PCBPnR
//!
//! The desugared `geometry={{kind: ..., ...}}` AttrMap shape
//! deserializes via serde into PCBPnR's `pcb_substrate::Geometry`
//! with `tag = "kind", rename_all = "snake_case"`. Variant names:
//! `rect`, `round_rect`, `circle`, `ellipse`, `polygon`, `path`,
//! `native`.

mod clip_path;
mod css_lexer;
mod desugar;
mod error;
mod resugar;
mod units;

pub use desugar::desugar_style;
pub use error::StyleError;
pub use resugar::{resugar_attrs, Resugared};
