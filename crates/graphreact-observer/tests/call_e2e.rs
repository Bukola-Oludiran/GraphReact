//! **M-Ingress phase 2 verification gate.** End-to-end tests for
//! `POST /v1/nodes/:id/call`: spin up the axum server, drain the
//! `InjectMessageWithReply` bridge variant with a fake responder
//! that simulates manifest gating + dispatch, and either fire a
//! `CallReply` event back into the `PendingCalls` registry (success
//! path) or let the request time out (504 path).
//!
//! Mirrors the inject_e2e.rs layout. We don't spin up a real `!Send`
//! Scheduler — a synthetic drain task simulates `(NodeNotFound /
//! RejectedByManifest / Dispatched)` dispositions; for the success
//! path we additionally fire `PendingCalls::on_call_reply` directly
//! to exercise the registry wiring. The scheduler-side
//! auto-propagation logic (correlation_id capture in
//! `Scheduler::deliver` + first-match emit tagging in `make_emit`)
//! is exercised by core's runtime tests.

use std::sync::Arc;

use graphreact_observer::{
    spawn_management_api, ApiServerHandle, BearerToken, BindConfig,
    InjectMessageWithReplyDisposition, MetricsInner, PendingCalls, SnapshotRequest,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Behaviour for the synthetic drain task: how to respond to each
/// inbound `InjectMessageWithReply`.
#[derive(Debug, Clone)]
enum CallBehaviour {
    /// Respond `Dispatched` and immediately fire `on_call_reply`
    /// with the supplied payload — mirrors the post-Phase-2
    /// scheduler tagging the first matching emit.
    ReplyImmediately(Value),
    /// Respond `Dispatched` and DROP the correlation_id without
    /// firing — used to exercise the timeout (504) path.
    DropAfterDispatch,
    /// Respond `RejectedByManifest` — exercises 403.
    RejectByManifest,
    /// Respond `NodeNotFound` — exercises 404.
    NodeNotFound,
}

/// Spawn the management API + a drain loop that responds to
/// `InjectMessageWithReply` per the supplied behaviour. The
/// PendingCalls registry from the API state is cloned into the
/// drain task so the success path can resolve oneshots directly.
async fn spawn_with_call_responder(
    token: Option<&[u8]>,
    behaviour: CallBehaviour,
) -> (ApiServerHandle, JoinHandle<()>) {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: token.map(|t| BearerToken::from_bytes(t.to_vec()).unwrap()),
        tls: None,
    };
    let mut server = spawn_management_api(bind, metrics, 1234)
        .await
        .expect("spawn");
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let pending_calls = server.pending_calls.clone();
    let drain_task = tokio::spawn(async move {
        while let Some(req) = snapshot_rx.recv().await {
            match req {
                SnapshotRequest::InjectMessageWithReply {
                    correlation_id,
                    reply_type,
                    oneshot_tx,
                    ..
                } => {
                    match behaviour.clone() {
                        CallBehaviour::ReplyImmediately(payload) => {
                            let _ = oneshot_tx.send(InjectMessageWithReplyDisposition::Dispatched);
                            // Fire on_call_reply via the
                            // PendingCalls TelemetrySink impl —
                            // mirrors the scheduler's auto-tag
                            // path firing on_call_reply on the
                            // observer-side multiplex.
                            graphreact_core::TelemetrySink::on_call_reply(
                                &pending_calls,
                                &graphreact_core::CallReply {
                                    correlation_id: &correlation_id,
                                    payload: &payload,
                                    from: "fixture",
                                    reply_type: &reply_type,
                                },
                            );
                        }
                        CallBehaviour::DropAfterDispatch => {
                            let _ = oneshot_tx.send(InjectMessageWithReplyDisposition::Dispatched);
                            // Don't fire on_call_reply — caller
                            // times out.
                        }
                        CallBehaviour::RejectByManifest => {
                            let _ = oneshot_tx
                                .send(InjectMessageWithReplyDisposition::RejectedByManifest);
                        }
                        CallBehaviour::NodeNotFound => {
                            let _ =
                                oneshot_tx.send(InjectMessageWithReplyDisposition::NodeNotFound);
                        }
                    }
                }
                _ => { /* not exercised in this suite */ }
            }
        }
    });
    (server, drain_task)
}

#[tokio::test]
async fn call_returns_200_with_payload_on_reply() {
    let reply_payload = serde_json::json!({"answer": 42});
    let (server, _drain) =
        spawn_with_call_responder(None, CallBehaviour::ReplyImmediately(reply_payload.clone()))
            .await;
    let (status, body) = http_post(
        &format!("http://{}/v1/nodes/aggregator/call", server.local_addr),
        None,
        r#"{"type":"query","reply_type":"query_result","payload":{"q":1}}"#,
    )
    .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["reply_type"], "query_result");
    assert_eq!(v["payload"], reply_payload);
    server.shutdown().await;
}

