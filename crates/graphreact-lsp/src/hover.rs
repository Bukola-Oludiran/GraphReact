//! Hover-payload builder.
//!
//! Inspects the source text under the cursor, classifies the token
//! (tag name vs attribute name vs style property), and returns a
//! Markdown payload pulled from [`crate::catalog`].

use graphreact_grx_parser::ast::{GrxFile, GrxNode, NodeDecl};
use tower_lsp_server::ls_types::{
    Hover, HoverContents, MarkupContent, MarkupKind, Position, Range,
};

use crate::catalog::{
    attribute_markdown, clip_path_shape_markdown, clip_path_shapes, style_properties,
    style_property_markdown, tag_markdown,
};
use crate::position::{loc_to_position, token_range_at};

/// Classification of what the cursor points at — drives hover doc
/// selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverTarget {
    Tag(String),
    Attribute { tag: String, attr: String },
    StyleProperty(String),
    ClipPathShape(String),
    SubsystemRef(String),
}

/// Render a hover payload for the token at `pos` in `source`. Returns
/// `None` when the cursor is on whitespace, inside a body block, or
/// otherwise outside the hover-able surface.
#[must_use]
pub fn build_hover(file: &GrxFile, source: &str, pos: Position) -> Option<Hover> {
    let target = classify_hover_target(file, source, pos)?;
    let (markdown, range) = match target {
        HoverTarget::Tag(tag) => {
            let blurb = tag_markdown(&tag)
                .map(str::to_string)
                .unwrap_or_else(|| format!("**`<{tag}>`** — user-defined component.\n\nNo built-in docs."));
            (blurb, word_range_at(source, pos))
        }
        HoverTarget::Attribute { tag, attr } => {
            (attribute_markdown(&tag, &attr), word_range_at(source, pos))
        }
        HoverTarget::StyleProperty(prop) => {
            (style_property_markdown(&prop), word_range_at(source, pos))
        }
        HoverTarget::ClipPathShape(shape) => {
            (clip_path_shape_markdown(&shape), word_range_at(source, pos))
        }
        HoverTarget::SubsystemRef(target_name) => (
            format!(
                "**`<Subsystem ref=\"{target_name}\">`** — splice target. Goto-definition (`gd`) jumps to the matching `<Subgraph>` declaration."
            ),
            word_range_at(source, pos),
        ),
    };
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(range),
    })
}

/// Classify the token at `pos`. Public so the test harness can
/// exercise classification independently of the LSP wire layer.
#[must_use]
pub fn classify_hover_target(file: &GrxFile, source: &str, pos: Position) -> Option<HoverTarget> {
    let line_text = nth_line(source, pos.line as usize)?;
    let col = pos.character as usize;
    // Inside an attribute value? Detect by walking back to find the
    // last `"` before the cursor and the last `=` before that.
    if let Some(target) = classify_inside_string(line_text, col) {
        return Some(target);
    }
    let word = word_at(line_text, col)?;
    // Tag-name match: search the parser-side AST for a NodeDecl with
    // matching component name OR a structural tag. Open-vocab tags
    // still produce a hover (with fallback markdown).
    if let Some(tag) = look_up_tag(file, &word) {
        return Some(HoverTarget::Tag(tag));
    }
    // Attribute-name match: if the word is preceded on the same line
    // by a `<TagName ...` prefix, we can attribute it.
    if let Some(tag) = preceding_tag(line_text, col) {
        // Only return Attribute if the word looks like an attr (i.e.
        // followed by `=` on the line). Be lenient otherwise.
        if word_is_attribute_shape(line_text, col, &word) {
            return Some(HoverTarget::Attribute { tag, attr: word });
        }
    }
    // Fallback: treat as a generic tag-like token (rare).
    None
}

