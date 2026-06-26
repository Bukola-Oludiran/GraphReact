//! **M-Prod sub-track #1 Phase B Part 1.** End-to-end test for the
//! multi-token RBAC + per-route ACL substrate.
//!
//! Spawns the management API with a [`RotatableRegistry`] threaded
//! through [`SpawnOptions::rbac`] + a [`FileAuditSink`] wired
//! through [`SpawnOptions::audit_sink`], then exercises the
//! resulting routes under Admin / ReadOnly / Custom-only / expired /
//! garbage / no-Authorization scenarios. After teardown, the
//! NDJSON chain is replayed to verify:
//!
//! 1. The Phase A `AuditChain` fan invariant is preserved when RBAC
//!    is wired — `auth_success` and `authz_deny` event kinds slot
//!    in alongside `bind` / `auth_fail` without tearing the chain.
//! 2. The Phase A strict-additive contract holds — every
//!    pre-Part-1 record kind still appears with its expected field
//!    shape (verified by the `bind` record having the same
//!    `addr` / `tls` / `auth` fields it always did).
//! 3. The new RBAC verdicts (`auth_success` + `authz_deny`)
//!    surface the operator-assigned `token_id` string (NOT a hash,
//!    NOT the raw token) + the bounded `route` label + the
//!    `role` / `reason` strings.
//!
//! Pattern: matches the raw-tokio HTTP / TempDir / FileAuditSink
//! shape used by `tests/audit_chain_e2e.rs` so no new dev-deps
//! are introduced.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use graphreact_observer::{
    replay_audit_chain, spawn_management_api_with_options, BearerToken, BindConfig, FileAuditSink,
    MetricsInner, Role, RotatableRegistry, SnapshotRequest, SpawnOptions, TokenEntry, TokenId,
    TokenRegistry,
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

/// Spawn the management API with a multi-token registry + audit
/// sink. Returns the handle + a no-op drain task + the bound
/// address.
async fn spawn_with_rbac(
    rbac: RotatableRegistry,
    audit_path: &std::path::Path,
) -> (
    graphreact_observer::ApiServerHandle,
    JoinHandle<()>,
    std::net::SocketAddr,
) {
    let metrics = Arc::new(MetricsInner::new());
    // `bind.token = None` — when `rbac = Some`, the single-token
    // path is bypassed (the new `auth_layer` precedence check).
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let sink = Arc::new(FileAuditSink::open(audit_path).expect("open sink"))
        as Arc<dyn graphreact_observer::AuditSink>;
    let mut server = spawn_management_api_with_options(SpawnOptions {
        bind,
        metrics,
        process_start_id: 0xfeed_face,
        cluster_readyz: None,
        audit_sink: Some(sink),
        draining: None,
        rate_limit: None,
        rbac: Some(rbac),
        // M-Prod #6 Phase B Part 1 — RBAC tests don't exercise
        // the X-Forwarded-For opt-in path.
        trusted_proxies: None,
        // M-Prod #1 Phase B Part 2 (Group 1c) — JWT bridge unwired
        // for the static-token RBAC e2e.
        jwt_verifier: None,
    })
    .await
    .expect("spawn");
    let local_addr = server.local_addr;
    let mut snapshot_rx = std::mem::replace(&mut server.snapshot_rx, mpsc::channel(1).1);
    let drain_task = tokio::spawn(async move {
        while let Some(req) = snapshot_rx.recv().await {
            let _: SnapshotRequest = req;
        }
    });
    (server, drain_task, local_addr)
}

