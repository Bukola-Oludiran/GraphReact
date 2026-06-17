//! In-memory document store backing the LSP `textDocument/*`
//! lifecycle.
//!
//! The server keeps every open document in memory keyed by `Uri`. On
//! `didOpen` the source is registered; on `didChange` the live text is
//! replaced (full sync for the MVP); on `didClose` the entry is
//! dropped. The store is the source of truth for unsaved buffers
//! that the multi-file resolver needs to see.

use std::collections::HashMap;
use std::fmt::Write as _;
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
///
/// Percent-encodes path segments per RFC 3986 so a path like
/// `/Users/jane/My Project/foo.grx` becomes
/// `file:///Users/jane/My%20Project/foo.grx`, matching what an LSP
/// client sends as the document URI. Without this, the resolver-side
/// canonicalised path key would never agree with the LSP-side URI key
/// for any path containing spaces or non-ASCII characters.
pub fn path_to_uri_string(path: &Path) -> Option<String> {
    // Canonicalise so resolver-side `canonicalize` results agree with
    // the URIs LSP clients send.
    let canonical = std::fs::canonicalize(path).ok()?;
    let canonical_str = canonical.to_str()?;
    let encoded = percent_encode_path(canonical_str);
    if cfg!(windows) {
        // Best-effort on Windows: prepend an extra slash and replace
        // back-slashes. The integration tests only target unix so this
        // path is unexercised in CI today.
        Some(format!("file:///{}", encoded.replace('\\', "/")))
    } else {
        Some(format!("file://{encoded}"))
    }
}

/// Percent-encode a filesystem path for embedding in a `file://` URI.
/// Keeps `/` (segment separator), unreserved RFC-3986 characters
/// (`A-Za-z0-9-_.~`), and on Windows the drive-colon `:` unescaped.
/// Everything else is `%HH` byte-encoded.
fn percent_encode_path(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '/' | 'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            ':' if cfg!(windows) => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                for &b in ch.encode_utf8(&mut buf).as_bytes() {
                    // Writing to a String is infallible; the `_ =`
                    // discards the Result without an unwrap.
                    let _ = write!(out, "%{b:02X}");
                }
            }
        }
    }
    out
}

/// Convert an LSP `Uri` to a filesystem `PathBuf`. Returns `None` for
/// non-`file://` schemes (e.g. `untitled:` buffers — those live in the
/// in-memory store but cannot be resolved through the multi-file
/// path-based resolver).
///
/// Percent-decodes per RFC 3986: LSP clients encode reserved characters
/// (spaces → `%20`, non-ASCII → `%C3%A9` etc.) in `DocumentUri`. A
/// literal `strip_prefix("file://")` leaves the encoded bytes in the
/// path, which `fs::canonicalize` then fails to find, silently breaking
/// cross-file goto-definition on any project under a path with spaces
/// or non-ASCII characters.
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let raw = uri.as_str();
    let encoded = raw.strip_prefix("file://")?;
    // Tolerate the leading `/` on Windows file URIs (`file:///C:/...`).
    let encoded = if cfg!(windows) {
        encoded.strip_prefix('/').unwrap_or(encoded)
    } else {
        encoded
    };
    let decoded = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()?;
    Some(PathBuf::from(decoded.as_ref()))
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

    #[test]
    fn uri_to_path_decodes_percent_escapes() {
        // LSP clients percent-encode spaces; uri_to_path must reverse
        // it so the path round-trips for fs::canonicalize and for
        // get_by_path lookups against the document store.
        let uri: Uri = "file:///Users/jane/My%20Project/foo.grx".parse().unwrap();
        let p = uri_to_path(&uri).expect("path");
        assert_eq!(p.to_str(), Some("/Users/jane/My Project/foo.grx"));
    }

    #[test]
    fn uri_to_path_decodes_non_ascii() {
        // `caf%C3%A9` is the percent-encoded form of "café".
        let uri: Uri = "file:///srv/caf%C3%A9/notes.grx".parse().unwrap();
        let p = uri_to_path(&uri).expect("path");
        assert_eq!(p.to_str(), Some("/srv/café/notes.grx"));
    }

    #[test]
    fn percent_encode_path_round_trips_with_decode() {
        let raw = "/Users/jane/My Project/café/foo.grx";
        let encoded = percent_encode_path(raw);
        assert!(encoded.contains("My%20Project"));
        assert!(encoded.contains("caf%C3%A9"));
        let decoded = percent_encoding::percent_decode_str(&encoded)
            .decode_utf8()
            .unwrap();
        assert_eq!(decoded, raw);
    }
}