fn classify_inside_string(line_text: &str, col: usize) -> Option<HoverTarget> {
    // Find the enclosing `"…"` if any.
    let bytes = line_text.as_bytes();
    if col > bytes.len() {
        return None;
    }
    let mut quote_open: Option<usize> = None;
    let mut count = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'"' {
            count += 1;
            if i < col {
                quote_open = if count % 2 == 1 { Some(i) } else { None };
            }
        }
    }
    let quote_open = quote_open?;
    // Find the matching close quote.
    let close = bytes[quote_open + 1..]
        .iter()
        .position(|&b| b == b'"')
        .map(|n| quote_open + 1 + n)?;
    if col < quote_open || col > close {
        return None;
    }
    let inner = &line_text[quote_open + 1..close];
    // Look back for `attr=` so we know which attribute hosts this
    // string.
    let prefix = &line_text[..quote_open];
    let eq_pos = prefix.rfind('=')?;
    let before_eq = &prefix[..eq_pos];
    let attr_name = before_eq
        .rsplit(|c: char| c.is_whitespace())
        .next()?
        .to_string();
    match attr_name.as_str() {
        "ref" => Some(HoverTarget::SubsystemRef(inner.to_string())),
        "style" => {
            // Inside the `style=` string, pick the property under the
            // cursor.
            let inner_col = col - (quote_open + 1);
            classify_inside_style(inner, inner_col)
        }
        _ => None,
    }
}

fn classify_inside_style(style: &str, col: usize) -> Option<HoverTarget> {
    // Walk semicolon-separated declarations; find which one the col
    // is in.
    let mut start = 0usize;
    for (i, ch) in style.char_indices() {
        if ch == ';' {
            let end = i;
            if col >= start && col <= end {
                return prop_or_shape_in_decl(&style[start..end]);
            }
            start = i + 1;
        }
    }
    if col >= start {
        return prop_or_shape_in_decl(&style[start..]);
    }
    None
}

fn prop_or_shape_in_decl(decl: &str) -> Option<HoverTarget> {
    let trimmed = decl.trim();
    if trimmed.is_empty() {
        return None;
    }
    let colon = trimmed.find(':')?;
    let prop = trimmed[..colon].trim().to_lowercase();
    let value = trimmed[colon + 1..].trim();
    // If the value starts with a known clip-path shape function, hover
    // that instead of the bare property name.
    if prop == "clip-path" {
        for shape in clip_path_shapes() {
            if value.starts_with(shape) {
                return Some(HoverTarget::ClipPathShape((*shape).to_string()));
            }
        }
    }
    if style_properties().iter().any(|p| **p == prop) {
        return Some(HoverTarget::StyleProperty(prop));
    }
    None
}

fn nth_line(source: &str, n: usize) -> Option<&str> {
    source.lines().nth(n)
}

