//! **M23 — Language Server Protocol for `.grx`.**
//!
//! Outer-shell MVP that surfaces the existing `graphreact-grx-parser`
//! pipeline (lexer + hand-rolled / tree-sitter parser + IR builder +
//! multi-file resolver + CSS-style desugarer) through the LSP wire
//! protocol. Editors that speak LSP — VS Code, Neovim, Emacs, Helix,
//! Zed — connect to this server over stdio and get diagnostics,
//! hover, goto-definition, and completion on `.grx` files without
//! any additional editor-side plumbing.
//!
//! The implementation deliberately stays at the **outer-shell**
//! layer: it never re-implements parsing or IR logic, it just maps
//! existing types to LSP types. When the parser's surface grows, the
//! LSP grows with it for free.
//!
//! Phases shipped (per the M23 plan locked at Q10):
//!
//! 1. Initialize / shutdown handshake + document sync.
//! 2. Diagnostics from typed `GrxParseError`.
//! 3. Hover with Markdown payloads for tags, attrs, splice targets,
//!    and `style=` property docs.
//! 4. Goto-definition for `<Subsystem ref="…">` and import aliases.
//! 5. Completion for tag names, attributes, style properties, and
//!    clip-path shapes.
//!
//! Deferred (M23.next): semantic tokens, in-body completions, VS
//! Code extension publishing, inlay hints, formatter, references.

#![allow(
    clippy::missing_const_for_fn,
    // `usize as u32` shows up everywhere we translate parser
    // `SourceLoc` (1-indexed, `usize`) into LSP `Position` (0-indexed,
    // `u32`). `.grx` files in the wild are well below `u32::MAX` lines
    // / columns; the workspace-wide style for the parser is to allow
    // these (Cargo.toml lints at the parser crate level).
    clippy::cast_possible_truncation,
    // `for (i, _) in xs.iter().enumerate()` is more readable than a
    // manually-incremented counter in tight string scanners.
    clippy::explicit_counter_loop,
    // `r#use::*` from the workspace clippy table is `pedantic`; the
    // patterns below are intentionally explicit for readability.
    clippy::needless_lifetimes,
    clippy::unnested_or_patterns,
    clippy::items_after_statements
)]

pub mod backend;
pub mod catalog;
pub mod completion;
pub mod diagnostics;
pub mod documents;
pub mod hover;
pub mod position;
pub mod resolve;

pub use backend::GrxLanguageServer;
pub use documents::{DocumentEntry, DocumentStore};
pub use resolve::{resolve_subsystem_target, resolve_with_store, SubsystemTarget};
