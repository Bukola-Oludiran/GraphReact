//! Phase 4 integration — `textDocument/definition` resolves
//! `<Subsystem ref="…">` against in-file subgraphs and cross-file
//! aliased imports.

use std::path::PathBuf;

use graphreact_grx_parser::multi_file::resolve_multi_file;
use graphreact_lsp::resolve::{resolve_import_alias, resolve_subsystem_target};

fn fixture_path(rel: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push(rel);
    p
}

#[test]
fn in_file_subsystem_ref_resolves_to_subgraph_loc() {
    let path = fixture_path("splice-root.grx");
    let resolved = resolve_multi_file(&path).expect("resolves");
    let target =
        resolve_subsystem_target(&resolved, "ShelfA").expect("in-file subsystem ref resolves");
    assert_eq!(target.subgraph_name, "ShelfA");
    assert_eq!(target.file_path, path.canonicalize().unwrap());
    assert!(target.loc.line >= 1);
}

#[test]
fn aliased_subsystem_ref_resolves_into_imported_file() {
    let path = fixture_path("multi-file/root.grx");
    let resolved = resolve_multi_file(&path).expect("resolves");
    let target =
        resolve_subsystem_target(&resolved, "rf:Bandpass").expect("aliased subsystem ref resolves");
    assert_eq!(target.subgraph_name, "Bandpass");
    let expected_path = fixture_path("multi-file/shelf.grx").canonicalize().unwrap();
    assert_eq!(target.file_path, expected_path);
}

#[test]
fn unknown_ref_returns_none() {
    let path = fixture_path("splice-root.grx");
    let resolved = resolve_multi_file(&path).expect("resolves");
    assert!(resolve_subsystem_target(&resolved, "DoesNotExist").is_none());
}

#[test]
fn alias_lookup_returns_imported_file() {
    let path = fixture_path("multi-file/root.grx");
    let resolved = resolve_multi_file(&path).expect("resolves");
    let target = resolve_import_alias(&resolved, "rf").expect("alias resolves");
    let expected = fixture_path("multi-file/shelf.grx").canonicalize().unwrap();
    assert_eq!(target.file_path, expected);
}
