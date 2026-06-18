//! **M-Prod sub-track #6 Phase A verification gate.** End-to-end
//! per-IP rate-limit tests: spawn the management API with a
//! deterministic [`graphreact_observer::RateLimitConfig`] (low
//! `per_second` + tiny `burst`), drive synthetic burst traffic
//! through a real TCP listener, then assert:
//!
//! 1. A burst above cap yields ≥1 `429 Too Many Requests` with the
//!    canonical `{error: "rate_limited", retry_after_secs}` body +
//!    `Retry-After` header.
//! 2. The public branch (`/v1/healthz`, `/v1/readyz`) NEVER 429s
//!    no matter how hard we hammer it — k8s liveness + readiness
//!    probes ride a path that the limiter never touches.
//! 3. The `grx_rate_limit_blocked_total{route}` Prometheus counter
//!    increments under burst — same value the
//!    `rate_limit_blocked` audit event reports.
//! 4. The audit-sink fan emits a `rate_limit_blocked` record per
//!    429, with the bounded route label + the peer IP (NEVER the
//!    token or payload bytes).
//!
//! Tests inject `RateLimitConfig::for_tests(per_second, burst)` via
//! `SpawnOptions::rate_limit = Some(...)` so parallel `cargo test`
//! runners don't collide on the `GRX_RATE_LIMIT_PER_IP_*` env vars.

use std::sync::Arc;
use std::time::Duration;

use graphreact_observer::{
    replay_audit_chain, spawn_management_api_with_options, BearerToken, BindConfig,
    FileAuditSink, MetricsInner, RateLimitConfig, SnapshotRequest, SpawnOptions,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Tempdir helper. Removes itself on drop. Mirrors the shape used
/// in `tests/audit_chain_e2e.rs` so the two test files stay
/// trivially diffable.
struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("{tag}-{pid}-{n}"));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Spawn the management API with a deterministic rate-limit cap
/// + no-op drain task + optional audit sink.
///
/// Returns the server handle, the drain task handle (for clean
/// teardown), the bound local addr, and the shared
/// `Arc<MetricsInner>` so individual tests can scrape the counter
/// without a second HTTP round-trip.
async fn spawn_with_limit(
    rate_limit: RateLimitConfig,
    token: Option<&[u8]>,
    audit_path: Option<&std::path::Path>,
) -> (
    graphreact_observer::ApiServerHandle,
    JoinHandle<()>,
    std::net::SocketAddr,
    Arc<MetricsInner>,
) {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: token.map(|t| BearerToken::from_bytes(t.to_vec()).unwrap()),
        tls: None,
    };
    let audit_sink: Option<Arc<dyn graphreact_observer::AuditSink>> =
        audit_path.map(|p| {
            Arc::new(FileAuditSink::open(p).expect("open sink")) as Arc<dyn graphreact_observer::AuditSink>
        });
    let mut server = spawn_management_api_with_options(SpawnOptions {
        bind,
        metrics: metrics.clone(),
        process_start_id: 0xdead_beef,
        cluster_readyz: None,
        audit_sink,
        draining: None,
        rate_limit: Some(rate_limit),
        // M-Prod #1 Phase B Part 1 RBAC is None on the rate-limit
        // e2e path — these tests exercise the per-IP rate-limit
        // middleware sitting OUTSIDE the auth layer, so the
        // auth path under test stays in the Phase A single-token
        // shape.
        rbac: None,
    })
    .await
    .expect("spawn");
    let local_addr = server.local_addr;
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let drain_task = tokio::spawn(async move {
        // No-op drain — the routes under test (identity / healthz
        // / readyz / metrics) don't depend on a snapshot reply.
        while let Some(req) = snapshot_rx.recv().await {
            let _: SnapshotRequest = req;
        }
    });
    (server, drain_task, local_addr, metrics)
}

/// Send N back-to-back HTTP/1.1 GETs from a single TCP connection,
/// each on a fresh socket so tower_governor's PeerIpKeyExtractor
/// sees the same `127.0.0.1` source for every request. Returns the
/// list of `(status_code, retry_after, body)` triples in order.
async fn flood_get_n(
    addr: std::net::SocketAddr,
    path: &str,
    bearer: Option<&str>,
    n: usize,
) -> Vec<(u16, Option<String>, String)> {
    use std::fmt::Write as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut stream = TcpStream::connect(addr).await.expect("tcp connect");
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n"
        );
        if let Some(b) = bearer {
            write!(req, "Authorization: Bearer {b}\r\n").expect("write to string");
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).await.expect("send req");
        let mut buf = Vec::new();
        stream
            .read_to_end(&mut buf)
            .await
            .expect("read response");
        let resp = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = resp
            .split_once("\r\n\r\n")
            .map(|(h, b)| (h.to_string(), b.to_string()))
            .unwrap_or((resp, String::new()));
        let mut lines = head.lines();
        let status_line = lines.next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let mut retry_after = None;
        for h in lines {
            if let Some((name, value)) = h.split_once(':') {
                if name.eq_ignore_ascii_case("retry-after") {
                    retry_after = Some(value.trim().to_string());
                }
            }
        }
        out.push((status, retry_after, body));
    }
    out
}

