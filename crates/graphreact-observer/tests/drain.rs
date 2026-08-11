//! **M-Prod sub-track #4 Phase α verification gate.** End-to-end
//! tests for the SIGTERM drain primitive: spin up the axum server
//! with a drain bit wired via `spawn_management_api_with_drain`,
//! flip the bit, hit `/v1/readyz` + ingress endpoints with a tokio
//! HTTP/1.1 client, verify the 503 `draining` body shape +
//! `Retry-After: 1` header + RAII guard balance on
//! [`graphreact_observer::IngressInFlight`].
//!
//! Mirrors the pattern in `management_api.rs` / `inject_e2e.rs`:
//! no real `!Send` scheduler, just a fake bridge drain task. The
//! tests assert on observer-side wiring; the
//! `DaemonHandle::start_drain` orchestration lives in
//! `graphreact-cli` and is covered by `cluster_cli.rs` integration
//! tests.

use std::sync::Arc;
use std::time::Duration;

use graphreact_observer::{
    spawn_management_api_with_drain, ApiServerHandle, BindConfig, DrainFlag, IngressInFlight,
    InjectResult, MetricsInner, SnapshotRequest,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Spawn the management API with a drain bit + a drain-loop that
/// resolves `InjectMessage` / `InjectMessageWithReply` after the
/// supplied `delay`. Lets each test simulate slow in-flight
/// handlers by stretching the bridge drain side.
async fn spawn_with_drain_responder(
    delay: Duration,
) -> (ApiServerHandle, JoinHandle<()>, DrainFlag, IngressInFlight) {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let drain = DrainFlag::new();
    let mut server =
        spawn_management_api_with_drain(bind, metrics, 4242, None, Some(drain.clone()))
            .await
            .expect("spawn");
    let ingress = server.ingress_in_flight.clone();
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let drain_task = tokio::spawn(async move {
        while let Some(req) = snapshot_rx.recv().await {
            match req {
                SnapshotRequest::InjectMessage { oneshot_tx, .. } => {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let _ = oneshot_tx.send(InjectResult::Accepted);
                }
                SnapshotRequest::InjectMessageWithReply { oneshot_tx, .. } => {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let _ = oneshot_tx
                        .send(graphreact_observer::InjectMessageWithReplyDisposition::Dispatched);
                }
                _ => {}
            }
        }
    });
    (server, drain_task, drain, ingress)
}

/// `/v1/readyz` flips to 503 + body `draining` as soon as the
/// `DrainFlag` flips. Pre-tick the metrics so all OTHER predicates
/// would be green — this isolates the drain branch.
#[tokio::test]
async fn readyz_returns_draining_after_start_drain() {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let drain = DrainFlag::new();
    let mut server =
        spawn_management_api_with_drain(bind, metrics.clone(), 7, None, Some(drain.clone()))
            .await
            .expect("spawn");
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let _noop: JoinHandle<()> =
        tokio::spawn(async move { while snapshot_rx.recv().await.is_some() {} });

    // Make the baseline predicate green: simulate a first batch tick.
    metrics.process_batch_duration.observe(100_000);

    // Pre-drain: 200 ready.
    let (status, body) = http_get(&format!("http://{}/v1/readyz", server.local_addr)).await;
    assert_eq!(status.code, 200);
    assert!(body.contains("ready"));

    // Flip the drain bit. start() must succeed (CAS gate).
    assert!(drain.start(15_000));

    // Post-drain: 503 + body "draining" + Retry-After: 1.
    let (status, body) = http_get(&format!("http://{}/v1/readyz", server.local_addr)).await;
    assert_eq!(status.code, 503);
    assert_eq!(body, "draining");
    assert_eq!(status.retry_after.as_deref(), Some("1"));

    server.shutdown().await;
}

/// Ingress POST `/v1/nodes/:id/messages` refuses with 503 +
/// `draining` JSON body once drain flips. The ingress-in-flight
/// counter MUST NOT bump on the refused path so the orchestrator's
/// quiescence poll stays consistent.
#[tokio::test]
async fn inject_endpoint_refuses_with_draining_after_drain_flip() {
    let (server, _drain_task, drain, ingress) = spawn_with_drain_responder(Duration::ZERO).await;

    // Pre-drain: 202 accepted.
    let (status, _) = http_post(
        &format!("http://{}/v1/nodes/alpha/messages", server.local_addr),
        r#"{"type":"calibrate","payload":{}}"#,
    )
    .await;
    assert_eq!(status, 202);

    // Drain on.
    assert!(drain.start(15_000));

    // Post-drain inject: 503 + body "draining" + Retry-After: 1.
    let (status, body) = http_post_with_headers(
        &format!("http://{}/v1/nodes/alpha/messages", server.local_addr),
        r#"{"type":"calibrate","payload":{}}"#,
    )
    .await;
    assert_eq!(status.code, 503);
    assert_eq!(status.retry_after.as_deref(), Some("1"));
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"], "draining");

    // Drain check fires BEFORE the in-flight bump, so the refused
    // request never registered. Counter remains zero.
    assert_eq!(
        ingress.load(),
        0,
        "ingress in-flight counter must not bump on drained-out refusal path"
    );

    server.shutdown().await;
}

/// Ingress POST `/v1/nodes/:id/call` refuses with 503 + `draining`
/// once drain flips. Same shape as the messages endpoint — the
/// body is the JSON `{"error":"draining","retry_after_secs":1}`.
#[tokio::test]
async fn call_endpoint_refuses_with_draining_after_drain_flip() {
    let (server, _drain_task, drain, _ingress) = spawn_with_drain_responder(Duration::ZERO).await;
    assert!(drain.start(10_000));

    let (status, body) = http_post_with_headers(
        &format!("http://{}/v1/nodes/alpha/call", server.local_addr),
        r#"{"type":"ask","reply_type":"answer","payload":{}}"#,
    )
    .await;
    assert_eq!(status.code, 503);
    assert_eq!(status.retry_after.as_deref(), Some("1"));
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"], "draining");

    server.shutdown().await;
}

/// In-flight POST that arrived BEFORE the drain flip completes
/// normally; the `IngressInFlight` counter bumps on entry and
/// settles back to zero on the response. Verifies the RAII guard
/// balance the orchestrator's quiescence loop relies on.
#[tokio::test]
async fn inflight_inject_completes_through_drain_settles_counter() {
    // 200ms scheduler-side delay so the request is in-flight long
    // enough for us to observe the counter increment between
    // request-start and response.
    let (server, _drain_task, _drain, ingress) =
        spawn_with_drain_responder(Duration::from_millis(200)).await;
    let url = format!("http://{}/v1/nodes/alpha/messages", server.local_addr);

    // Fire the inject in a background task so we can poll the
    // counter while it's in-flight.
    let url2 = url.clone();
    let inject =
        tokio::spawn(async move { http_post(&url2, r#"{"type":"calibrate","payload":{}}"#).await });

    // Wait long enough for the request to register but not finish.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        ingress.load(),
        1,
        "in-flight counter should reflect the pending request"
    );

    // Now await the inject; it should complete with 202.
    let (status, _) = inject.await.unwrap();
    assert_eq!(status, 202);

    // Counter back to zero. Critical for the orchestrator's
    // quiescence-loop correctness.
    assert_eq!(ingress.load(), 0);

    server.shutdown().await;
}

/// A call admitted before drain keeps its normal timeout during
/// steady state, but drain truncates that wait to the drain budget.
/// The handler returns a structured 504 and evicts both lifecycle
/// counters before the outer daemon deadline needs to escalate.
#[tokio::test]
async fn inflight_call_timeout_is_truncated_by_drain_deadline() {
    let (server, _drain_task, drain, ingress) = spawn_with_drain_responder(Duration::ZERO).await;
    let pending_calls = server.pending_calls.clone();
    let url = format!("http://{}/v1/nodes/alpha/call", server.local_addr);
    let call = tokio::spawn(async move {
        http_post(
            &url,
            r#"{"type":"ask","reply_type":"answer","payload":{},"timeout_ms":300000}"#,
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        while pending_calls.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("call should register before drain");
    assert_eq!(ingress.load(), 1);

    let started = std::time::Instant::now();
    assert!(drain.start(100));
    let (status, body) = call.await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(status, 504);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["error"], "reply_timeout");
    assert_eq!(body["reason"], "drain_started");
    assert_eq!(body["timeout_ms"], 100);
    assert!(
        elapsed < Duration::from_secs(1),
        "drain should truncate the five-minute call timeout, got {elapsed:?}"
    );
    assert!(pending_calls.is_empty());
    assert_eq!(ingress.load(), 0);

    server.shutdown().await;
}

/// Drain also bounds a call still waiting for the scheduler bridge's
/// initial disposition, not only calls already waiting for a reply.
#[tokio::test]
async fn call_waiting_for_dispatch_is_truncated_by_drain_deadline() {
    let (server, drain_task, drain, ingress) =
        spawn_with_drain_responder(Duration::from_millis(500)).await;
    let pending_calls = server.pending_calls.clone();
    let url = format!("http://{}/v1/nodes/alpha/call", server.local_addr);
    let call = tokio::spawn(async move {
        http_post(
            &url,
            r#"{"type":"ask","reply_type":"answer","payload":{},"timeout_ms":300000}"#,
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        while pending_calls.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("call should register before drain");

    assert!(drain.start(50));
    let (status, body) = call.await.unwrap();
    assert_eq!(status, 504);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["reason"], "drain_started");
    assert!(pending_calls.is_empty());
    assert_eq!(ingress.load(), 0);

    drain_task.abort();
    server.shutdown().await;
}

/// Double-fire of `DrainFlag::start` is a no-op past the first
/// successful flip. Mirrors duplicate-SIGTERM handling — the
/// kubelet may deliver the signal twice, and a second
/// `drain_started` audit emit would mislead operators.
#[tokio::test]
async fn drain_start_is_idempotent_across_duplicate_calls() {
    let drain = DrainFlag::new();
    assert!(!drain.is_draining());
    assert!(drain.start(20_000));
    assert!(drain.is_draining());
    // Second call returns false — no second audit row emitted.
    assert!(!drain.start(20_000));
    assert!(drain.is_draining());
}

// ----- HTTP/1.1 helpers (mirror inject_e2e.rs + management_api.rs) -----

struct Status {
    code: u16,
    retry_after: Option<String>,
}

async fn http_get(url: &str) -> (Status, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let after_scheme = url.strip_prefix("http://").expect("http:// scheme");
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(hp, p)| (hp, format!("/{p}")))
        .unwrap_or((after_scheme, "/".into()));
    let mut stream = TcpStream::connect(host_port).await.expect("connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("send");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    let response = String::from_utf8(response).expect("utf-8");
    let (head, body) = response.split_once("\r\n\r\n").expect("framing");
    let mut lines = head.lines();
    let status_line = lines.next().expect("status");
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut retry_after = None;
    for h in lines {
        if let Some((name, value)) = h.split_once(':') {
            if name.eq_ignore_ascii_case("retry-after") {
                retry_after = Some(value.trim().to_string());
            }
        }
    }
    (Status { code, retry_after }, body.to_string())
}

async fn http_post(url: &str, body: &str) -> (u16, String) {
    let (status, body) = http_post_with_headers(url, body).await;
    (status.code, body)
}

async fn http_post_with_headers(url: &str, body: &str) -> (Status, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let after_scheme = url.strip_prefix("http://").expect("http:// scheme");
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(hp, p)| (hp, format!("/{p}")))
        .unwrap_or((after_scheme, "/".into()));
    let mut stream = TcpStream::connect(host_port).await.expect("connect");
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Connection: close\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\r\n{body}",
        len = body.len(),
    );
    stream.write_all(req.as_bytes()).await.expect("send");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    let response = String::from_utf8(response).expect("utf-8");
    let (head, body) = response.split_once("\r\n\r\n").expect("framing");
    let mut lines = head.lines();
    let status_line = lines.next().expect("status");
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut retry_after = None;
    for h in lines {
        if let Some((name, value)) = h.split_once(':') {
            if name.eq_ignore_ascii_case("retry-after") {
                retry_after = Some(value.trim().to_string());
            }
        }
    }
    (Status { code, retry_after }, body.to_string())
}
