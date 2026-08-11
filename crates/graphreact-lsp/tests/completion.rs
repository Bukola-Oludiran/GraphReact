//! Phase 5 integration — completion lane classification + payload.

#![allow(clippy::cast_possible_truncation)]

use graphreact_grx_parser::parser::Parser;
use graphreact_lsp::completion::{
    build_completions, classify_completion_context, CompletionContext,
};
use tower_lsp_server::ls_types::Position;

#[test]
fn tag_completion_pulls_open_vocab_tags_from_ir() {
    let src = "<Graph id=\"g\"><Counter id=\"a\"/><Logger id=\"l\"/></Graph>";
    let file = Parser::parse_file(src).expect("parses");
    // Cursor right after a `<` mid-text.
    let mut probe = src.to_string();
    probe.push_str("\n<");
    let pos = Position::new(1, 1);
    let items = build_completions(&file, &probe, pos);
    let names: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
    assert!(
        names.iter().any(|n| n == "Counter"),
        "expected Counter; got {names:?}"
    );
    assert!(names.iter().any(|n| n == "Logger"), "expected Logger");
    // Built-ins from the static catalog also appear.
    assert!(names.iter().any(|n| n == "Graph"));
    assert!(names.iter().any(|n| n == "Subsystem"));
}

#[test]
fn attribute_completion_returns_per_tag_attrs() {
    let src = "<Graph ";
    let file = Parser::parse_file("<Graph id=\"g\"/>").unwrap_or_default();
    let pos = Position::new(0, src.len() as u32);
    let ctx = classify_completion_context(src, pos);
    assert!(matches!(ctx, CompletionContext::AttributeName { ref tag } if tag == "Graph"));
    let items = build_completions(&file, src, pos);
    let names: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
    assert!(
        names.iter().any(|n| n == "id"),
        "expected id; got {names:?}"
    );
}

#[test]
fn style_property_completion_includes_left_top_clip_path() {
    let src = "<Component style=\"";
    let file = Parser::parse_file("<Graph id=\"g\"><Component id=\"c\"/></Graph>").unwrap();
    let pos = Position::new(0, src.len() as u32);
    let ctx = classify_completion_context(src, pos);
    assert_eq!(ctx, CompletionContext::StyleProperty);
    let items = build_completions(&file, src, pos);
    let names: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
    for required in ["left", "top", "rotate", "clip-path"] {
        assert!(
            names.iter().any(|n| n == required),
            "expected `{required}`; got {names:?}"
        );
    }
}

#[test]
fn clip_path_shape_completion_includes_inset_polygon_circle() {
    let src = "<Pad style=\"clip-path: ";
    let file = Parser::parse_file("<Graph id=\"g\"><Pad id=\"p\"/></Graph>").unwrap();
    let pos = Position::new(0, src.len() as u32);
    let ctx = classify_completion_context(src, pos);
    assert_eq!(ctx, CompletionContext::ClipPathShape);
    let items = build_completions(&file, src, pos);
    let names: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
    for required in ["inset", "circle", "polygon"] {
        assert!(
            names.iter().any(|n| n == required),
            "expected `{required}`; got {names:?}"
        );
    }
}