/// **Gate 1.** Driving > `burst` requests in a tight loop against a
/// protected route from one IP MUST surface ≥1 `429 Too Many
/// Requests` with the canonical body + Retry-After header.
#[tokio::test]
async fn rate_limit_layer_emits_429_under_burst() {
    // burst=2, per_second=1 — three requests in the same wall-
    // clock millisecond MUST trip the limiter (the first two pass
    // the burst; the third has no tokens left and 1s hasn't
    // replenished one).
    let rl = RateLimitConfig::for_tests(1, 2);
    let (server, drain, addr, _metrics) = spawn_with_limit(rl, None, None).await;

    // Hit /v1/identity (auth-bypassed because no token configured)
    // 10 times — burst=2 means the first 2 pass, the remainder
    // should 429.
    let url_path = "/v1/identity";
    let responses = flood_get_n(addr, url_path, None, 10).await;
    let blocked: Vec<&(u16, Option<String>, String)> = responses
        .iter()
        .filter(|(s, _, _)| *s == 429)
        .collect();
    assert!(
        !blocked.is_empty(),
        "expected ≥1 429 in burst run; got statuses: {:?}",
        responses.iter().map(|(s, _, _)| s).collect::<Vec<_>>()
    );
    // Shape check on a sampled 429: canonical body + Retry-After.
    let (_, retry_after, body) = blocked[0];
    assert!(
        body.contains("\"error\":\"rate_limited\""),
        "expected rate_limited body discriminator, got: {body}"
    );
    assert!(
        body.contains("\"retry_after_secs\":"),
        "expected retry_after_secs field, got: {body}"
    );
    assert!(
        retry_after.is_some(),
        "Retry-After header required on 429; got headers without it"
    );

    server.shutdown().await;
    drain.abort();
}

/// **Gate 2.** Public routes (`/v1/healthz`, `/v1/readyz`) MUST
/// stay unrated — k8s probes that hammer those at high frequency
/// MUST NOT see a 429. Phase A applies the GovernorLayer to the
/// protected branch only; this test pins that property so a
/// regression that mounts governor on the merged router fails
/// loudly here.
#[tokio::test]
async fn rate_limit_does_not_apply_to_public_routes() {
    // Aggressively tight cap (burst=2, per_second=1) — if public
    // routes WERE rated, a 200-request flood would trivially 429.
    let rl = RateLimitConfig::for_tests(1, 2);
    let (server, drain, addr, _metrics) = spawn_with_limit(rl, None, None).await;

    let healthz = flood_get_n(addr, "/v1/healthz", None, 200).await;
    let readyz = flood_get_n(addr, "/v1/readyz", None, 200).await;

    let healthz_429s = healthz.iter().filter(|(s, _, _)| *s == 429).count();
    let readyz_429s = readyz.iter().filter(|(s, _, _)| *s == 429).count();

    assert_eq!(
        healthz_429s, 0,
        "/v1/healthz must never 429 (k8s liveness probe); got {healthz_429s} 429s"
    );
    assert_eq!(
        readyz_429s, 0,
        "/v1/readyz must never 429 (k8s readiness probe); got {readyz_429s} 429s"
    );

    server.shutdown().await;
    drain.abort();
}

/// **Gate 3.** `grx_rate_limit_blocked_total{route}` counter
/// increments under burst — same value the audit event reports.
/// Scrapes `/v1/metrics` after triggering the limiter, parses the
/// counter line, asserts > 0.
#[tokio::test]
async fn grx_rate_limit_blocked_total_counter_increments() {
    let rl = RateLimitConfig::for_tests(1, 2);
    let (server, drain, addr, metrics) = spawn_with_limit(rl, None, None).await;

    // Trigger the limiter against a known route — burst=2 means
    // first 2 pass, next ones 429.
    let responses = flood_get_n(addr, "/v1/identity", None, 10).await;
    let blocked_count = responses.iter().filter(|(s, _, _)| *s == 429).count();
    assert!(
        blocked_count > 0,
        "no 429s fired — test setup wrong, can't verify counter"
    );

    // Direct in-process counter read — the metric callsite lives
    // inside `rate_limit_observer` which has already fired by the
    // time the HTTP response returned, so a synchronous read of
    // the shared `MetricsInner` reflects the same count.
    let entries: Vec<u64> = metrics
        .rate_limit_blocked
        .iter()
        .map(|e| e.value().counter.get())
        .collect();
    let total: u64 = entries.iter().sum();
    assert_eq!(
        total, blocked_count as u64,
        "metric total ({total}) should equal blocked_count ({blocked_count})"
    );

    // Also confirm via the Prometheus encoder — scraping
    // `/v1/metrics` produces the same value, with the bounded
    // route label. This is the operator-visible surface. We have
    // to wait long enough for the token bucket to replenish past
    // the burst — `/v1/metrics` is itself a protected route, so
    // a scrape immediately after the burst would itself 429.
    // burst=2, per_second=1 → wait 3s + retry until we get a
    // successful scrape.
    let mut metrics_body = String::new();
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(700)).await;
        metrics_body = scrape_metrics(addr).await;
        if metrics_body.contains("grx_rate_limit_blocked_total") {
            break;
        }
    }
    assert!(
        metrics_body.contains("grx_rate_limit_blocked_total{route=\"/v1/identity\"}"),
        "expected counter line for /v1/identity in the Prometheus scrape; got body:\n{metrics_body}"
    );

    server.shutdown().await;
    drain.abort();
}

