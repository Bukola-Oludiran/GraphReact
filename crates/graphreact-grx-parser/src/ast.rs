//! AST node types for `.grx` files.
//!
//! Mirrors the TypeScript reference at [`graphreact/src/core/parser/ast.ts`].
//! Values produced by the parser are owned (no string interning) — `.grx`
//! files are typically tens of kilobytes; the win from interning isn't
//! worth the API friction.

use serde_json::Value as JsonValue;

use crate::error::SourceLoc;

/// A whole `.grx` file (Phase-7 multi-file format). Either contains
/// graph markup, definition blocks (component bodies), or both.
///
/// `subgraphs` (PCBPnR Phase 5-D §2.2) holds zero-or-more named
/// reusable subgraph fragments. They are peer to the optional
/// top-level `<Graph>` block, addressable by name from importers
/// (qualified by the importer's `as=` alias if any). Subsystem
/// splice resolution at IR-build time replaces each `<Subsystem>`
/// reference with the named subgraph's children, prefixed with
/// the local id.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GrxFile {
    pub imports: Vec<ImportDirective>,
    pub graph: Option<GraphDecl>,
    pub subgraphs: Vec<SubgraphDecl>,
    pub definitions: Vec<NodeDefinitionBlock>,
}

/// PCBPnR Phase 5-D §2.2. A named reusable subgraph fragment,
/// exported by its containing file and addressable from importers
/// via the splice syntax `<Subsystem ref="<alias>:<name>" id="..."/>`.
///
/// Children mirror the shape of `<Graph>` children — `<Component>`,
/// `<Net>`, `<Pin>`, `<Pad>`, nested `<Subsystem>` references, and
/// `<edge>` declarations. Splice resolution prefixes inner ids with
/// the splice's local id so they become addressable as
/// `<local-id>.<inner-id>` from the outer scope.
#[derive(Debug, Clone, PartialEq)]
pub struct SubgraphDecl {
    pub name: String,
    pub children: Vec<GrxNode>,
    pub loc: SourceLoc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphDecl {
    pub id: String,
    pub children: Vec<GrxNode>,
    pub entrypoints: Vec<EntrypointDecl>,
    pub loc: SourceLoc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrypointDecl {
    pub name: String,
    pub target: String,
    pub request: String,
    pub reply: String,
    pub loc: SourceLoc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GrxNode {
    Node(NodeDecl),
    Edge(EdgeDecl),
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeDecl {
    /// Component tag name (e.g. `Counter`, `Logger`).
    pub component: String,
    /// `id` attribute value (separated out from `attrs`).
    pub id: String,
    /// All other attributes.
    pub attrs: AttrMap,
    /// Nested children.
    pub children: Vec<GrxNode>,
    /// `<Foo />` vs `<Foo></Foo>`.
    pub self_closing: bool,
    pub loc: SourceLoc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeDecl {
    pub source: String,
    pub target: String,
    /// All attributes other than `source`/`target`.
    pub attrs: AttrMap,
    pub loc: SourceLoc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportDirective {
    pub src: String,
    /// PCBPnR Phase 5-D §2.1. Optional namespace alias from
    /// `<import src="./rf.grx" as="rf"/>`. When `None`, definitions
    /// plus subgraphs from the imported file flatten into the global
    /// namespace (pre-§2.1 behavior). When `Some(alias)`, every
    /// definition and `<Subgraph name="X"/>` the file exports is
    /// namespaced as `alias:X` and is NOT flattened into the global
    /// namespace. Alias must match `[a-zA-Z_][a-zA-Z0-9_]*` (Rust
    /// identifier style — stricter than `id` because dot-prefix
    /// splice addressing makes hyphens ambiguous in `{expr}` values).
    pub alias: Option<String>,
    pub loc: SourceLoc,
}

impl ImportDirective {
    /// Validate that a candidate alias matches the
    /// `[a-zA-Z_][a-zA-Z0-9_]*` shape required by §2.1.
    /// Returns `true` when the alias is well-formed.
    #[must_use]
    pub fn is_valid_alias(alias: &str) -> bool {
        let mut chars = alias.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeDefinitionBlock {
    /// Tag name = component name.
    pub component: String,
    /// Raw component-body code. Language-specific; see [`Self::lang`].
    /// When [`Self::src`] is `Some`, this is populated by the
    /// multi-file resolver after reading the external body file;
    /// the parser itself leaves it empty in that case.
    pub code: String,
    /// PCBPnR Phase 5-D follow-on (item #2 from session 2026-05-26).
    /// Optional external body source path on a self-closing
    /// definition block: `<Component src="./body.py" lang="python"/>`.
    /// Resolved relative to the importing `.grx` file's directory
    /// by [`crate::multi_file::resolve_multi_file`]. `None` for the
    /// canonical inline-body form `<Component>...code...</Component>`.
    pub src: Option<String>,
    /// Body language. Default is [`ComponentLang::Js`] when no `lang`
    /// attribute is present (preserves backward compatibility with
    /// pre-M13-phase3 fixtures). [`ComponentLang::Python`] routes
    /// through the CPython engine + PyRuntime composition layer.
    pub lang: ComponentLang,
    pub loc: SourceLoc,
}

/// Body language for a [`NodeDefinitionBlock`]. M13-phase3a introduces
/// Python; Go was added at M17 per ADR-0005 (Java→Go pivot supersedes the
/// earlier JVM/V8-at-M17 wording). Future engines extend
/// this enum (M17-phase3a added `Go` per ADR-0005's Java→Go pivot;
/// V8 stays soft-demoted per ADR-0001). Selecting a `lang` other
/// than what the active runtime supports surfaces as a runtime
/// error at mount time, not parse time (the parser is
/// engine-agnostic).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum ComponentLang {
    /// JavaScript / TypeScript body. Routed to the JSC engine.
    #[default]
    Js,
    /// Python body. Routed to the CPython engine via the PyRuntime
    /// composition layer (M13-phase3+).
    Python,
    /// Go body. Routed to the Go engine via the GoRuntime
    /// composition layer (M17-phase3+). Bodies are pre-compiled via
    /// `graphreact-codegen-go` + `go build -buildmode=c-shared`; the
    /// daemon loads the artifact through `libloading` and dispatches
    /// against the `Engine` trait.
    Go,
}

impl ComponentLang {
    /// Lower-case, single-token name suitable for `lang="…"`
    /// attribute values + `IrDefinition` JSON serialisation.
    #[must_use]
    pub const fn as_attr_str(self) -> &'static str {
        match self {
            Self::Js => "js",
            Self::Python => "python",
            Self::Go => "go",
        }
    }

    /// Parse a `lang="…"` attribute value. Returns `None` for
    /// unrecognised values; the parser turns that into a `Parse`
    /// error pointing at the definition block's source loc.
    #[must_use]
    pub fn from_attr_str(value: &str) -> Option<Self> {
        match value {
            "js" | "javascript" | "ts" | "typescript" => Some(Self::Js),
            "python" | "py" => Some(Self::Python),
            "go" | "golang" => Some(Self::Go),
            _ => None,
        }
    }
}

/// Ordered map of attribute key → JSON value (string / bool / number /
/// array / object / null). `serde_json::Map` preserves insertion order
/// (matches TS `Record<string, unknown>` iteration order).
pub type AttrMap = serde_json::Map<String, JsonValue>;

impl GraphDecl {
    /// Walk the graph's children depth-first, yielding each `NodeDecl`
    /// with its parent path (insertion order). Useful when downstream
    /// IR builders need a flat traversal.
    pub fn walk_nodes<'a>(&'a self) -> impl Iterator<Item = WalkedNode<'a>> + 'a {
        let mut stack: Vec<(usize, Vec<&'a str>, &'a [GrxNode])> =
            vec![(0, Vec::new(), &self.children)];
        std::iter::from_fn(move || loop {
            let (depth, path, slice) = stack.last_mut()?;
            if let Some((first, rest)) = slice.split_first() {
                *slice = rest;
                if let GrxNode::Node(n) = first {
                    let full_path: Vec<&'a str> = {
                        let mut p = path.clone();
                        p.push(n.id.as_str());
                        p
                    };
                    let walked = WalkedNode {
                        node: n,
                        depth: *depth,
                        parent_path: full_path.clone(),
                    };
                    let new_depth = *depth + 1;
                    stack.push((new_depth, full_path, &n.children));
                    return Some(walked);
                }
            } else {
                stack.pop();
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct WalkedNode<'a> {
    pub node: &'a NodeDecl,
    pub depth: usize,
    pub parent_path: Vec<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::ImportDirective;

    #[test]
    fn alias_validation_accepts_identifier_shape() {
        for ok in &["rf", "_x", "Mcu", "rf_v2", "_2", "A0"] {
            assert!(ImportDirective::is_valid_alias(ok), "expected `{ok}` valid");
        }
    }

    #[test]
    fn alias_validation_rejects_non_identifier_shape() {
        for bad in &["", "rf-1", "1rf", "rf.x", "rf:x", "rf/x", "rf x"] {
            assert!(
                !ImportDirective::is_valid_alias(bad),
                "expected `{bad}` invalid"
            );
        }
    }
}