async fn http_get(url: &str, bearer: Option<&str>) -> (u16, String) {
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

/// **Verify-finding patch (MED).** POST helper for the two new
/// POST-route e2e tests. Sends a minimal JSON body — the actual
/// payload content doesn't matter because we expect the RBAC layer
/// to short-circuit (403 ReadOnly / past auth for Admin) BEFORE the
/// handler parses the body.
async fn http_post(url: &str, bearer: Option<&str>, json_body: &str) -> (u16, String) {
    use std::fmt::Write as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let parsed = url::parse_test_url(url);
    let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .await
        .expect("connect");
    let mut req = format!(
        "POST {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        parsed.path,
        parsed.host,
        parsed.port,
        json_body.len()
    );
    if let Some(b) = bearer {
        write!(req, "Authorization: Bearer {b}\r\n").expect("write to String");
    }
    req.push_str("\r\n");
    req.push_str(json_body);
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
/// only. Pulling a real URL crate for one integration test is overkill.
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

fn build_registry_admin_reader_expired() -> RotatableRegistry {
    let past = std::time::SystemTime::now() - Duration::from_secs(60);
    let entries = vec![
        TokenEntry {
            id: TokenId("admin1".into()),
            token: BearerToken::from_bytes(b"AAAA-admin-token-aaaa".to_vec()).unwrap(),
            role: Role::Admin,
            expires_at: None,
        },
        TokenEntry {
            id: TokenId("reader1".into()),
            token: BearerToken::from_bytes(b"BBBB-read-token-bbbb".to_vec()).unwrap(),
            role: Role::ReadOnly,
            expires_at: None,
        },
        TokenEntry {
            id: TokenId("expired1".into()),
            token: BearerToken::from_bytes(b"CCCC-expired-token-ccc".to_vec()).unwrap(),
            role: Role::Admin,
            expires_at: Some(past),
        },
    ];
    let registry = TokenRegistry::new(entries).expect("valid registry");
    RotatableRegistry::new(Arc::new(registry))
}

/// Public route (no auth required by the middleware). `/v1/healthz`
/// bypasses `auth_layer` entirely — verify no auth-side audit
/// records are produced.
#[tokio::test]
async fn healthz_bypasses_rbac() {
    let dir = TempDir::new("grx-rbac-e2e-healthz");
    let audit_path = dir.path.join("audit.ndjson");
    let rbac = build_registry_admin_reader_expired();
    let (server, drain, addr) = spawn_with_rbac(rbac, &audit_path).await;

    let (status, _body) = http_get(&format!("http://{addr}/v1/healthz"), None).await;
    assert_eq!(status, 200);

    tokio::time::sleep(Duration::from_millis(50)).await;
    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    // Bind record always lands. No auth-side records for the
    // unauthenticated public route.
    assert!(records.iter().any(|r| r.kind == "bind"), "bind record");
    assert!(
        !records.iter().any(|r| matches!(
            r.kind.as_str(),
            "auth_success" | "auth_fail" | "authz_deny"
        )),
        "no auth-side records on a public route, got: {records:?}"
    );
}

/// Admin role hits `/v1/identity` (a read-only route, but Admin
/// grants all protected routes) → 200 + `auth_success` chain
/// record with role=admin.
#[tokio::test]
async fn admin_token_succeeds_with_admin_role() {
    let dir = TempDir::new("grx-rbac-e2e-admin");
    let audit_path = dir.path.join("audit.ndjson");
    let rbac = build_registry_admin_reader_expired();
    let (server, drain, addr) = spawn_with_rbac(rbac, &audit_path).await;

    let (status, _body) = http_get(
        &format!("http://{addr}/v1/identity"),
        Some("AAAA-admin-token-aaaa"),
    )
    .await;
    assert_eq!(status, 200);

    tokio::time::sleep(Duration::from_millis(50)).await;
    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    let success = records
        .iter()
        .find(|r| r.kind == "auth_success")
        .expect("auth_success record");
    assert_eq!(success.fields["token_id"], "admin1");
    assert_eq!(success.fields["route"], "/v1/identity");
    assert_eq!(success.fields["role"], "admin");
}

/// ReadOnly role hits `/v1/identity` (in READ_ONLY_ROUTES) → 200 +
/// `auth_success` with role=read_only.
#[tokio::test]
async fn read_only_token_succeeds_on_get() {
    let dir = TempDir::new("grx-rbac-e2e-read");
    let audit_path = dir.path.join("audit.ndjson");
    let rbac = build_registry_admin_reader_expired();
    let (server, drain, addr) = spawn_with_rbac(rbac, &audit_path).await;

    let (status, _body) = http_get(
        &format!("http://{addr}/v1/identity"),
        Some("BBBB-read-token-bbbb"),
    )
    .await;
    assert_eq!(status, 200);

    tokio::time::sleep(Duration::from_millis(50)).await;
    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    let success = records
        .iter()
        .find(|r| r.kind == "auth_success")
        .expect("auth_success record");
    assert_eq!(success.fields["token_id"], "reader1");
    assert_eq!(success.fields["route"], "/v1/identity");
    assert_eq!(success.fields["role"], "read_only");
}

/// Expired token → 401 + `auth_fail` with reason=expired.
#[tokio::test]
async fn expired_token_fails_with_expired_reason() {
    let dir = TempDir::new("grx-rbac-e2e-expired");
    let audit_path = dir.path.join("audit.ndjson");
    let rbac = build_registry_admin_reader_expired();
    let (server, drain, addr) = spawn_with_rbac(rbac, &audit_path).await;

    let (status, _body) = http_get(
        &format!("http://{addr}/v1/identity"),
        Some("CCCC-expired-token-ccc"),
    )
    .await;
    assert_eq!(status, 401);

    tokio::time::sleep(Duration::from_millis(50)).await;
    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    let fail = records
        .iter()
        .find(|r| r.kind == "auth_fail")
        .expect("auth_fail record");
    assert_eq!(fail.fields["reason"], "expired");
    // **Verify-finding patch (MED).** The expired path KNOWS the
    // matched principal — assert the audit record carries token_id
    // so SOC pipelines can distinguish a known-principal credential-
    // aged-out from unknown wire-side junk that happened to be
    // expired-shaped.
    assert_eq!(
        fail.fields["token_id"], "expired1",
        "expired-path audit MUST record the matched token_id; \
         missing/wrong arms legitimately don't (no entry matched)"
    );
}

/// Garbage bearer → 401 + `auth_fail` with reason=wrong.
#[tokio::test]
async fn wrong_token_fails_with_wrong_reason() {
    let dir = TempDir::new("grx-rbac-e2e-wrong");
    let audit_path = dir.path.join("audit.ndjson");
    let rbac = build_registry_admin_reader_expired();
    let (server, drain, addr) = spawn_with_rbac(rbac, &audit_path).await;

    let (status, _body) = http_get(
        &format!("http://{addr}/v1/identity"),
        Some("garbage-not-in-registry"),
    )
    .await;
    assert_eq!(status, 401);

    tokio::time::sleep(Duration::from_millis(50)).await;
    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    let fail = records
        .iter()
        .find(|r| r.kind == "auth_fail")
        .expect("auth_fail record");
    assert_eq!(fail.fields["reason"], "wrong");
}

/// No Authorization header → 401 + `auth_fail` with reason=missing.
#[tokio::test]
async fn no_auth_header_fails_with_missing_reason() {
    let dir = TempDir::new("grx-rbac-e2e-missing");
    let audit_path = dir.path.join("audit.ndjson");
    let rbac = build_registry_admin_reader_expired();
    let (server, drain, addr) = spawn_with_rbac(rbac, &audit_path).await;

    let (status, _body) = http_get(&format!("http://{addr}/v1/identity"), None).await;
    assert_eq!(status, 401);

    tokio::time::sleep(Duration::from_millis(50)).await;
    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    let fail = records
        .iter()
        .find(|r| r.kind == "auth_fail")
        .expect("auth_fail record");
    assert_eq!(fail.fields["reason"], "missing");
}

/// RBAC deny: Custom role with allowlist that does NOT include
/// `/v1/identity` → 403 + `authz_deny` chain record with
/// reason=rbac. Brief sidesteps the "no admin-only Phase A
/// protected route exists yet" gap by using a Custom role rather
/// than relying on a not-yet-shipped admin-only label.
#[tokio::test]
async fn custom_role_denies_unlisted_route() {
    let dir = TempDir::new("grx-rbac-e2e-deny");
    let audit_path = dir.path.join("audit.ndjson");
    // T_custom_metrics_only: Custom role with /v1/metrics ONLY.
    let mut allow = BTreeSet::new();
    allow.insert("/v1/metrics".to_string());
    let entries = vec![TokenEntry {
        id: TokenId("metrics-only".into()),
        token: BearerToken::from_bytes(b"DDDD-metrics-only-token-d".to_vec()).unwrap(),
        role: Role::Custom(allow),
        expires_at: None,
    }];
    let registry = TokenRegistry::new(entries).expect("valid registry");
    let rbac = RotatableRegistry::new(Arc::new(registry));
    let (server, drain, addr) = spawn_with_rbac(rbac, &audit_path).await;

    // /v1/identity is NOT in the Custom allowlist → 403.
    let (status, _body) = http_get(
        &format!("http://{addr}/v1/identity"),
        Some("DDDD-metrics-only-token-d"),
    )
    .await;
    assert_eq!(status, 403);

    tokio::time::sleep(Duration::from_millis(50)).await;
    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    let deny = records
        .iter()
        .find(|r| r.kind == "authz_deny")
        .expect("authz_deny record");
    assert_eq!(deny.fields["token_id"], "metrics-only");
    assert_eq!(deny.fields["route"], "/v1/identity");
    assert_eq!(deny.fields["role"], "custom");
    assert_eq!(deny.fields["reason"], "rbac");
    // No auth_success for the denied request.
    assert!(
        !records.iter().any(|r| r.kind == "auth_success"),
        "no auth_success when the role denies the route"
    );
}

/// **Verify-finding patch (MED).** ReadOnly token hitting a POST
/// route NOT in `READ_ONLY_ROUTES` (`/v1/nodes/:id/messages` per
/// `PROTECTED_ROUTE_LABELS`) is denied with 403 + `authz_deny`
/// audit row carrying the live classified route label. Proves the
/// three pieces of the wire-up compose at the live HTTP layer:
/// (a) `classify_protected_path` maps `/v1/nodes/test-node/messages`
/// to `/v1/nodes/:id/messages`, (b) `auth_layer_rbac` passes that
/// label to `check_route`, (c) `Role::ReadOnly` rejects it. Prior
/// tests pin (a) and (c) as unit tests; this one closes (b).
#[tokio::test]
async fn read_only_token_denied_on_post_route() {
    let dir = TempDir::new("grx-rbac-e2e-readonly-post");
    let audit_path = dir.path.join("audit.ndjson");
    let rbac = build_registry_admin_reader_expired();
    let (server, drain, addr) = spawn_with_rbac(rbac, &audit_path).await;

    // POST /v1/nodes/test-node/messages with ReadOnly token. RBAC
    // short-circuits with 403 BEFORE the handler runs, so the
    // hung-handler problem (no scheduler in this test rig) is moot.
    let (status, _body) = http_post(
        &format!("http://{addr}/v1/nodes/test-node/messages"),
        Some("BBBB-read-token-bbbb"),
        r#"{"type":"ping"}"#,
    )
    .await;
    assert_eq!(status, 403, "ReadOnly MUST be denied on POST routes");

    tokio::time::sleep(Duration::from_millis(50)).await;
    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    let deny = records
        .iter()
        .find(|r| r.kind == "authz_deny")
        .expect("authz_deny record");
    assert_eq!(deny.fields["token_id"], "reader1");
    assert_eq!(
        deny.fields["route"], "/v1/nodes/:id/messages",
        "classified route label MUST be the static-str pattern from PROTECTED_ROUTE_LABELS, \
         not the raw URI with substituted :id"
    );
    assert_eq!(deny.fields["role"], "read_only");
    assert_eq!(deny.fields["reason"], "rbac");
    // No auth_success for the denied request.
    assert!(
        !records.iter().any(|r| r.kind == "auth_success"),
        "no auth_success when the role denies the route"
    );
}

/// **Verify-finding patch (MED).** Admin token on the same POST
/// route as the ReadOnly-deny test passes auth + RBAC — `auth_success`
/// audit fires with `role=admin` and the same classified route
/// label. The actual handler will hang in this rig (no scheduler
/// reply-task wired) so the test wraps the HTTP call in a short
/// timeout and asserts the audit chain rather than the response
/// status. The contract under test is "auth + RBAC pass for Admin";
/// the handler's downstream behavior is out of scope here.
#[tokio::test]
async fn admin_token_passes_rbac_on_post_route() {
    let dir = TempDir::new("grx-rbac-e2e-admin-post");
    let audit_path = dir.path.join("audit.ndjson");
    let rbac = build_registry_admin_reader_expired();
    let (server, drain, addr) = spawn_with_rbac(rbac, &audit_path).await;

    // Wrap in a short timeout; handler-hang is expected in this rig.
    let _ = tokio::time::timeout(
        Duration::from_millis(750),
        http_post(
            &format!("http://{addr}/v1/nodes/test-node/messages"),
            Some("AAAA-admin-token-aaaa"),
            r#"{"type":"ping"}"#,
        ),
    )
    .await;

    // Audit emission happens BEFORE the handler runs, so the chain
    // has the auth_success record even though we didn't wait for
    // the response.
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    let success = records
        .iter()
        .find(|r| r.kind == "auth_success")
        .expect("auth_success record");
    assert_eq!(success.fields["token_id"], "admin1");
    assert_eq!(
        success.fields["route"], "/v1/nodes/:id/messages",
        "classified route label MUST be the static-str pattern"
    );
    assert_eq!(success.fields["role"], "admin");
    // No authz_deny for the Admin path.
    assert!(
        !records.iter().any(|r| r.kind == "authz_deny"),
        "no authz_deny when Admin role grants the route"
    );
}

/// Audit-chain integrity assertion: after a sequence of mixed
/// outcomes (success + deny + miss), the NDJSON chain replays
/// cleanly — the new event kinds `auth_success` + `authz_deny`
/// don't tear the chain. Pins the Phase A AuditChain fan
/// invariant + strict-additive contract.
#[tokio::test]
async fn chain_integrity_holds_across_mixed_rbac_outcomes() {
    let dir = TempDir::new("grx-rbac-e2e-chain");
    let audit_path = dir.path.join("audit.ndjson");
    let rbac = build_registry_admin_reader_expired();
    let (server, drain, addr) = spawn_with_rbac(rbac, &audit_path).await;

    // Mix of outcomes: success + success + fail + fail.
    let _ = http_get(
        &format!("http://{addr}/v1/identity"),
        Some("AAAA-admin-token-aaaa"),
    )
    .await;
    let _ = http_get(
        &format!("http://{addr}/v1/identity"),
        Some("BBBB-read-token-bbbb"),
    )
    .await;
    let _ = http_get(&format!("http://{addr}/v1/identity"), Some("garbage")).await;
    let _ = http_get(&format!("http://{addr}/v1/identity"), None).await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    server.shutdown().await;
    drain.abort();

    let records = replay_audit_chain(&audit_path).expect("chain ok");
    // Chain links: each record's prev_hash matches the prior
    // record's this_hash. Pins the Phase A invariant survives
    // the new event kinds.
    for win in records.windows(2) {
        assert_eq!(win[1].prev_hash, win[0].this_hash);
        assert_eq!(win[1].seq, win[0].seq + 1);
    }
    // Phase A strict-additive contract: bind record still has the
    // same field shape it always did.
    let bind = records
        .iter()
        .find(|r| r.kind == "bind")
        .expect("bind record");
    assert_eq!(bind.fields["tls"], serde_json::Value::Bool(false));
    // Auth was wired (rbac was Some) so the Phase A bind audit
    // emits `auth: false` because the legacy `bind.token` was
    // `None`. This is the documented Phase A semantic: `auth`
    // reflects the legacy single-token field, not the RBAC handle.
    // Phase B Part 2 can extend the bind audit fields if richer
    // bootstrap visibility is desired.
    assert_eq!(bind.fields["auth"], serde_json::Value::Bool(false));

    // Verify the mixed-outcome record kinds are all present.
    let kinds: BTreeSet<&str> = records.iter().map(|r| r.kind.as_str()).collect();
    assert!(kinds.contains("bind"));
    assert!(kinds.contains("auth_success"));
    assert!(kinds.contains("auth_fail"));
}
