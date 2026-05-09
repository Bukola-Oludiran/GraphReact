//! **M-Ingress phase 1 verification gate.** End-to-end tests for
//! `POST /v1/nodes/:id/messages`: spin up the axum server, drain the
//! `InjectMessage` bridge variant with a fake responder, hit the route
//! with a tokio HTTP/1.1 client + verify routing / auth / idempotency /
//! 404 / accepted shapes.
//!
//! Mirrors the management_api.rs test pattern. We don't spin up a real
//! `!Send` Scheduler — the bridge drain task simulates scheduler
//! responses (Accepted vs NodeNotFound) which is sufficient to pin the
//! HTTP-layer wiring. The scheduler-side enforcement of
//! `manifest.accepts_external` (default-deny via `Scheduler::deliver`)
//! is exercised by core's runtime tests.

use std::sync::Arc;

use graphreact_observer::{
    spawn_management_api, ApiServerHandle, BearerToken, BindConfig, IdempotencyCache,
    InjectResult, MetricsInner, SnapshotRequest,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Spawn the management API + a test drain loop that responds to
/// `InjectMessage` with `Accepted` for any node id starting with
/// "alpha" / "beta", and `NodeNotFound` for anything else. Returns
/// the server handle + the drain task handle.
async fn spawn_with_inject_responder(
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
            match req {
                SnapshotRequest::InjectMessage {
                    node_id,
                    oneshot_tx,
                    ..
                } => {
                    if node_id.starts_with("alpha") || node_id.starts_with("beta") {
                        let _ = oneshot_tx.send(InjectResult::Accepted);
                    } else {
                        let _ = oneshot_tx.send(InjectResult::NodeNotFound);
                    }
                }
                _ => { /* other requests not exercised in this suite */ }
            }
        }
    });
    (server, drain_task)
}

#[tokio::test]
async fn inject_returns_202_for_existing_node() {
    let (server, _drain) = spawn_with_inject_responder(None).await;
    let (status, body) = http_post(
        &format!("http://{}/v1/nodes/alpha/messages", server.local_addr),
        None,
        None,
        r#"{"type":"calibrate","payload":{"offset":0.5}}"#,
    )
    .await;
    assert_eq!(status, 202);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "accepted");
    server.shutdown().await;
}

#[tokio::test]
async fn inject_returns_404_for_unknown_node() {
    let (server, _drain) = spawn_with_inject_responder(None).await;
    let (status, body) = http_post(
        &format!("http://{}/v1/nodes/ghost/messages", server.local_addr),
        None,
        None,
        r#"{"type":"calibrate","payload":{}}"#,
    )
    .await;
    assert_eq!(status, 404);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"], "node_not_found");
    server.shutdown().await;
}

#[tokio::test]
async fn inject_requires_bearer_when_token_configured() {
    let (server, _drain) = spawn_with_inject_responder(Some(b"top-secret")).await;
    // Missing bearer
    let (status, _) = http_post(
        &format!("http://{}/v1/nodes/alpha/messages", server.local_addr),
        None,
        None,
        r#"{"type":"calibrate","payload":{}}"#,
    )
    .await;
    assert_eq!(status, 401);
    // Wrong bearer
    let (status, _) = http_post(
        &format!("http://{}/v1/nodes/alpha/messages", server.local_addr),
        Some("Bearer wrong-token"),
        None,
        r#"{"type":"calibrate","payload":{}}"#,
    )
    .await;
    assert_eq!(status, 401);
    // Correct bearer
    let (status, _) = http_post(
        &format!("http://{}/v1/nodes/alpha/messages", server.local_addr),
        Some("Bearer top-secret"),
        None,
        r#"{"type":"calibrate","payload":{}}"#,
    )
    .await;
    assert_eq!(status, 202);
    server.shutdown().await;
}

#[tokio::test]
async fn inject_idempotency_key_replay_returns_409() {
    let (server, _drain) = spawn_with_inject_responder(None).await;
    let url = format!("http://{}/v1/nodes/alpha/messages", server.local_addr);
    let body = r#"{"type":"calibrate","payload":{"v":1}}"#;
    let key = "dedup-test-key-1";

    // First request → 202
    let (status1, _) = http_post(&url, None, Some(key), body).await;
    assert_eq!(status1, 202);

    // Replay with same key → 409
    let (status2, body2) = http_post(&url, None, Some(key), body).await;
    assert_eq!(status2, 409);
    let v: Value = serde_json::from_str(&body2).unwrap();
    assert_eq!(v["error"], "idempotency_conflict");
    assert_eq!(v["key"], key);

    // Different key → 202
    let (status3, _) = http_post(&url, None, Some("dedup-test-key-2"), body).await;
    assert_eq!(status3, 202);

    server.shutdown().await;
}

#[tokio::test]
async fn inject_returns_400_for_malformed_body() {
    let (server, _drain) = spawn_with_inject_responder(None).await;
    let (status, _) = http_post(
        &format!("http://{}/v1/nodes/alpha/messages", server.local_addr),
        None,
        None,
        r#"{"not_a_type_field":"oops"}"#,
    )
    .await;
    // axum's `Json<T>` extractor rejects with 422 by default in 0.7,
    // or 400 depending on config. Either is acceptable for malformed input.
    assert!(
        status == 400 || status == 422,
        "expected 400 or 422 for malformed body; got {status}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn inject_omits_idempotency_key_proceeds_normally() {
    let (server, _drain) = spawn_with_inject_responder(None).await;
    let url = format!("http://{}/v1/nodes/alpha/messages", server.local_addr);
    let body = r#"{"type":"calibrate","payload":{}}"#;
    // Two requests without Idempotency-Key — both succeed.
    let (s1, _) = http_post(&url, None, None, body).await;
    let (s2, _) = http_post(&url, None, None, body).await;
    assert_eq!(s1, 202);
    assert_eq!(s2, 202);
    server.shutdown().await;
}

#[test]
fn idempotency_cache_dedups_within_ttl() {
    let cache = IdempotencyCache::new();
    assert!(cache.try_insert("k1"));
    assert!(!cache.try_insert("k1")); // duplicate
    assert!(cache.try_insert("k2"));
    assert_eq!(cache.len(), 2);
}

#[test]
fn idempotency_cache_distinct_keys_pass() {
    let cache = IdempotencyCache::new();
    for i in 0..100 {
        assert!(cache.try_insert(&format!("key-{i}")));
    }
    assert_eq!(cache.len(), 100);
}

// ----- HTTP/1.1 helpers -----

async fn http_post(
    url: &str,
    authorization: Option<&str>,
    idempotency_key: Option<&str>,
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
    let auth_line = match authorization {
        Some(a) => format!("Authorization: {a}\r\n"),
        None => String::new(),
    };
    let idem_line = match idempotency_key {
        Some(k) => format!("Idempotency-Key: {k}\r\n"),
        None => String::new(),
    };
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Connection: close\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         {auth_line}{idem_line}\r\n{body}",
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
    (status, body.to_string())
}
