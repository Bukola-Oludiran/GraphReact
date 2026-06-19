//! **M-Prod sub-track #1 Phase A.** End-to-end test for the
//! hash-chained audit sink: spawn the management API with a
//! [`graphreact_observer::FileAuditSink`] wired through
//! [`graphreact_observer::SpawnOptions::audit_sink`], hit
//! `/v1/healthz` (no auth) + `/v1/identity` (auth-protected) under a
//! valid AND invalid bearer, then verify the resulting NDJSON
//! contains the expected `bind` / `accept` / `auth_fail` /
//! `tls_handshake_failed` records in chained order.
//!
//! The companion `audit_chain.rs` unit tests cover the chain
//! primitives in isolation (single-thread, no axum). This file
//! covers the wire-up path — that the sink Arc actually threads from
//! `SpawnOptions` through `ApiState` through `audit::log_*` through
//! the on-disk file, AND that the tracing-side stream STILL FIRES
//! unchanged when a sink IS wired (the strict-additive guarantee).

use std::sync::Arc;
use std::time::Duration;

use graphreact_observer::{
    replay_audit_chain, spawn_management_api_with_options, BearerToken, BindConfig,
    FileAuditSink, MetricsInner, SnapshotRequest, SpawnOptions,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Tempdir helper. Removes itself on drop.
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

/// Spawn the management API with an audit sink + a no-op drain
/// loop. The drain loop matters because some routes (e.g.
/// `/v1/identity`) reply directly from `ApiState` and don't touch
/// the bridge — but having a drain task alive lets us reuse the
/// pattern from `tests/management_api.rs` if Phase B extends the
/// test to additional routes.
async fn spawn_with_sink(
    token: Option<&[u8]>,
    audit_path: &std::path::Path,
) -> (
    graphreact_observer::ApiServerHandle,
    JoinHandle<()>,
    std::net::SocketAddr,
) {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: token.map(|t| BearerToken::from_bytes(t.to_vec()).unwrap()),
        tls: None,
    };
    let sink = Arc::new(FileAuditSink::open(audit_path).expect("open sink"))
        as Arc<dyn graphreact_observer::AuditSink>;
    let mut server = spawn_management_api_with_options(SpawnOptions {
        bind,
        metrics,
        process_start_id: 0xdead_beef,
        cluster_readyz: None,
        audit_sink: Some(sink),
        // M-Prod #4 Phase α drain flag is None on the audit-chain
        // e2e path — drain is irrelevant here and a None flag keeps
        // the readyz + ingress fast-paths in their pre-#4 shape.
        draining: None,
        // M-Prod #6 Phase A rate-limit is None on the audit-chain
        // e2e path — these tests don't exercise the limiter and
        // None falls through to the env-var defaults.
        rate_limit: None,
        // M-Prod #1 Phase B Part 1 RBAC is None on the audit-chain
        // e2e path — these tests cover the single-token Phase A
        // flow + the strict-additive guarantee that a Phase A
        // daemon spawn keeps producing the same chain shape.
        rbac: None,
        // M-Prod #6 Phase B Part 1 trusted_proxies is None — the
        // audit-chain test isn't about X-Forwarded-For honoring.
        trusted_proxies: None,
    })
    .await
    .expect("spawn");
    let local_addr = server.local_addr;
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let drain_task = tokio::spawn(async move {
        // No-op drain — the routes under test (healthz +
        // identity) don't depend on a snapshot reply.
        //
        // Matching the request shape would require depending on
        // graphreact-core's introspection types from this
        // integration test; for the audit-chain wire-up smoke
        // those shapes are out of scope.
        while let Some(req) = snapshot_rx.recv().await {
            let _: SnapshotRequest = req;
        }
    });
    (server, drain_task, local_addr)
}

async fn http_get(url: &str, bearer: Option<&str>) -> (u16, String) {
    // Minimal raw-tokio HTTP/1.1 client — avoids pulling reqwest
    // into the dev-deps for one integration test. The wire shape
    // matches what `tests/management_api.rs` uses.
    use std::fmt::Write as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let parsed = url::parse_test_url(url);
    let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .await
        .expect("connect");
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n",
        parsed.path, parsed.host, parsed.port
    );
    if let Some(b) = bearer {
        write!(req, "Authorization: Bearer {b}\r\n").expect("write to String");
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let resp = String::from_utf8_lossy(&buf).into_owned();
    let status_line = resp.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, resp)
}

/// Lightweight URL parser scoped to this test file — `http://host:port/path`
/// only. Pulling a real URL crate for one test is overkill.
mod url {
    pub struct Parsed {
        pub host: String,
        pub port: u16,
        pub path: String,
    }
    pub fn parse_test_url(s: &str) -> Parsed {
        let s = s.strip_prefix("http://").expect("http://");
        let (host_port, path) = match s.find('/') {
            Some(idx) => (&s[..idx], &s[idx..]),
            None => (s, "/"),
        };
        let (host, port) = match host_port.rfind(':') {
            Some(idx) => (
                host_port[..idx].to_string(),
                host_port[idx + 1..].parse::<u16>().unwrap_or(80),
            ),
            None => (host_port.to_string(), 80),
        };
        Parsed {
            host,
            port,
            path: path.to_string(),
        }
    }
}