#[tokio::test]
async fn call_returns_504_on_timeout() {
    let (server, _drain) = spawn_with_call_responder(None, CallBehaviour::DropAfterDispatch).await;
    let (status, body) = http_post(
        &format!("http://{}/v1/nodes/slow/call", server.local_addr),
        None,
        // 100ms client-side cap so the test finishes quickly.
        r#"{"type":"query","reply_type":"query_result","payload":{},"timeout_ms":100}"#,
    )
    .await;
    assert_eq!(status, 504);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"], "reply_timeout");
    assert_eq!(v["timeout_ms"], 100);
    server.shutdown().await;
}

#[tokio::test]
async fn call_returns_403_on_manifest_rejection() {
    let (server, _drain) = spawn_with_call_responder(None, CallBehaviour::RejectByManifest).await;
    let (status, body) = http_post(
        &format!("http://{}/v1/nodes/aggregator/call", server.local_addr),
        None,
        r#"{"type":"unknown","reply_type":"unknown_result","payload":{}}"#,
    )
    .await;
    assert_eq!(status, 403);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"], "rejected_by_manifest");
    server.shutdown().await;
}

#[tokio::test]
async fn call_returns_404_for_unknown_node() {
    let (server, _drain) = spawn_with_call_responder(None, CallBehaviour::NodeNotFound).await;
    let (status, body) = http_post(
        &format!("http://{}/v1/nodes/ghost/call", server.local_addr),
        None,
        r#"{"type":"query","reply_type":"query_result","payload":{}}"#,
    )
    .await;
    assert_eq!(status, 404);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"], "node_not_found");
    server.shutdown().await;
}

#[tokio::test]
async fn concurrent_calls_isolate_correlation_ids() {
    // Three concurrent calls each get a distinct payload back.
    // The synthetic responder echoes the request payload as the
    // reply payload so we can verify per-call isolation through
    // the registry — no cross-contamination across in-flight
    // correlation_ids.
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let mut server = spawn_management_api(bind, metrics, 1234)
        .await
        .expect("spawn");
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let pending_calls = server.pending_calls.clone();
    let _drain = tokio::spawn(async move {
        while let Some(req) = snapshot_rx.recv().await {
            if let SnapshotRequest::InjectMessageWithReply {
                correlation_id,
                payload,
                reply_type,
                oneshot_tx,
                ..
            } = req
            {
                let _ = oneshot_tx.send(InjectMessageWithReplyDisposition::Dispatched);
                graphreact_core::TelemetrySink::on_call_reply(
                    &pending_calls,
                    &graphreact_core::CallReply {
                        correlation_id: &correlation_id,
                        payload: &payload,
                        from: "fixture",
                        reply_type: &reply_type,
                    },
                );
            }
        }
    });
    let url = format!("http://{}/v1/nodes/aggregator/call", server.local_addr);
    let mut tasks = Vec::new();
    for i in 0..3u32 {
        let url = url.clone();
        tasks.push(tokio::spawn(async move {
            let body =
                format!(r#"{{"type":"query","reply_type":"query_result","payload":{{"i":{i}}}}}"#);
            http_post(&url, None, &body).await
        }));
    }
    let mut got: Vec<u32> = Vec::new();
    for t in tasks {
        let (status, body) = t.await.unwrap();
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        let i = u32::try_from(v["payload"]["i"].as_u64().unwrap()).unwrap();
        got.push(i);
    }
    got.sort_unstable();
    assert_eq!(got, vec![0, 1, 2]);
    server.shutdown().await;
}

#[tokio::test]
async fn pending_calls_guard_evicts_on_drop() {
    // Direct unit-style test of the registry guard. Registers an
    // entry, drops the guard without resolving, asserts the entry
    // is gone.
    let registry = PendingCalls::new();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let guard = registry
        .register("abc-123".into(), tx)
        .expect("registry empty");
    assert_eq!(registry.len(), 1);
    drop(guard);
    assert_eq!(registry.len(), 0);
}

#[tokio::test]
async fn pending_calls_guard_disarm_skips_drop_eviction() {
    // After resolving via on_call_reply, the registry sink
    // already removed the entry — calling disarm() prevents the
    // Drop from a redundant remove.
    let registry = PendingCalls::new();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let guard = registry
        .register("abc-456".into(), tx)
        .expect("registry empty");
    assert_eq!(registry.len(), 1);
    // Simulate the sink resolving + removing the entry first.
    graphreact_core::TelemetrySink::on_call_reply(
        &registry,
        &graphreact_core::CallReply {
            correlation_id: "abc-456",
            payload: &serde_json::Value::Null,
            from: "fixture",
            reply_type: "ignored",
        },
    );
    assert_eq!(registry.len(), 0);
    // Disarm + drop — should NOT panic and should NOT touch the
    // (now empty) inner map.
    guard.disarm();
}

// ----- HTTP/1.1 helper (clone of inject_e2e.rs's; no
// `Idempotency-Key` since /call doesn't use it in phase 2) -----

async fn http_post(url: &str, authorization: Option<&str>, body: &str) -> (u16, String) {
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
         Content-Type: application/json\r\n\
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
    (status, body.to_string())
}