fn word_at(line: &str, col: usize) -> Option<String> {
    let bytes = line.as_bytes();
    if col > bytes.len() {
        return None;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.');
    let mut start = col;
    while start > 0 && is_word(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < bytes.len() && is_word(bytes[end]) {
        end += 1;
    }
    if start == end {
        None
    } else {
        Some(line[start..end].to_string())
    }
}

/// Look up the closest `<TagName` on the same line preceding `col`.
fn preceding_tag(line: &str, col: usize) -> Option<String> {
    let prefix = &line[..col.min(line.len())];
    let lt = prefix.rfind('<')?;
    let after_lt = &prefix[lt + 1..];
    // Skip any `/` (closing tag).
    let after_lt = after_lt.strip_prefix('/').unwrap_or(after_lt);
    // Capture until whitespace, `>`, or `/`.
    let end = after_lt
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(after_lt.len());
    let name = after_lt[..end].to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn word_is_attribute_shape(line: &str, col: usize, word: &str) -> bool {
    let bytes = line.as_bytes();
    let mut end = col;
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-' || bytes[end] == b'_')
    {
        end += 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    end < bytes.len() && bytes[end] == b'=' && !word.is_empty()
}

/// Look up a tag by name within the file's NodeDecls (open-vocab); if
/// the parser knows a structural / PCB tag with this name, return it.
fn look_up_tag(file: &GrxFile, word: &str) -> Option<String> {
    if matches!(word, "Graph" | "Subgraph" | "Subsystem" | "import" | "edge") {
        return Some(word.to_string());
    }
    // Walk every NodeDecl looking for a matching component.
    fn walk<'a>(nodes: &'a [GrxNode], word: &str) -> Option<&'a NodeDecl> {
        for n in nodes {
            if let GrxNode::Node(decl) = n {
                if decl.component == word {
                    return Some(decl);
                }
                if let Some(found) = walk(&decl.children, word) {
                    return Some(found);
                }
            }
        }
        None
    }
    if let Some(g) = &file.graph {
        if let Some(decl) = walk(&g.children, word) {
            return Some(decl.component.clone());
        }
    }
    for sg in &file.subgraphs {
        if let Some(decl) = walk(&sg.children, word) {
            return Some(decl.component.clone());
        }
    }
    None
}

fn word_range_at(source: &str, pos: Position) -> Range {
    // Approximate the token range by mapping the position back through
    // the source loc helper.
    let loc = graphreact_grx_parser::SourceLoc::new(
        (pos.line + 1) as usize,
        (pos.character + 1) as usize,
    );
    token_range_at(loc, source)
}

/// Test helper — exposes the SourceLoc → Position conversion so the
/// hover tests can land cursors deterministically.
#[must_use]
pub fn pos_for(line_1idx: usize, col_1idx: usize, source: &str) -> Position {
    loc_to_position(
        graphreact_grx_parser::SourceLoc::new(line_1idx, col_1idx),
        source,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_grx_parser::parser::Parser;

    fn parse(src: &str) -> GrxFile {
        Parser::parse_file(src).expect("parses")
    }

    #[test]
    fn classifies_tag_name() {
        let src = "<Graph id=\"g\"><Counter id=\"c\"/></Graph>";
        let file = parse(src);
        // Hover over 'C' in 'Counter' on line 1, col 16 (1-indexed).
        let pos = pos_for(1, 16, src);
        let t = classify_hover_target(&file, src, pos).expect("classified");
        match t {
            HoverTarget::Tag(t) => assert_eq!(t, "Counter"),
            other => panic!("expected Tag, got {other:?}"),
        }
    }

    #[test]
    fn classifies_attribute_name() {
        // The parser requires `<Graph>` to host children, so build the
        // fixture in the proper open-close form.
        let src = "<Graph id=\"g\"></Graph>";
        let file = parse(src);
        // 'id' starts at col 8 (1-indexed).
        let pos = pos_for(1, 9, src);
        let t = classify_hover_target(&file, src, pos).expect("classified");
        assert!(matches!(t, HoverTarget::Attribute { ref attr, .. } if attr == "id"));
    }

    #[test]
    fn classifies_style_property_inside_string() {
        let src = "<Graph id=\"g\"><C id=\"x\" style=\"left:30mm; top:5mm\"/></Graph>";
        let file = parse(src);
        // Find the column of 'l' in 'left' inside the style string.
        let col = src.find("left").unwrap() + 1;
        let pos = pos_for(1, col + 1, src);
        let t = classify_hover_target(&file, src, pos).expect("classified");
        assert!(matches!(t, HoverTarget::StyleProperty(ref p) if p == "left"));
    }

    #[test]
    fn classifies_subsystem_ref() {
        let src = "<Subgraph name=\"X\"><A id=\"a\"/></Subgraph><Graph id=\"g\"><Subsystem ref=\"X\" id=\"x\"/></Graph>";
        let file = parse(src);
        // Cursor inside the `ref="X"` string.
        let col = src.find("ref=\"X\"").unwrap() + 5; // inside the 'X'
        let pos = pos_for(1, col + 1, src);
        let t = classify_hover_target(&file, src, pos).expect("classified");
        assert!(matches!(t, HoverTarget::SubsystemRef(ref r) if r == "X"));
    }
}
