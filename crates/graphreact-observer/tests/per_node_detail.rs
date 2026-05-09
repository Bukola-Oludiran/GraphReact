//! **M16-Backend P5 verification gate.** End-to-end test of
//! `GET /v1/nodes/:id/messages/recent?limit=N` — drive
//! [`RingBufferTelemetry`] with synthetic events + verify the
//! HTTP route returns the expected tail.
//!
//! Mirrors the `tests/management_api.rs` pattern: spawn the
//! server, drive the ring buffer directly (no scheduler), hit
//! the route from a tokio HTTP client.

use std::sync::Arc;

use graphreact_core::{MessageDelivered, TelemetrySink};
use graphreact_observer::{
    spawn_management_api, ApiServerHandle, BindConfig, MetricsInner, MessageSnapshot,
};
use serde_json::Value;

async fn spawn_with_ring() -> ApiServerHandle {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    spawn_management_api(bind, metrics, 0xCAFE_BABE).await.expect("spawn")
}

#[tokio::test]
async fn recent_returns_empty_for_unknown_node() {
    let server = spawn_with_ring().await;
    let body = http_get(&format!(
        "http://{}/v1/nodes/ghost/messages/recent",
        server.local_addr
    ))
    .await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v.as_array().unwrap().is_empty());
    server.shutdown().await;
}

#[tokio::test]
async fn recent_returns_chronological_tail() {
    let server = spawn_with_ring().await;
    // Drive 5 messages directly into the ring (bypass scheduler).
    for i in 0..5 {
        let ty = format!("hop{i}");
        server.ring.on_message_delivered(&MessageDelivered {
            from: "alice",
            to: "bob",
            message_type: ty.as_str(),
            ttl: 1,
            direct: false,
            accepted: true,
            trace_context: None,
        });
    }
    let body = http_get(&format!(
        "http://{}/v1/nodes/bob/messages/recent?limit=3",
        server.local_addr
    ))
    .await;
    let rows: Vec<MessageSnapshot> = serde_json::from_str(&body).unwrap();
    assert_eq!(rows.len(), 3);
    let types: Vec<&str> = rows.iter().map(|s| s.message_type.as_str()).collect();
    assert_eq!(types, vec!["hop2", "hop3", "hop4"]);
    server.shutdown().await;
}

#[tokio::test]
async fn recent_wraps_at_default_capacity() {
    let server = spawn_with_ring().await;
    // Push 100 messages — ring should wrap at default 64.
    for i in 0..100 {
        let ty = format!("m{i}");
        server.ring.on_message_delivered(&MessageDelivered {
            from: "a",
            to: "n",
            message_type: ty.as_str(),
            ttl: 1,
            direct: false,
            accepted: true,
            trace_context: None,
        });
    }
    let body = http_get(&format!(
        "http://{}/v1/nodes/n/messages/recent?limit=200",
        server.local_addr
    ))
    .await;
    let rows: Vec<MessageSnapshot> = serde_json::from_str(&body).unwrap();
    // 64 = default capacity; the route caps `limit` at 4×default
    // = 256 but the ring itself only ever holds 64.
    assert_eq!(rows.len(), 64);
    // Newest is m99; oldest in the kept window is m36.
    assert_eq!(rows.first().unwrap().message_type, "m36");
    assert_eq!(rows.last().unwrap().message_type, "m99");
    server.shutdown().await;
}

#[tokio::test]
async fn snapshot_payload_field_is_absent() {
    // PII safe-by-default: payload is NEVER captured.
    let server = spawn_with_ring().await;
    server.ring.on_message_delivered(&MessageDelivered {
        from: "a",
        to: "leak-target",
        message_type: "secret",
        ttl: 1,
        direct: false,
        accepted: true,
        trace_context: None,
    });
    let body = http_get(&format!(
        "http://{}/v1/nodes/leak-target/messages/recent",
        server.local_addr
    ))
    .await;
    let v: Value = serde_json::from_str(&body).unwrap();
    let row = &v.as_array().unwrap()[0];
    assert!(row.get("payload").is_none(), "payload must NOT appear in response");
    assert!(row.get("body").is_none(), "body must NOT appear in response");
    server.shutdown().await;
}

#[tokio::test]
async fn rejected_messages_show_with_accepted_false() {
    let server = spawn_with_ring().await;
    server.ring.on_message_delivered(&MessageDelivered {
        from: "a",
        to: "b",
        message_type: "policy_blocked",
        ttl: 1,
        direct: false,
        accepted: false,
        trace_context: None,
    });
    let body = http_get(&format!(
        "http://{}/v1/nodes/b/messages/recent",
        server.local_addr
    ))
    .await;
    let rows: Vec<MessageSnapshot> = serde_json::from_str(&body).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].accepted);
    server.shutdown().await;
}

async fn http_get(url: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let after_scheme = url.strip_prefix("http://").expect("http://");
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(hp, p)| (hp, format!("/{p}")))
        .unwrap_or((after_scheme, "/".into()));

    let mut stream = TcpStream::connect(host_port).await.expect("tcp connect");
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.expect("send");

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let response = String::from_utf8(buf).expect("utf-8");
    let (head, body) = response.split_once("\r\n\r\n").expect("framing");
    let status: u16 = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(status, 200, "expected 200 from {url}; got {status}; body: {body}");
    body.to_string()
}
