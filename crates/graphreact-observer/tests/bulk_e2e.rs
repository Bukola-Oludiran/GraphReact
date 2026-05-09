//! **M-Ingress phase 3 verification gate.** End-to-end tests for
//! `POST /v1/nodes/:id/messages/bulk`: spin up the axum server,
//! drain the `InjectMessageBulkLine` bridge variant with a
//! synthetic responder, hit the route with a chunked HTTP/1.1
//! client + verify the streaming NDJSON response shape.
//!
//! What's exercised:
//! - Per-line `accepted` / `node_not_found` / `rejected_by_manifest`
//!   dispositions (via the synthetic responder).
//! - Per-line `bad_json` for malformed lines (parser-side; no
//!   bridge dispatch).
//! - Per-line `idempotency_conflict` via the per-line
//!   `idempotency_key` field (HTTP-boundary dedup, no bridge
//!   dispatch).
//! - 413 Payload Too Large pre-flight via Content-Length cap.
//! - `exceeded_line_cap` terminator + stream close at the
//!   configured line cap.
//! - Empty trailing newlines (NDJSON tolerance) — last line
//!   without `\n` still processed.

use std::sync::Arc;

use graphreact_observer::{
    spawn_management_api, ApiServerHandle, BearerToken, BindConfig, BulkInjectResult, MetricsInner,
    SnapshotRequest, BULK_MAX_BODY_BYTES,
};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

/// Serialize every bulk test that depends on process-shared
/// state (notably the `GRX_INGRESS_BULK_LINE_CAP` env var read by
/// the bulk handler each request). Without this lock, tokio's
/// default parallel test runner lets `bulk_caps_lines_via_env_var_terminator`'s
/// `set_var("2")` leak into concurrent tests reading the env
/// from `bulk_line_cap()`. We hold the lock for the duration of
/// each test so all bulk tests run sequentially within the
/// process.
static BULK_TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// Behaviour for the synthetic bulk-drain responder. Per-line
/// disposition decided from the inbound line's payload (so each
/// line in a multi-line request can have a different disposition).
fn decide_bulk_response(node_id: &str, message_type: &str) -> BulkInjectResult {
    if node_id == "ghost" {
        BulkInjectResult::NodeNotFound
    } else if message_type.starts_with("forbidden") {
        BulkInjectResult::RejectedByManifest
    } else {
        BulkInjectResult::Accepted
    }
}

async fn spawn_with_bulk_responder(
    token: Option<&[u8]>,
) -> (ApiServerHandle, JoinHandle<()>) {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: token.map(|t| BearerToken::from_bytes(t.to_vec()).unwrap()),
        tls: None,
    };
    let mut server = spawn_management_api(bind, metrics, 1234).await.expect("spawn");
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let drain_task = tokio::spawn(async move {
        while let Some(req) = snapshot_rx.recv().await {
            if let SnapshotRequest::InjectMessageBulkLine {
                node_id,
                message_type,
                oneshot_tx,
                ..
            } = req
            {
                let _ = oneshot_tx.send(decide_bulk_response(&node_id, &message_type));
            }
        }
    });
    (server, drain_task)
}

