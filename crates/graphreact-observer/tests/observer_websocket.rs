//! **M16-Backend P7 verification gate.** End-to-end test of the WS +
//! SSE live event stream — drive [`WebSocketSink`] with synthetic
//! events + verify the WebSocket route delivers them as JSON frames
//! and the SSE route delivers them as `data:` lines.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use graphreact_core::{
    telemetry::{
        MessageDelivered, NodeMounted, SchedulerTickStart, SupervisorAction, SupervisorDecisionMade,
    },
    TelemetrySink,
};
use graphreact_observer::{
    spawn_management_api, ApiServerHandle, BindConfig, MetricsInner, TelemetryEvent,
};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

async fn spawn_with_ws() -> ApiServerHandle {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    spawn_management_api(bind, metrics, 1234)
        .await
        .expect("spawn")
}

fn fire_messages(server: &ApiServerHandle) {
    let ws = server.websocket.as_ref().expect("websocket sink installed");
    ws.on_message_delivered(&MessageDelivered {
        from: "alice",
        to: "bob",
        message_type: "ping",
        ttl: 4,
        direct: false,
        accepted: true,
        trace_context: None,
    });
    ws.on_message_delivered(&MessageDelivered {
        from: "alice",
        to: "carol",
        message_type: "pong",
        ttl: 3,
        direct: false,
        accepted: true,
        trace_context: None,
    });
    ws.on_node_mounted(&NodeMounted { node_id: "logger" });
    ws.on_supervisor_decision(&SupervisorDecisionMade {
        failed_id: "worker-1",
        reason: "crashed",
        action: SupervisorAction::Proceeded,
    });
}

async fn ws_connect(
    addr: std::net::SocketAddr,
    query: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}/v1/observer/events{query}");
    let (socket, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    socket
}

async fn collect_n_text_frames(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    n: usize,
) -> Vec<TelemetryEvent> {
    let mut events = Vec::with_capacity(n);
    while events.len() < n {
        let msg = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("recv timeout")
            .expect("stream not ended")
            .expect("ws frame");
        if let Message::Text(s) = msg {
            let ev: TelemetryEvent = serde_json::from_str(&s).expect("decode");
            events.push(ev);
        }
    }
    events
}

#[tokio::test]
async fn ws_round_trip_delivers_messages_as_json_frames() {
    let server = spawn_with_ws().await;
    let mut socket = ws_connect(server.local_addr, "").await;
    // Tiny grace so the subscriber registers before we fire events.
    tokio::time::sleep(Duration::from_millis(30)).await;
    fire_messages(&server);
    let events = collect_n_text_frames(&mut socket, 4).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, TelemetryEvent::MessageDelivered { from, to, .. } if from == "alice" && to == "bob")));
    assert!(events
        .iter()
        .any(|e| matches!(e, TelemetryEvent::NodeMounted { node_id, .. } if node_id == "logger")));
    let _ = socket.send(Message::Close(None)).await;
    server.shutdown().await;
}

#[tokio::test]
async fn ws_kind_filter_drops_other_event_kinds() {
    let server = spawn_with_ws().await;
    let mut socket = ws_connect(server.local_addr, "?type=message_delivered").await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    fire_messages(&server);
    let events = collect_n_text_frames(&mut socket, 2).await;
    assert!(events
        .iter()
        .all(|e| matches!(e, TelemetryEvent::MessageDelivered { .. })));
    let _ = socket.send(Message::Close(None)).await;
    server.shutdown().await;
}

#[tokio::test]
async fn ws_node_filter_matches_from_or_to() {
    let server = spawn_with_ws().await;
    let mut socket = ws_connect(server.local_addr, "?node=carol").await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    fire_messages(&server);
    let events = collect_n_text_frames(&mut socket, 1).await;
    assert!(matches!(
        &events[0],
        TelemetryEvent::MessageDelivered { to, .. } if to == "carol"
    ));
    let _ = socket.send(Message::Close(None)).await;
    server.shutdown().await;
}

#[tokio::test]
async fn ws_lag_resync_kicks_subscriber_with_resync_frame() {
    let server = spawn_with_ws().await;
    let ws = server.websocket.as_ref().expect("websocket sink installed");
    let mut socket = ws_connect(server.local_addr, "").await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    // Force broadcast lag by firing > BROADCAST_CAPACITY (1024) events
    // before the client polls. We don't need exactly 1024 — even
    // 4096 fires faster than a stalled client can drain.
    for _ in 0..4096 {
        ws.on_scheduler_tick_start(&SchedulerTickStart {});
    }
    // Drain until we see the Resync frame.
    let mut saw_resync = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let msg = tokio::time::timeout(Duration::from_secs(1), socket.next())
            .await
            .ok()
            .flatten();
        match msg {
            Some(Ok(Message::Text(s))) => {
                let v: Value = serde_json::from_str(&s).expect("json");
                if v["kind"] == "resync" {
                    saw_resync = true;
                    let missed = v["missed"].as_u64().unwrap_or(0);
                    assert!(missed > 0, "missed should be > 0 (got {missed})");
                    break;
                }
            }
            Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            Some(Ok(_)) => {}
        }
    }
    assert!(saw_resync, "expected resync frame after broadcast lag");
    assert!(
        ws.kicked_total() >= 1,
        "kicked counter should increment (got {})",
        ws.kicked_total()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn metrics_endpoint_includes_subscriber_metrics() {
    let server = spawn_with_ws().await;
    let mut socket = ws_connect(server.local_addr, "").await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let body = http_get(&format!("http://{}/v1/metrics", server.local_addr)).await;
    assert!(
        body.contains("grx_observer_subscribers_active"),
        "metric missing: {body}"
    );
    assert!(
        body.contains("grx_observer_subscribers_kicked_total"),
        "metric missing: {body}"
    );
    let _ = socket.send(Message::Close(None)).await;
    server.shutdown().await;
}

#[tokio::test]
async fn sse_route_streams_events_as_data_lines() {
    let server = spawn_with_ws().await;
    let url = format!("http://{}/v1/observer/events.sse", server.local_addr);
    let body_task = tokio::spawn(http_get_partial(url, Duration::from_millis(400), 1024));
    tokio::time::sleep(Duration::from_millis(60)).await;
    fire_messages(&server);
    let body = body_task.await.expect("collect").unwrap_or_default();
    assert!(body.contains("event: telemetry"), "sse body: {body}");
    assert!(
        body.contains("\"kind\":\"message_delivered\""),
        "sse body: {body}"
    );
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
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("send");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let response = String::from_utf8(buf).expect("utf-8");
    let (_head, body) = response.split_once("\r\n\r\n").expect("framing");
    body.to_string()
}

async fn http_get_partial(
    url: String,
    duration: Duration,
    max_bytes: usize,
) -> Result<String, std::io::Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let after_scheme = url.strip_prefix("http://").expect("http://");
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(hp, p)| (hp.to_string(), format!("/{p}")))
        .unwrap_or((after_scheme.to_string(), "/".into()));
    let mut stream = TcpStream::connect(&host_port).await?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\nAccept: text/event-stream\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::with_capacity(max_bytes);
    let read_fut = async {
        let mut chunk = [0u8; 256];
        while buf.len() < max_bytes {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    };
    let _ = tokio::time::timeout(duration, read_fut).await;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