/// **Gate 4.** Audit event `rate_limit_blocked` lands in the
/// hash-chained sink when the limiter rejects. Fields carry
/// peer + route; no token / payload bytes.
#[tokio::test]
async fn rate_limit_blocked_audit_event_fires() {
    let dir = TempDir::new("grx-ratelimit-audit");
    let audit_path = dir.path.join("audit.ndjson");
    let rl = RateLimitConfig::for_tests(1, 2);
    let (server, drain, addr, _metrics) =
        spawn_with_limit(rl, None, Some(&audit_path)).await;

    let responses = flood_get_n(addr, "/v1/identity", None, 10).await;
    let blocked_count = responses.iter().filter(|(s, _, _)| *s == 429).count();
    assert!(blocked_count > 0);

    // Give the sink a tick to flush the NDJSON line — fsync per
    // line is synchronous on the call site but the axum task
    // running rate_limit_observer may not have fully returned.
    tokio::time::sleep(Duration::from_millis(50)).await;

    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    let blocked: Vec<_> = records
        .iter()
        .filter(|r| r.kind == "rate_limit_blocked")
        .collect();
    assert_eq!(
        blocked.len(),
        blocked_count,
        "audit record count ({}) should equal HTTP 429 count ({blocked_count})",
        blocked.len()
    );
    // Shape check on the first audit record: peer + route are
    // present; token / payload bytes are NOT present.
    let first = blocked[0];
    let peer = first.fields["peer"]
        .as_str()
        .expect("peer field is a string");
    assert!(
        peer.starts_with("127.0.0.1:"),
        "peer should be loopback ip:port, got {peer}"
    );
    let route = first.fields["route"]
        .as_str()
        .expect("route field is a string");
    assert_eq!(route, "/v1/identity");
    // Defensive — confirm the audit shape does NOT leak the
    // bearer token or any payload bytes via stringly-typed field
    // names operators might trust.
    assert!(first.fields.get("token").is_none());
    assert!(first.fields.get("payload").is_none());
    assert!(first.fields.get("authorization").is_none());
}

/// Public regression test for the layer-ordering invariant: when
/// the limit is well above the test's request rate, NO 429 fires
/// even with a sustained burst — proves the limiter doesn't
/// false-positive on legitimate traffic.
#[tokio::test]
async fn rate_limit_passes_under_cap() {
    // Cap well above test rate — 1000 burst, 100/s. Send 50
    // sequential requests; none should 429.
    let rl = RateLimitConfig::for_tests(100, 1000);
    let (server, drain, addr, _metrics) = spawn_with_limit(rl, None, None).await;

    let responses = flood_get_n(addr, "/v1/identity", None, 50).await;
    let blocked = responses.iter().filter(|(s, _, _)| *s == 429).count();
    assert_eq!(
        blocked, 0,
        "no 429s expected under cap; got {blocked} from {} responses",
        responses.len()
    );
    // Also confirm every response was 200 OK (not 503 / 401 /
    // other surprise) — the chain is intact end-to-end.
    let ok = responses.iter().filter(|(s, _, _)| *s == 200).count();
    assert_eq!(
        ok,
        responses.len(),
        "all responses should be 200 under cap; got status distribution: {:?}",
        responses.iter().map(|(s, _, _)| s).collect::<Vec<_>>()
    );

    server.shutdown().await;
    drain.abort();
}

/// Helper: fetch `/v1/metrics` as a `String` for the
/// counter-shape assertion above.
async fn scrape_metrics(addr: std::net::SocketAddr) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let mut stream = TcpStream::connect(addr).await.expect("tcp connect");
    let req = format!(
        "GET /v1/metrics HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.expect("send");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let resp = String::from_utf8_lossy(&buf).into_owned();
    resp.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or(resp)
}