/// Parse the streaming NDJSON response body into per-line
/// `(line_num, result)` pairs.
fn parse_bulk_body(body: &str) -> Vec<(u64, String)> {
    body.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let v: Value = serde_json::from_str(line).expect("ndjson parse");
            (
                v["line"].as_u64().unwrap(),
                v["result"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn bulk_returns_per_line_accepted_for_three_valid_lines() {
    let _guard = BULK_TEST_LOCK.lock().await;
    let (server, _drain) = spawn_with_bulk_responder(None).await;
    let body = "\
        {\"type\":\"audit\",\"payload\":{\"actor\":\"alice\"}}\n\
        {\"type\":\"audit\",\"payload\":{\"actor\":\"bob\"}}\n\
        {\"type\":\"audit\",\"payload\":{\"actor\":\"carol\"}}\n";
    let (status, content_type, response) = http_post_bulk(
        &format!("http://{}/v1/nodes/logger/messages/bulk", server.local_addr),
        None,
        body,
    )
    .await;
    assert_eq!(status, 200);
    assert!(content_type.starts_with("application/x-ndjson"));
    let lines = parse_bulk_body(&response);
    assert_eq!(
        lines,
        vec![
            (0, "accepted".into()),
            (1, "accepted".into()),
            (2, "accepted".into()),
        ]
    );
    server.shutdown().await;
}

#[tokio::test]
async fn bulk_surfaces_rejected_by_manifest_per_line() {
    let _guard = BULK_TEST_LOCK.lock().await;
    let (server, _drain) = spawn_with_bulk_responder(None).await;
    let body = "\
        {\"type\":\"audit\",\"payload\":{}}\n\
        {\"type\":\"forbidden_op\",\"payload\":{}}\n\
        {\"type\":\"audit\",\"payload\":{}}\n";
    let (status, _, response) = http_post_bulk(
        &format!("http://{}/v1/nodes/logger/messages/bulk", server.local_addr),
        None,
        body,
    )
    .await;
    assert_eq!(status, 200);
    let lines = parse_bulk_body(&response);
    assert_eq!(
        lines,
        vec![
            (0, "accepted".into()),
            (1, "rejected_by_manifest".into()),
            (2, "accepted".into()),
        ]
    );
    server.shutdown().await;
}

#[tokio::test]
async fn bulk_surfaces_node_not_found_per_line() {
    let _guard = BULK_TEST_LOCK.lock().await;
    let (server, _drain) = spawn_with_bulk_responder(None).await;
    let (status, _, response) = http_post_bulk(
        &format!(
            "http://{}/v1/nodes/ghost/messages/bulk",
            server.local_addr
        ),
        None,
        "{\"type\":\"audit\",\"payload\":{}}\n",
    )
    .await;
    assert_eq!(status, 200);
    let lines = parse_bulk_body(&response);
    assert_eq!(lines, vec![(0, "node_not_found".into())]);
    server.shutdown().await;
}

#[tokio::test]
async fn bulk_emits_bad_json_for_malformed_lines() {
    let _guard = BULK_TEST_LOCK.lock().await;
    let (server, _drain) = spawn_with_bulk_responder(None).await;
    let body = "\
        {\"type\":\"audit\",\"payload\":{}}\n\
        not-actually-json\n\
        {\"type\":\"audit\",\"payload\":{}}\n";
    let (status, _, response) = http_post_bulk(
        &format!("http://{}/v1/nodes/logger/messages/bulk", server.local_addr),
        None,
        body,
    )
    .await;
    assert_eq!(status, 200);
    let lines = parse_bulk_body(&response);
    assert_eq!(
        lines,
        vec![
            (0, "accepted".into()),
            (1, "bad_json".into()),
            (2, "accepted".into()),
        ]
    );
    server.shutdown().await;
}

#[tokio::test]
async fn bulk_dedupes_duplicate_idempotency_keys() {
    let _guard = BULK_TEST_LOCK.lock().await;
    let (server, _drain) = spawn_with_bulk_responder(None).await;
    // Same idempotency_key on lines 0 and 2 → second occurrence
    // returns idempotency_conflict and skips bridge dispatch.
    let body = "\
        {\"type\":\"audit\",\"payload\":{},\"idempotency_key\":\"k1\"}\n\
        {\"type\":\"audit\",\"payload\":{},\"idempotency_key\":\"k2\"}\n\
        {\"type\":\"audit\",\"payload\":{},\"idempotency_key\":\"k1\"}\n";
    let (status, _, response) = http_post_bulk(
        &format!("http://{}/v1/nodes/logger/messages/bulk", server.local_addr),
        None,
        body,
    )
    .await;
    assert_eq!(status, 200);
    let lines = parse_bulk_body(&response);
    assert_eq!(
        lines,
        vec![
            (0, "accepted".into()),
            (1, "accepted".into()),
            (2, "idempotency_conflict".into()),
        ]
    );
    server.shutdown().await;
}

#[tokio::test]
async fn bulk_returns_413_for_oversized_content_length() {
    let _guard = BULK_TEST_LOCK.lock().await;
    let (server, _drain) = spawn_with_bulk_responder(None).await;
    // Send a request with Content-Length larger than the body
    // cap. The handler rejects pre-flight without consuming the
    // body. We can't actually upload the full body in the test
    // (would be ~10 GB) — fortunately the pre-flight check fires
    // on the declared header, so a small body suffices.
    let url = format!("http://{}/v1/nodes/logger/messages/bulk", server.local_addr);
    let (status, body) = http_post_with_fake_content_length(
        &url,
        BULK_MAX_BODY_BYTES + 1,
        "{}",
    )
    .await;
    assert_eq!(status, 413);
    let v: Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(v["error"], "body_too_large");
    server.shutdown().await;
}

#[tokio::test]
async fn bulk_caps_lines_via_env_var_terminator() {
    let _guard = BULK_TEST_LOCK.lock().await;
    // Configure a small per-test cap via env var. The handler
    // re-reads the env var per request, so this works without
    // restarting the server.
    std::env::set_var("GRX_INGRESS_BULK_LINE_CAP", "2");
    let (server, _drain) = spawn_with_bulk_responder(None).await;
    let body = "\
        {\"type\":\"a\",\"payload\":{}}\n\
        {\"type\":\"b\",\"payload\":{}}\n\
        {\"type\":\"c\",\"payload\":{}}\n\
        {\"type\":\"d\",\"payload\":{}}\n";
    let (status, _, response) = http_post_bulk(
        &format!("http://{}/v1/nodes/logger/messages/bulk", server.local_addr),
        None,
        body,
    )
    .await;
    assert_eq!(status, 200);
    let lines = parse_bulk_body(&response);
    // First two accepted; third triggers the cap → terminator
    // line; stream closes (no fourth line in response).
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], (0, "accepted".into()));
    assert_eq!(lines[1], (1, "accepted".into()));
    assert_eq!(lines[2], (2, "exceeded_line_cap".into()));
    std::env::remove_var("GRX_INGRESS_BULK_LINE_CAP");
    server.shutdown().await;
}

#[tokio::test]
async fn bulk_processes_final_line_without_trailing_newline() {
    let _guard = BULK_TEST_LOCK.lock().await;
    let (server, _drain) = spawn_with_bulk_responder(None).await;
    // No trailing `\n` after the last line.
    let body = "\
        {\"type\":\"audit\",\"payload\":{\"i\":0}}\n\
        {\"type\":\"audit\",\"payload\":{\"i\":1}}";
    let (status, _, response) = http_post_bulk(
        &format!("http://{}/v1/nodes/logger/messages/bulk", server.local_addr),
        None,
        body,
    )
    .await;
    assert_eq!(status, 200);
    let lines = parse_bulk_body(&response);
    assert_eq!(
        lines,
        vec![(0, "accepted".into()), (1, "accepted".into())]
    );
    server.shutdown().await;
}

#[tokio::test]
async fn bulk_skips_empty_lines_silently() {
    let _guard = BULK_TEST_LOCK.lock().await;
    let (server, _drain) = spawn_with_bulk_responder(None).await;
    // CRLF + double-newlines mixed in. Empty lines are
    // tolerated (NDJSON convention) and don't increment the
    // line counter or hit the bridge.
    let body = "\
        {\"type\":\"audit\",\"payload\":{}}\r\n\
        \n\
        {\"type\":\"audit\",\"payload\":{}}\n\
        \n";
    let (status, _, response) = http_post_bulk(
        &format!("http://{}/v1/nodes/logger/messages/bulk", server.local_addr),
        None,
        body,
    )
    .await;
    assert_eq!(status, 200);
    let lines = parse_bulk_body(&response);
    assert_eq!(
        lines,
        vec![(0, "accepted".into()), (1, "accepted".into())]
    );
    server.shutdown().await;
}

#[tokio::test]
async fn bulk_requires_bearer_token_when_configured() {
    let _guard = BULK_TEST_LOCK.lock().await;
    let (server, _drain) = spawn_with_bulk_responder(Some(b"top-secret")).await;
    // No Authorization header → 401.
    let (status, _, _) = http_post_bulk(
        &format!("http://{}/v1/nodes/logger/messages/bulk", server.local_addr),
        None,
        "{\"type\":\"a\",\"payload\":{}}\n",
    )
    .await;
    assert_eq!(status, 401);

    // Correct token → 200.
    let (status_ok, _, body_ok) = http_post_bulk(
        &format!("http://{}/v1/nodes/logger/messages/bulk", server.local_addr),
        Some("Bearer top-secret"),
        "{\"type\":\"a\",\"payload\":{}}\n",
    )
    .await;
    assert_eq!(status_ok, 200);
    assert!(parse_bulk_body(&body_ok)
        .iter()
        .any(|(_, r)| r == "accepted"));
    server.shutdown().await;
}

// ----- HTTP/1.1 helpers -----

async fn http_post_bulk(
    url: &str,
    authorization: Option<&str>,
    body: &str,
) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let after_scheme = url.strip_prefix("http://").expect("http:// scheme");
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(hp, p)| (hp, format!("/{p}")))
        .unwrap_or((after_scheme, "/".into()));

    let mut stream = TcpStream::connect(host_port).await.expect("tcp connect");
    let auth_line = match authorization {
        Some(a) => format!("Authorization: {a}\r\n"),
        None => String::new(),
    };
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Connection: close\r\n\
         Content-Type: application/x-ndjson\r\n\
         Content-Length: {len}\r\n\
         {auth_line}\r\n{body}",
        len = body.len(),
    );
    stream.write_all(req.as_bytes()).await.expect("send req");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let response = String::from_utf8(response).expect("utf-8");
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP/1.1 framing missing CRLF CRLF");
    let status_line = head.lines().next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("u16 status");
    let content_type = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-type:")
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_default();
    // For chunked-encoded responses, the body comes back with
    // chunk-size hex prefixes. Decode minimally — our test
    // bodies are small so we walk hex-prefixed chunks. For
    // non-chunked responses (Content-Length-bounded) the body
    // is raw.
    let body_str = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        decode_chunked(body)
    } else {
        body.to_string()
    };
    (status, content_type, body_str)
}