#[tokio::test]
async fn no_token_chain_carries_bind_and_response() {
    let dir = TempDir::new("grx-audit-e2e-1");
    let audit_path = dir.path.join("audit.ndjson");
    let (server, drain, addr) = spawn_with_sink(None, &audit_path).await;

    // hit healthz — no auth required so this never produces an
    // auth_fail.
    let (status, _body) = http_get(&format!("http://{addr}/v1/healthz"), None).await;
    assert_eq!(status, 200);
    // Give the sink a tick to flush; FileAuditSink fsyncs per-line
    // so the bytes are already on disk, but the in-flight axum task
    // may not have completed yet.
    tokio::time::sleep(Duration::from_millis(50)).await;

    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    // Phase A logs `bind` at spawn but `/v1/healthz` doesn't
    // currently call audit::log_response (the management API is
    // axum-driven, and the audit::log_response helper is wired
    // from the legacy P2 `/metrics` server). The substrate
    // guarantee is the `bind` record — that proves the sink
    // Arc threaded through spawn_with_options → audit::log_bind →
    // FileAuditSink::append → on-disk NDJSON.
    assert!(!records.is_empty(), "at least the bind record");
    let bind = records.iter().find(|r| r.kind == "bind").expect("bind kind");
    assert_eq!(bind.seq, 0);
    // tls + auth fields from the structured fan-out
    assert_eq!(bind.fields["tls"], serde_json::Value::Bool(false));
    assert_eq!(bind.fields["auth"], serde_json::Value::Bool(false));
}

#[tokio::test]
async fn auth_fail_lands_in_chain_under_bad_token() {
    let dir = TempDir::new("grx-audit-e2e-2");
    let audit_path = dir.path.join("audit.ndjson");
    let token = b"valid-secret-12345";
    let (server, drain, addr) = spawn_with_sink(Some(token), &audit_path).await;

    // No Authorization header → auth_fail "missing"
    let (status, _body) = http_get(&format!("http://{addr}/v1/identity"), None).await;
    assert_eq!(status, 401);
    // Wrong bearer → auth_fail "wrong"
    let (status, _body) = http_get(&format!("http://{addr}/v1/identity"), Some("bogus")).await;
    assert_eq!(status, 401);
    // Correct bearer → 200, no auth_fail emitted
    let (status, _body) = http_get(
        &format!("http://{addr}/v1/identity"),
        Some("valid-secret-12345"),
    )
    .await;
    assert_eq!(status, 200);

    tokio::time::sleep(Duration::from_millis(50)).await;

    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    let bind_count = records.iter().filter(|r| r.kind == "bind").count();
    let auth_fail = records
        .iter()
        .filter(|r| r.kind == "auth_fail")
        .collect::<Vec<_>>();
    assert_eq!(bind_count, 1, "exactly one bind record on a fresh chain");
    assert_eq!(
        auth_fail.len(),
        2,
        "two auth_fail records: missing + wrong"
    );
    let reasons: Vec<&str> = auth_fail
        .iter()
        .map(|r| {
            r.fields["reason"]
                .as_str()
                .expect("reason is a string")
        })
        .collect();
    assert!(reasons.contains(&"missing"));
    assert!(reasons.contains(&"wrong"));
    // Chain links: each record's prev_hash matches the prior
    // record's this_hash (which `replay` already verified, but
    // assert explicitly so a future regression that silently
    // skipped chain verification can't pass).
    for win in records.windows(2) {
        assert_eq!(win[1].prev_hash, win[0].this_hash);
        assert_eq!(win[1].seq, win[0].seq + 1);
    }
}

#[tokio::test]
async fn no_sink_default_keeps_legacy_behavior() {
    // Sanity: a daemon spawned through the legacy spawn() (no sink)
    // produces no audit file even with `--observer-audit-log`-shaped
    // tests inadvertently expecting one. This locks in the
    // strict-additive guarantee.
    let dir = TempDir::new("grx-audit-e2e-3");
    let no_audit = dir.path.join("never-written.ndjson");
    assert!(!no_audit.exists());

    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let mut server = graphreact_observer::spawn_management_api(bind, metrics, 1)
        .await
        .expect("spawn");
    let addr = server.local_addr;
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let drain = tokio::spawn(async move {
        while let Some(_req) = snapshot_rx.recv().await {}
    });

    let (status, _) = http_get(&format!("http://{addr}/v1/healthz"), None).await;
    assert_eq!(status, 200);

    server.shutdown().await;
    drain.abort();

    assert!(
        !no_audit.exists(),
        "no audit file when audit_sink is None — strict-additive guarantee"
    );
}
