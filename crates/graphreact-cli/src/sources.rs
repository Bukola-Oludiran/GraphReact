//! Rust-native event source factories for the standalone `grx` CLI —
//! HTTP server and filesystem watcher. Mirrors
//! [`graphreact_napi::sources`] but lives here so the CLI doesn't pull
//! the napi crate (which exists only to expose the runtime as a Node
//! addon). Both implementations satisfy [`graphreact_core::EventSource`]
//! and plug into `core::use_source` via the JSC `host_use_source`
//! decoder.
//!
//! Per [`feedback_feature_vs_line_parity`](memory): feature parity with
//! the TS reference, not implementation parity. The TS originals shimmed
//! `node:http` / `node:fs`; that defeats the M12 standalone CLI vision
//! (raw JSC, no Node). These are Rust-native (`tokio::net`, `notify`)
//! all the way down. The grx binary's `main()` installs a tokio runtime
//! so `tokio::spawn` inside `start()` finds a context.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use graphreact_core::EventSource;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

static SOURCE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_id(prefix: &str) -> String {
    let n = SOURCE_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{prefix}-{n}")
}

// ---------------------------------------------------------------------------
// HTTP source
// ---------------------------------------------------------------------------

/// Create an HTTP-server event source that emits a `request` message
/// for each incoming request.
///
/// The server binds `127.0.0.1:port` on `start` and accepts HTTP/1
/// connections. For each request, a `request` message fires with
/// `{ method, url, headers, body }`. Responses are a fixed
/// `{"status":"ok"}` JSON body — sources are firehoses; rich response
/// shaping belongs in the userland handler that invokes outbound
/// effects, not in the source itself.
///
/// **Scope of the parser:** minimal HTTP/1.1 — request line + headers +
/// Content-Length-bounded body. Sufficient for graph node ingestion;
/// not a production HTTP framework. Chunked transfer encoding,
/// keep-alive across requests, HTTP/2, and pipelining are out of scope.
/// Bind failures and parse errors log to stderr with a `[graphreact-cli]`
/// prefix.
#[must_use]
pub fn http_source(port: u16) -> Arc<dyn EventSource> {
    Arc::new(HttpSource {
        id: next_id("http"),
        port,
        handle: parking_lot::Mutex::new(None),
    })
}

struct HttpSource {
    id: String,
    port: u16,
    handle: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

impl EventSource for HttpSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self, push: Arc<dyn Fn(&str, Value) + Send + Sync>) {
        let port = self.port;
        let handle = tokio::spawn(async move {
            let listener = match TcpListener::bind(("127.0.0.1", port)).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[graphreact-cli] http source bind failed on 127.0.0.1:{port}: {e}");
                    return;
                }
            };
            loop {
                let (socket, _addr) = match listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[graphreact-cli] http source accept failed: {e}");
                        continue;
                    }
                };
                let push_for_conn = Arc::clone(&push);
                tokio::spawn(handle_connection(socket, push_for_conn));
            }
        });
        *self.handle.lock() = Some(handle);
    }

    fn stop(&self) {
        if let Some(h) = self.handle.lock().take() {
            h.abort();
        }
    }
}

/// Body delivered over a single HTTP/1 connection. Reads the request
/// line + headers, then Content-Length-bounded body. Pushes a
/// `request` message and writes a fixed JSON response.
async fn handle_connection(
    mut socket: tokio::net::TcpStream,
    push: Arc<dyn Fn(&str, Value) + Send + Sync>,
) {
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = match socket.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_double_crlf(&buf) {
            header_end = idx + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\n\r\n",
                )
                .await;
            return;
        }
    }

    let header_text = match std::str::from_utf8(&buf[..header_end]) {
        Ok(s) => s,
        Err(_) => {
            let _ = socket
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }
    };
    let mut lines = header_text.split("\r\n");
    let request_line = match lines.next() {
        Some(l) => l,
        None => return,
    };
    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("").to_string();
    let url = parts.next().unwrap_or("").to_string();

    let mut headers = serde_json::Map::new();
    let mut content_length: usize = 0;
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name_lower = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name_lower == "content-length" {
            content_length = value.parse().unwrap_or(0);
        }
        headers.insert(name_lower, Value::String(value));
    }

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let want = content_length - body.len();
        let mut chunk = vec![0u8; want.min(8 * 1024)];
        let n = match socket.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return,
        };
        body.extend_from_slice(&chunk[..n]);
    }
    let body_str = String::from_utf8_lossy(&body).to_string();
    let body_value = if body_str.is_empty() {
        Value::Null
    } else {
        Value::String(body_str)
    };

    push(
        "request",
        json!({
            "method": method,
            "url": url,
            "headers": Value::Object(headers),
            "body": body_value,
        }),
    );

    // 15-byte body ({"status":"ok"}); Content-Length must match exactly
    // or strict HTTP clients raise content-length-mismatch errors.
    let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"status\":\"ok\"}";
    let _ = socket.write_all(response).await;
    let _ = socket.shutdown().await;
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

// ---------------------------------------------------------------------------
// File watch source
// ---------------------------------------------------------------------------

/// Create a filesystem-watch event source that emits a `change` message
/// for each filesystem event under `path`.
///
/// Backed by [`notify::RecommendedWatcher`] (FSEvents on macOS, inotify
/// on Linux, ReadDirectoryChangesW on Windows). Watch is recursive.
/// Events surface as `{ kind, paths: string[] }` where `kind` is the
/// debug-formatted notify event kind (e.g., `"Modify(Data(Content))"`).
///
/// notify drives its own thread internally, so this source does not
/// depend on a tokio runtime.
#[must_use]
pub fn watch_source(path: String) -> Arc<dyn EventSource> {
    Arc::new(WatchSource {
        id: next_id("watch"),
        path,
        watcher: parking_lot::Mutex::new(None),
    })
}

struct WatchSource {
    id: String,
    path: String,
    watcher: parking_lot::Mutex<Option<RecommendedWatcher>>,
}

impl EventSource for WatchSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self, push: Arc<dyn Fn(&str, Value) + Send + Sync>) {
        let push_for_handler = Arc::clone(&push);
        let mut watcher =
            match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else {
                    return;
                };
                let paths: Vec<String> = event
                    .paths
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                push_for_handler(
                    "change",
                    json!({
                        "kind": format!("{:?}", event.kind),
                        "paths": paths,
                    }),
                );
            }) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("[graphreact-cli] watch source init failed: {e}");
                    return;
                }
            };
        if let Err(e) = watcher.watch(std::path::Path::new(&self.path), RecursiveMode::Recursive) {
            eprintln!(
                "[graphreact-cli] watch source watch({}) failed: {e}",
                self.path
            );
            return;
        }
        *self.watcher.lock() = Some(watcher);
    }

    fn stop(&self) {
        self.watcher.lock().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_source_handle_id_is_prefixed() {
        let s = http_source(0);
        assert!(
            s.id().starts_with("http-"),
            "expected `http-` prefix, got {}",
            s.id()
        );
    }

    #[test]
    fn watch_source_handle_id_is_prefixed() {
        let s = watch_source("/tmp".to_string());
        assert!(
            s.id().starts_with("watch-"),
            "expected `watch-` prefix, got {}",
            s.id()
        );
    }
}
