//! In-memory document store backing the LSP `textDocument/*`
//! lifecycle.
//!
//! The server keeps every open document in memory keyed by `Uri`. On
//! `didOpen` the source is registered; on `didChange` the live text is
//! replaced (full sync for the MVP); on `didClose` the entry is
//! dropped. The store is the source of truth for unsaved buffers
//! that the multi-file resolver needs to see.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp_server::ls_types::Uri;

/// Holds a single document's text plus version. We keep version so
/// stale diagnostics can be discarded if a newer revision arrives
/// while a parse is in flight.
#[derive(Debug, Clone)]
pub struct DocumentEntry {
    pub text: String,
    pub version: i32,
}

/// Thread-safe document map shared by every request handler. Backed
/// by an `RwLock` because the LSP runtime fans requests out across
/// many tokio tasks.
#[derive(Debug, Default, Clone)]
pub struct DocumentStore {
    inner: Arc<RwLock<HashMap<String, DocumentEntry>>>,
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the document at `uri`.
    pub async fn upsert(&self, uri: &Uri, text: String, version: i32) {
        let mut guard = self.inner.write().await;
        guard.insert(uri.as_str().to_string(), DocumentEntry { text, version });
    }

    /// Drop a document — called from `didClose`.
    pub async fn remove(&self, uri: &Uri) {
        let mut guard = self.inner.write().await;
        guard.remove(uri.as_str());
    }

    /// Read a document by `Uri`. Returns the live copy so handlers can
    /// avoid holding the lock across awaits.
    pub async fn get(&self, uri: &Uri) -> Option<DocumentEntry> {
        let guard = self.inner.read().await;
        guard.get(uri.as_str()).cloned()
    }

    /// Look up an entry by raw filesystem path (the multi-file
    /// resolver hands us `&Path` references; we need to translate to
    /// a `file://` URI lookup).
    pub async fn get_by_path(&self, path: &Path) -> Option<DocumentEntry> {
        let uri = path_to_uri_string(path)?;
        let guard = self.inner.read().await;
        guard.get(&uri).cloned()
    }

    /// Drain a snapshot of every open URI. Used by integration tests
    /// to assert the lifecycle, never on the hot path.
    pub async fn snapshot_uris(&self) -> Vec<String> {
        let guard = self.inner.read().await;
        let mut keys: Vec<String> = guard.keys().cloned().collect();
        keys.sort();
        keys
    }
}

/// Build a `file://` URI string from a filesystem path. Mirrors what
/// editors produce, so the document map key spaces line up.
pub fn path_to_uri_string(path: &Path) -> Option<String> {
    // Canonicalise so resolver-side `canonicalize` results agree with
    // the URIs LSP clients send.
    let canonical = std::fs::canonicalize(path).ok()?;
    let canonical_str = canonical.to_str()?;
    if cfg!(windows) {
        // Best-effort on Windows: prepend an extra slash and replace
        // back-slashes. The integration tests only target unix so this
        // path is unexercised in CI today.
        Some(format!(
            "file:///{}",
            canonical_str.replace('\\', "/")
        ))
    } else {
        Some(format!("file://{canonical_str}"))
    }
}

/// Convert an LSP `Uri` to a filesystem `PathBuf`. Returns `None` for
/// non-`file://` schemes (e.g. `untitled:` buffers — those live in the
/// in-memory store but cannot be resolved through the multi-file
/// path-based resolver).
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let raw = uri.as_str();
    let path = raw.strip_prefix("file://")?;
    // Tolerate the leading `/` on Windows file URIs.
    let path = if cfg!(windows) {
        path.strip_prefix('/').unwrap_or(path)
    } else {
        path
    };
    Some(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_uri(s: &str) -> Uri {
        s.parse().expect("valid uri")
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips() {
        let store = DocumentStore::new();
        let uri = make_uri("file:///tmp/a.grx");
        store
            .upsert(&uri, "<Graph id=\"g\" />".to_string(), 1)
            .await;
        let got = store.get(&uri).await.expect("found");
        assert_eq!(got.text, "<Graph id=\"g\" />");
        assert_eq!(got.version, 1);
    }

    #[tokio::test]
    async fn remove_clears_entry() {
        let store = DocumentStore::new();
        let uri = make_uri("file:///tmp/a.grx");
        store.upsert(&uri, "x".to_string(), 1).await;
        store.remove(&uri).await;
        assert!(store.get(&uri).await.is_none());
    }

    #[test]
    fn uri_to_path_strips_scheme() {
        let uri: Uri = "file:///tmp/a.grx".parse().unwrap();
        let p = uri_to_path(&uri).expect("path");
        assert_eq!(p.to_str(), Some("/tmp/a.grx"));
    }
}
