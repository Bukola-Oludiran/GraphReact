//! Rust-native event source factories — HTTP server and filesystem
//! watcher. These live in the napi crate (rather than `graphreact-core`)
//! because their dependencies (`tokio::net`, `notify`) aren't appropriate
//! for the core's compile graph (browser/WASM targets, pure-Rust hosts
//! that don't want network/fs deps).
//!
//! Both implementations satisfy [`graphreact_core::EventSource`] so they
//! plug directly into `core::use_source`. Push events flow through the
//! scheduler's external inbox (cross-thread safe via the scheduler
//! handle the core wires up at registration time) — same path as
//! [`graphreact_core::timer_source`] / [`graphreact_core::delay_source`].
//!
//! M6.6 design note: an earlier iteration shipped JS-side wrappers
//! around `node:http` / `node:fs`. That defeated the M12 standalone
//! CLI vision (JSC-via-Zig, no Node), so this module replaces them with
//! Rust-native equivalents. JS users see the same factories
//! (`httpSource(port)`, `watchSource(path)`) but they're now Rust all
//! the way down.

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
/// Bind failures and parse errors log to stderr (matching the
/// `eprintln!` convention used elsewhere in the napi bridge).
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
                    eprintln!("[graphreact-napi] http source bind failed on 127.0.0.1:{port}: {e}");
                    return;
                }
            };
            loop {
                let (socket, _addr) = match listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[graphreact-napi] http source accept failed: {e}");
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
    // Read until headers end (\r\n\r\n) or buffer fills. 16 KiB is
    // plenty for typical request lines + headers; bodies are streamed
    // separately via Content-Length.
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = match socket.read(&mut tmp).await {
            // Peer closed before headers complete OR read failed.
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_double_crlf(&buf) {
            header_end = idx + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            // Headers too large; drop the connection.
            let _ = socket
                .write_all(
                    b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\n\r\n",
                )
                .await;
            return;
        }
    }

    // Parse request line + headers from `buf[..header_end]`.
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
    // version (parts.next()) ignored — we accept anything and respond HTTP/1.1.

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

    // Body: combine any bytes already in `buf` past `header_end` with
    // additional reads until we have `content_length` total.
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let want = content_length - body.len();
        let mut chunk = vec![0u8; want.min(8 * 1024)];
        let n = match socket.read(&mut chunk).await {
            Ok(0) => break, // EOF before declared Content-Length
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

    // Fixed response body — userland that wants per-request shaping
    // should bind a separate downstream node, not the source.
    // Body is `{"status":"ok"}` (15 bytes). The `Content-Length`
    // declaration must match exactly or undici / strict HTTP clients
    // raise `UND_ERR_RES_CONTENT_LENGTH_MISMATCH`.
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
/// Backed by [`notify::RecommendedWatcher`] (platform-native: FSEvents
/// on macOS, inotify on Linux, ReadDirectoryChangesW on Windows).
/// Watch is recursive. Events surface as
/// `{ kind, paths: string[] }` where `kind` is the debug-formatted
/// notify event kind (e.g., `"Modify(Data(Content))"`).
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
                    eprintln!("[graphreact-napi] watch source init failed: {e}");
                    return;
                }
            };
        if let Err(e) = watcher.watch(std::path::Path::new(&self.path), RecursiveMode::Recursive) {
            eprintln!(
                "[graphreact-napi] watch source watch({}) failed: {e}",
                self.path
            );
            return;
        }
        *self.watcher.lock() = Some(watcher);
    }

    fn stop(&self) {
        // Drop the watcher to unwatch + drain its event channel.
        self.watcher.lock().take();
    }
}
