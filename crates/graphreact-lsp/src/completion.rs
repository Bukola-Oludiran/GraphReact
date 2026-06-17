//! Completion-payload builder.
//!
//! Picks one of four completion lanes based on the cursor context:
//!
//! 1. Inside `<…` before any whitespace → tag-name completion (pulls
//!    structural + PCB built-ins + open-vocab tags seen in the IR).
//! 2. After a tag name + whitespace → attribute-name completion per
//!    the [`crate::catalog::attributes_for_tag`] table.
//! 3. Inside `style="…"` at a property-name slot →
//!    [`crate::catalog::style_properties`].
//! 4. Inside `style="… clip-path: "` at the function name slot →
//!    [`crate::catalog::clip_path_shapes`].

use graphreact_grx_parser::ast::{GrxFile, GrxNode};
use std::collections::BTreeSet;
use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, Documentation, MarkupContent, MarkupKind, Position,
};

use crate::catalog::{
    attribute_markdown, attributes_for_tag, clip_path_shape_markdown, clip_path_shapes,
    style_properties, style_property_markdown, tag_markdown, PCB_TAGS, STRUCTURAL_TAGS,
};

/// Cursor classification for completion. The LSP server hands each
/// `(file, source, pos)` to [`build_completions`]; the helpers here
/// are also exposed for unit tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    TagName,
    AttributeName { tag: String },
    StyleProperty,
    ClipPathShape,
    None,
}

/// Build the completion list for the given position.
#[must_use]
pub fn build_completions(
    file: &GrxFile,
    source: &str,
    pos: Position,
) -> Vec<CompletionItem> {
    match classify_completion_context(source, pos) {
        CompletionContext::TagName => tag_completions(file),
        CompletionContext::AttributeName { tag } => attribute_completions(&tag),
        CompletionContext::StyleProperty => style_completions(),
        CompletionContext::ClipPathShape => clip_path_completions(),
        CompletionContext::None => Vec::new(),
    }
}

/// Pure classifier exposed for tests.
#[must_use]
pub fn classify_completion_context(source: &str, pos: Position) -> CompletionContext {
    let Some(line) = nth_line(source, pos.line as usize) else {
        return CompletionContext::None;
    };
    let col = (pos.character as usize).min(line.len());
    let prefix = &line[..col];

    // Quoted-string completions take priority.
    if let Some(ctx) = classify_in_string(prefix) {
        return ctx;
    }

    // Find the nearest `<` before the cursor on this line.
    let Some(lt) = prefix.rfind('<') else {
        return CompletionContext::None;
    };
    let after_lt = &prefix[lt + 1..];
    if after_lt.starts_with('/') {
        // Closing tag — no completion.
        return CompletionContext::None;
    }
    // Determine if we're still typing the tag name or past it.
    if let Some(space_idx) = after_lt.find(|c: char| c.is_whitespace()) {
        // Past the tag name — we're somewhere in the attribute area.
        let tag = after_lt[..space_idx].to_string();
        if tag.is_empty() {
            return CompletionContext::None;
        }
        // Only return AttributeName when the immediate-left context is
        // whitespace (i.e. we're not in a value string — already
        // handled above).
        return CompletionContext::AttributeName { tag };
    }
    // No whitespace yet → still in the tag name slot.
    CompletionContext::TagName
}

fn classify_in_string(prefix: &str) -> Option<CompletionContext> {
    // Determine if we're currently inside an unterminated `"…` string.
    let mut open: Option<usize> = None;
    let mut count = 0usize;
    for (i, b) in prefix.bytes().enumerate() {
        if b == b'"' {
            count += 1;
            open = if count % 2 == 1 { Some(i) } else { None };
        }
    }
    let open = open?;
    let before = &prefix[..open];
    let eq = before.rfind('=')?;
    let before_eq = &before[..eq];
    let attr_name = before_eq
        .rsplit(|c: char| c.is_whitespace())
        .next()?;
    if attr_name == "style" {
        // Inside style="…". Walk forwards from `open + 1` to see
        // whether we're in a property-name slot or after a `:` (value
        // slot, which includes the clip-path shape function).
        let inside = &prefix[open + 1..];
        return Some(classify_inside_style(inside));
    }
    None
}

