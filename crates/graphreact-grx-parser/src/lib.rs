//! Parser + IR builder for the GraphReact `.grx` markup language.
//!
//! Direct port of the TypeScript reference parser at
//! [`graphreact/src/core/parser/`]. The Rust port is a hand-rolled
//! recursive-descent parser; see [the M11 memory] for why we chose
//! that over a tree-sitter grammar (short version: tree-sitter for
//! `.grx` would need C external scanners for raw-content blocks and
//! brace-balanced expressions, which is meaningful build-system
//! complexity for a markup language with state-aware modes).
//!
//! Pipeline:
//!
//! ```text
//! source → Lexer → tokens → Parser → ast::GrxFile
//!                                        │
//!                                        ▼
//!                              MultiFileResolver (cycle detection)
//!                                        │
//!                                        ▼
//!                                  ir::GrxIr
//! ```

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::similar_names
)]

pub mod ast;
pub mod error;
pub mod ir;
pub mod lexer;
pub mod multi_file;
pub mod parser;
pub mod style;
pub mod treesitter;

pub use ast::{
    AttrMap, EdgeDecl, GraphDecl, GrxFile, GrxNode, ImportDirective, NodeDecl,
    NodeDefinitionBlock, SubgraphDecl, WalkedNode,
};
pub use error::{GrxParseError, GrxResult, SourceLoc};
pub use ir::{GrxIr, IrEdge, IrNode};
pub use lexer::{Lexer, Token, TokenType};
pub use multi_file::{
    resolve_multi_file, resolve_multi_file_cached, resolve_multi_file_cached_treesitter,
    MultiFileResolved, ParseCache,
};
pub use parser::Parser;
