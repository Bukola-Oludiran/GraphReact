//! **M16-Backend P2 verification gate.** End-to-end Prometheus
//! scrape — drive a [`Scheduler`] with [`PrometheusSink`] attached,
//! then HTTP-GET `/metrics` and validate the response with the
//! upstream [`prometheus_parse`] crate. If a real Prometheus parser
//! accepts our output, every Prometheus client / Grafana plugin /
//! Mimir/VictoriaMetrics scraper will too.

use std::sync::Arc;

use graphreact_core::{
    runtime::types::{NodeContext, PermissionManifest},
    Attrs, CommunicationGraph, NodeDefinition, Scheduler, TelemetrySink,
};
use graphreact_observer::{spawn_http_server, BindConfig, MetricsInner, PrometheusSink};
use prometheus_parse::Scrape;

const RING_NODE_COUNT: usize = 4;
const MESSAGE_COUNT: usize = 100;

fn build_ring_scheduler() -> Scheduler {
    let mut graph = CommunicationGraph::new();
    for i in 0..RING_NODE_COUNT {
        graph.add_node(format!("n{i}"), Attrs::new());
    }
    for i in 0..RING_NODE_COUNT {
        let from = format!("n{i}");
        let to = format!("n{}", (i + 1) % RING_NODE_COUNT);
        graph.add_edge(from, to, None, Attrs::new());
    }
    let mut scheduler = Scheduler::new(graph);
    for i in 0..RING_NODE_COUNT {
        let id = format!("n{i}");
        let definition = NodeDefinition::new(&id, |_ctx: &NodeContext| {
            PermissionManifest::new(vec!["hop".to_string()], vec!["hop".to_string()])
        });
        scheduler
            .register_node(definition, id, Attrs::new(), 0)
            .expect("register");
    }
    scheduler.start().expect("start");
    scheduler
}

fn drive_messages(scheduler: &mut Scheduler, count: usize) {
    for r in 0..count {
        let to = format!("n{}", (r + 1) % RING_NODE_COUNT);
        scheduler.send(to, "hop", serde_json::json!({ "r": r }));
        scheduler.process_batch().expect("batch");
    }
}

#[tokio::test]
async fn scrape_returns_valid_prometheus_text_format() {
    let metrics = Arc::new(MetricsInner::new());
    let sink = PrometheusSink::from_arc(metrics.clone());

    let mut scheduler = build_ring_scheduler();
    scheduler.install_telemetry_sink(Arc::new(sink) as Arc<dyn TelemetrySink>);
    drive_messages(&mut scheduler, MESSAGE_COUNT);

    // Bind on an ephemeral port (port 0 → kernel-assigned). Avoids
    // collisions when multiple test processes run in parallel.
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let handle = spawn_http_server(bind, metrics).await.expect("spawn");
    let url = format!("http://{}/metrics", handle.local_addr);

    // Use a tokio TcpStream + raw HTTP/1.1 request. Avoids pulling
    // a `reqwest`/`hyper-client` dev-dep just for one GET.
    let body = http_get(&url).await;

    // Validate against the upstream parser. Any text-format drift
    // (missing `# TYPE`, malformed bucket lines, etc.) fails here.
    let lines: Vec<_> = body.lines().map(|l| Ok(l.to_string())).collect();
    let scrape = Scrape::parse(lines.into_iter()).expect("valid prometheus exposition format");

    // Per-test assertion: the 100 messages we drove all materialize
    // as `grx_messages_delivered_total{...}` series with a total
    // sum of 100.
    let delivered_total: f64 = scrape
        .samples
        .iter()
        .filter(|s| s.metric == "grx_messages_delivered_total")
        .map(|s| match &s.value {
            prometheus_parse::Value::Counter(v) => *v,
            other => panic!("expected counter, got {other:?}"),
        })
        .sum();
    assert!(
        (delivered_total - 100.0).abs() < 0.5,
        "expected 100 deliveries, got {delivered_total}"
    );

    // Spot-check that # HELP and # TYPE made it through.
    assert!(
        scrape
            .docs
            .iter()
            .any(|(name, _)| name == "grx_messages_delivered_total"),
        "missing HELP doc for grx_messages_delivered_total"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn healthz_always_200() {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let handle = spawn_http_server(bind, metrics).await.expect("spawn");
    let body = http_get(&format!("http://{}/healthz", handle.local_addr)).await;
    assert_eq!(body, "ok");
    handle.shutdown().await;
}

#[tokio::test]
async fn unknown_path_404s() {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let handle = spawn_http_server(bind, metrics).await.expect("spawn");
    let (status, _body) = http_get_status(&format!("http://{}/whatever", handle.local_addr)).await;
    assert_eq!(status, 404);
    handle.shutdown().await;
}

/// Bare-bones HTTP/1.1 GET that returns the response body. Only
/// used by tests; we stay dev-dep-light by avoiding `reqwest`.
async fn http_get(url: &str) -> String {
    let (status, body) = http_get_status(url).await;
    assert_eq!(status, 200, "expected 200 from {url}; body: {body}");
    body
}

async fn http_get_status(url: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    // url shape: http://host:port/path
    let after_scheme = url.strip_prefix("http://").expect("http:// scheme");
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(hp, p)| (hp, format!("/{p}")))
        .unwrap_or((after_scheme, "/".into()));

    let mut stream = TcpStream::connect(host_port).await.expect("tcp connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
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
    // Format: `HTTP/1.1 NNN reason`
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("u16 status");
    (status, body.to_string())
}