fn classify_inside_style(inside: &str) -> CompletionContext {
    // Find the start of the current declaration (after the last `;`).
    let last_semi = inside.rfind(';').map(|i| i + 1).unwrap_or(0);
    let current = &inside[last_semi..];
    if let Some(colon_idx) = current.find(':') {
        // We're past the colon — value slot. Check whether the prop
        // is `clip-path`; if so, offer shape functions.
        let prop = current[..colon_idx].trim().to_lowercase();
        if prop == "clip-path" {
            return CompletionContext::ClipPathShape;
        }
        // Other style values are open vocabulary — no completion in
        // the MVP.
        return CompletionContext::None;
    }
    CompletionContext::StyleProperty
}

fn tag_completions(file: &GrxFile) -> Vec<CompletionItem> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for entry in STRUCTURAL_TAGS.iter().chain(PCB_TAGS.iter()) {
        names.insert(entry.name.to_string());
    }
    // Open-vocab: pull every tag seen in the parsed file.
    walk_collect_tags(file, &mut names);

    names
        .into_iter()
        .map(|name| {
            let mut item = CompletionItem::new_simple(name.clone(), String::new());
            item.kind = Some(CompletionItemKind::CLASS);
            if let Some(md) = tag_markdown(&name) {
                item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: md.to_string(),
                }));
            }
            item
        })
        .collect()
}

fn walk_collect_tags(file: &GrxFile, out: &mut BTreeSet<String>) {
    fn walk(nodes: &[GrxNode], out: &mut BTreeSet<String>) {
        for n in nodes {
            if let GrxNode::Node(decl) = n {
                out.insert(decl.component.clone());
                walk(&decl.children, out);
            }
        }
    }
    if let Some(g) = &file.graph {
        walk(&g.children, out);
    }
    for sg in &file.subgraphs {
        walk(&sg.children, out);
    }
}

fn attribute_completions(tag: &str) -> Vec<CompletionItem> {
    attributes_for_tag(tag)
        .iter()
        .map(|attr| {
            let mut item = CompletionItem::new_simple((*attr).to_string(), String::new());
            item.kind = Some(CompletionItemKind::PROPERTY);
            item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: attribute_markdown(tag, attr),
            }));
            item
        })
        .collect()
}

fn style_completions() -> Vec<CompletionItem> {
    style_properties()
        .iter()
        .map(|prop| {
            let mut item = CompletionItem::new_simple((*prop).to_string(), String::new());
            item.kind = Some(CompletionItemKind::PROPERTY);
            item.insert_text = Some(format!("{prop}: "));
            item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: style_property_markdown(prop),
            }));
            item
        })
        .collect()
}

fn clip_path_completions() -> Vec<CompletionItem> {
    clip_path_shapes()
        .iter()
        .map(|shape| {
            let mut item = CompletionItem::new_simple((*shape).to_string(), String::new());
            item.kind = Some(CompletionItemKind::FUNCTION);
            item.insert_text = Some(format!("{shape}()"));
            item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: clip_path_shape_markdown(shape),
            }));
            item
        })
        .collect()
}

fn nth_line(source: &str, n: usize) -> Option<&str> {
    source.lines().nth(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_name_context_inside_open_lt() {
        let src = "<Gr";
        let pos = Position::new(0, 3);
        assert_eq!(classify_completion_context(src, pos), CompletionContext::TagName);
    }

    #[test]
    fn attribute_context_after_tag_space() {
        let src = "<Graph ";
        let pos = Position::new(0, 7);
        let ctx = classify_completion_context(src, pos);
        assert!(matches!(ctx, CompletionContext::AttributeName { ref tag } if tag == "Graph"));
    }

    #[test]
    fn style_property_context_inside_style_string() {
        let src = "<Component style=\"po";
        let pos = Position::new(0, src.len() as u32);
        assert_eq!(
            classify_completion_context(src, pos),
            CompletionContext::StyleProperty
        );
    }

    #[test]
    fn clip_path_shape_context_after_colon() {
        let src = "<Pad style=\"clip-path: ci";
        let pos = Position::new(0, src.len() as u32);
        assert_eq!(
            classify_completion_context(src, pos),
            CompletionContext::ClipPathShape
        );
    }

    #[test]
    fn closing_tag_yields_no_completion() {
        let src = "</Counter";
        let pos = Position::new(0, src.len() as u32);
        assert_eq!(
            classify_completion_context(src, pos),
            CompletionContext::None
        );
    }
}
