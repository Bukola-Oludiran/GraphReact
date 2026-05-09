//! **M16-Backend P4 verification gate.** End-to-end management-API
//! tests: spin up the axum server, drain snapshot requests with a
//! fake responder, hit each route from a tokio HTTP client + verify
//! routing / auth / JSON shape / scaling characteristics.
//!
//! Tests are kept lightweight by NOT spinning up a real scheduler
//! (which is `!Send` + needs a `LocalSet`). Instead, a small drain
//! task on the same tokio runtime responds to each
//! [`SnapshotRequest`] with a fixed fixture. This isolates
//! management-API surface tests from scheduler-internal behaviour;
//! the scheduler-side wiring is exercised by the
//! `cluster_cli`/`cluster_pubsub` integration tests in
//! `crates/graphreact-cli/tests/`.

use std::sync::Arc;

use graphreact_core::telemetry::{NodeIntrospection, SupervisorIntrospection};
use graphreact_observer::{
    spawn_management_api, ApiServerHandle, BearerToken, BindConfig, MetricsInner,
    SnapshotRequest,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Spawn the management API + a test drain loop that replies with
/// fixed fixture data. Returns the server handle (caller awaits
/// `.shutdown()`) + the drain task handle (for graceful teardown).
async fn spawn_with_fixture(
    token: Option<&[u8]>,
) -> (ApiServerHandle, JoinHandle<()>) {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: token.map(|t| BearerToken::from_bytes(t.to_vec()).unwrap()),
        tls: None,
    };
    let mut server = spawn_management_api(bind, metrics, 1234).await.expect("spawn");
    // Move the receiver out of `server` for the drain loop. We
    // rebuild the handle without the receiver so `server.shutdown()`
    // still joins the accept-loop task at end of test.
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let drain_task = tokio::spawn(async move {
        while let Some(req) = snapshot_rx.recv().await {
            match req {
                SnapshotRequest::Nodes(tx) => {
                    let _ = tx.send(vec![
                        NodeIntrospection {
                            id: "alpha".into(),
                            role: "sensor".into(),
                            depth: 0,
                            path: vec!["alpha".into()],
                            supervisor: None,
                        },
                        NodeIntrospection {
                            id: "beta".into(),
                            role: "logger".into(),
                            depth: 1,
                            path: vec!["alpha".into(), "beta".into()],
                            supervisor: Some("sup:0".into()),
                        },
                    ]);
                }
                SnapshotRequest::NodeDetail(id, tx) => {
                    let resp = if id == "alpha" {
                        Some(NodeIntrospection {
                            id: "alpha".into(),
                            role: "sensor".into(),
                            depth: 0,
                            path: vec!["alpha".into()],
                            supervisor: None,
                        })
                    } else {
                        None
                    };
                    let _ = tx.send(resp);
                }
                SnapshotRequest::SupervisorTree(tx) => {
                    let _ = tx.send(vec![SupervisorIntrospection {
                        key: "sup:0".into(),
                        strategy: "OneForOne".into(),
                        max_restarts: 5,
                        period_secs: 10,
                        restart_count: 0,
                        children: vec!["beta".into()],
                    }]);
                }
                // **M-Ingress.** This test suite predates ingress and
                // does not exercise inject / call / bulk — silently
                // drop all three. Dedicated coverage in
                // `inject_e2e.rs`, `call_e2e.rs`, `bulk_e2e.rs`.
                SnapshotRequest::InjectMessage { .. }
                | SnapshotRequest::InjectMessageWithReply { .. }
                | SnapshotRequest::InjectMessageBulkLine { .. } => {}
            }
        }
    });
    (server, drain_task)
}

#[tokio::test]
async fn healthz_bypasses_auth_and_returns_ok() {
    let (server, _drain) = spawn_with_fixture(Some(b"top-secret")).await;
    let (status, body) = http_get(&format!("http://{}/v1/healthz", server.local_addr), None).await;
    assert_eq!(status, 200);
    assert_eq!(body, "ok");
    server.shutdown().await;
}

