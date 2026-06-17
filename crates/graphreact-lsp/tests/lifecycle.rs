//! Phase 1 integration — initialize / didOpen / didChange / didClose
//! cycle exercised against the in-process document store.
//!
//! We deliberately don't spin a real stdio LSP transport here — the
//! lifecycle handlers all delegate to [`DocumentStore`], so testing
//! the store is sufficient to lock in the surface. A wire-level test
//! is folded into `wire_smoke.rs`.

use graphreact_lsp::DocumentStore;
use tower_lsp_server::ls_types::Uri;

fn uri(s: &str) -> Uri {
    s.parse().expect("uri")
}

#[tokio::test]
async fn did_open_did_change_did_close_round_trip() {
    let store = DocumentStore::new();
    let uri = uri("file:///tmp/lifecycle.grx");

    // didOpen
    store
        .upsert(&uri, include_str!("fixtures/well-formed.grx").to_string(), 1)
        .await;
    let opened = store.get(&uri).await.expect("present after open");
    assert_eq!(opened.version, 1);
    assert!(opened.text.contains("Counter"));

    // didChange — bump version, replace text.
    let next = "<Graph id=\"g2\"/>".to_string();
    store.upsert(&uri, next.clone(), 2).await;
    let changed = store.get(&uri).await.expect("present after change");
    assert_eq!(changed.version, 2);
    assert_eq!(changed.text, next);

    // didClose
    store.remove(&uri).await;
    assert!(store.get(&uri).await.is_none());
}

#[tokio::test]
async fn snapshot_lists_every_open_uri() {
    let store = DocumentStore::new();
    store
        .upsert(&uri("file:///tmp/a.grx"), "a".to_string(), 1)
        .await;
    store
        .upsert(&uri("file:///tmp/b.grx"), "b".to_string(), 1)
        .await;
    let mut snap = store.snapshot_uris().await;
    snap.sort();
    assert_eq!(
        snap,
        vec![
            "file:///tmp/a.grx".to_string(),
            "file:///tmp/b.grx".to_string()
        ]
    );
}
