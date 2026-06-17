//! Phase 3 integration — hover payload shape.

use graphreact_grx_parser::parser::Parser;
use graphreact_lsp::hover::{build_hover, classify_hover_target, pos_for, HoverTarget};
use tower_lsp_server::ls_types::HoverContents;

const FIXTURE: &str = include_str!("fixtures/well-formed.grx");

#[test]
fn hover_over_counter_tag_returns_markdown() {
    let file = Parser::parse_file(FIXTURE).expect("parses");
    // 'C' in 'Counter' on the first <Counter id="a" .../> line.
    let line_no = FIXTURE
        .lines()
        .position(|l| l.contains("<Counter id=\"a\""))
        .unwrap()
        + 1;
    let line_text = FIXTURE.lines().nth(line_no - 1).unwrap();
    let col = line_text.find("Counter").unwrap() + 1; // 1-indexed
    let pos = pos_for(line_no, col + 1, FIXTURE);

    let target = classify_hover_target(&file, FIXTURE, pos).expect("classified");
    match target {
        HoverTarget::Tag(t) => assert_eq!(t, "Counter"),
        other => panic!("expected Tag, got {other:?}"),
    }

    let hover = build_hover(&file, FIXTURE, pos).expect("hover");
    match hover.contents {
        HoverContents::Markup(m) => {
            assert!(m.value.contains("Counter"));
        }
        other => panic!("expected markup, got {other:?}"),
    }
}

#[test]
fn hover_over_subsystem_ref_returns_splice_target_text() {
    let src = include_str!("fixtures/splice-root.grx");
    let file = Parser::parse_file(src).expect("parses");
    let line_no = src
        .lines()
        .position(|l| l.contains("<Subsystem"))
        .unwrap()
        + 1;
    let line_text = src.lines().nth(line_no - 1).unwrap();
    let col = line_text.find("ShelfA").unwrap() + 2; // inside the string
    let pos = pos_for(line_no, col + 1, src);

    let target = classify_hover_target(&file, src, pos).expect("classified");
    match target {
        HoverTarget::SubsystemRef(r) => assert_eq!(r, "ShelfA"),
        other => panic!("expected SubsystemRef, got {other:?}"),
    }
}