/// Send a POST with a deliberately-wrong Content-Length header
/// to exercise the pre-flight 413 path. The body is small (the
/// server rejects before reading it) but we declare a huge
/// length to trigger the cap check.
async fn http_post_with_fake_content_length(
    url: &str,
    declared_len: u64,
    body: &str,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let after_scheme = url.strip_prefix("http://").expect("http:// scheme");
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(hp, p)| (hp, format!("/{p}")))
        .unwrap_or((after_scheme, "/".into()));

    let mut stream = TcpStream::connect(host_port).await.expect("tcp connect");
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Connection: close\r\n\
         Content-Type: application/x-ndjson\r\n\
         Content-Length: {declared_len}\r\n\
         \r\n{body}",
    );
    stream.write_all(req.as_bytes()).await.expect("send req");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let response = String::from_utf8(response).expect("utf-8");
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP/1.1 framing missing CRLF CRLF");
    let status_line = head.lines().next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("u16 status");
    (status, body.to_string())
}

/// Decode an HTTP/1.1 chunked-transfer-encoded body. Each chunk:
/// `<hex-size>\r\n<bytes>\r\n`; terminated by `0\r\n\r\n`.
fn decode_chunked(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        let (size_line, after_size) = match rest.split_once("\r\n") {
            Some(parts) => parts,
            None => return out,
        };
        let size = match usize::from_str_radix(size_line.trim(), 16) {
            Ok(n) => n,
            Err(_) => return out,
        };
        if size == 0 {
            return out;
        }
        if after_size.len() < size {
            return out;
        }
        out.push_str(&after_size[..size]);
        rest = match after_size[size..].strip_prefix("\r\n") {
            Some(r) => r,
            None => return out,
        };
    }
    out
}