#[tokio::test]
async fn identity_returns_process_start_id_and_version() {
    let (server, _drain) = spawn_with_fixture(None).await;
    let (status, body) =
        http_get(&format!("http://{}/v1/identity", server.local_addr), None).await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["process_start_id"], 1234);
    assert_eq!(v["api_version"], 1);
    assert!(v["version"].as_str().unwrap().contains('.'));
    server.shutdown().await;
}

#[tokio::test]
async fn nodes_list_returns_array_of_introspections() {
    let (server, _drain) = spawn_with_fixture(None).await;
    let (status, body) =
        http_get(&format!("http://{}/v1/nodes", server.local_addr), None).await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "alpha");
    assert_eq!(arr[1]["supervisor"], "sup:0");
    server.shutdown().await;
}

#[tokio::test]
async fn node_detail_with_known_id_returns_object() {
    let (server, _drain) = spawn_with_fixture(None).await;
    let (status, body) =
        http_get(&format!("http://{}/v1/nodes/alpha", server.local_addr), None).await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["id"], "alpha");
    assert_eq!(v["role"], "sensor");
    server.shutdown().await;
}

#[tokio::test]
async fn node_detail_with_unknown_id_returns_404() {
    let (server, _drain) = spawn_with_fixture(None).await;
    let (status, _body) =
        http_get(&format!("http://{}/v1/nodes/nonexistent", server.local_addr), None).await;
    assert_eq!(status, 404);
    server.shutdown().await;
}

#[tokio::test]
async fn supervisor_tree_returns_array_with_strategy() {
    let (server, _drain) = spawn_with_fixture(None).await;
    let (status, body) = http_get(
        &format!("http://{}/v1/supervisor/tree", server.local_addr),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["strategy"], "OneForOne");
    assert_eq!(arr[0]["children"][0], "beta");
    server.shutdown().await;
}

#[tokio::test]
async fn metrics_route_returns_prometheus_text() {
    let (server, _drain) = spawn_with_fixture(None).await;
    let (status, body) =
        http_get(&format!("http://{}/v1/metrics", server.local_addr), None).await;
    assert_eq!(status, 200);
    assert!(body.contains("# TYPE grx_node_mount_total counter"));
    server.shutdown().await;
}

#[tokio::test]
async fn protected_routes_return_401_when_token_missing() {
    let (server, _drain) = spawn_with_fixture(Some(b"the-token")).await;
    for path in ["/v1/identity", "/v1/metrics", "/v1/nodes", "/v1/supervisor/tree"] {
        let (status, _body) =
            http_get(&format!("http://{}{}", server.local_addr, path), None).await;
        assert_eq!(status, 401, "{path} expected 401 without token");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn protected_routes_accept_correct_token() {
    let (server, _drain) = spawn_with_fixture(Some(b"my-token-2027")).await;
    for path in ["/v1/identity", "/v1/metrics"] {
        let (status, _body) = http_get(
            &format!("http://{}{}", server.local_addr, path),
            Some("Bearer my-token-2027"),
        )
        .await;
        assert_eq!(status, 200, "{path} expected 200 with correct token");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn fifty_parallel_gets_complete_under_2s() {
    // Verifies the cross-thread bridge handles concurrent requests
    // without serializing on a single mutex. The drain loop replies
    // immediately (no scheduler tick to wait for in this fixture);
    // any contention in the axum router / tower middleware would
    // surface as request stalling.
    let (server, _drain) = spawn_with_fixture(None).await;
    let url = format!("http://{}/v1/nodes", server.local_addr);

    let start = std::time::Instant::now();
    let mut handles = Vec::with_capacity(50);
    for _ in 0..50 {
        let url = url.clone();
        handles.push(tokio::spawn(async move { http_get(&url, None).await }));
    }
    for h in handles {
        let (status, _) = h.await.unwrap();
        assert_eq!(status, 200);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "50 parallel GETs took {elapsed:?} (gate: <2s)"
    );
    server.shutdown().await;
}

/// Bare HTTP/1.1 client. Mirrors P2's tests/auth_required.rs helper.
async fn http_get(url: &str, authorization: Option<&str>) -> (u16, String) {
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
        "GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n{auth_line}\r\n"
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
