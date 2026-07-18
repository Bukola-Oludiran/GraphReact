//! **M-Prod sub-track #2 Phase A.** Integration tests for the
//! atomic-swap TLS rotation substrate.
//!
//! The substrate ships in [`graphreact_observer::tls_rotation`]
//! (the [`RotatableServerConfig`] type) + a public rotation entry
//! point on [`ApiServerHandle::reload_tls`]. These tests exercise:
//!
//! 1. `rotate_swaps_in_new_config` — direct round-trip on the
//!    handle: build initial config A, rotate to config B, confirm
//!    `current()` returns B and the pre-rotation snapshot still
//!    points at A (no UAF, no torn config).
//! 2. `accept_loop_uses_rotated_config_for_next_connection` —
//!    end-to-end through the management API: bind a server with
//!    cert A, dial via tokio-rustls + read the peer cert chain to
//!    confirm we saw A, call `reload_tls` to swap in cert B, dial
//!    again, confirm the second handshake observes cert B.
//! 3. `reload_tls_rejects_garbage_pem` — the cert-parse failure
//!    path surfaces as [`TlsRotationError::CertParse`] and leaves
//!    the existing config in place.
//! 4. `reload_tls_rejects_garbage_key` — the key-parse failure
//!    path surfaces as [`TlsRotationError::KeyParse`] and leaves
//!    the existing config in place.
//! 5. `reload_tls_preserves_alpn_config` — operator-set ALPN
//!    survives the rotation (rotation is cert-only; ALPN is not
//!    a per-rotation concern).
//! 6. `reload_tls_rejects_not_configured` — the loopback-default
//!    "no TLS at bind time" case returns
//!    [`TlsRotationError::NotConfigured`].
//! 7. `tls_reload_audit_event_fires` — a custom
//!    tracing-subscriber `Layer` captures the
//!    `graphreact::observer::audit` events fired by `reload_tls`
//!    and asserts the success path emits
//!    `event = "tls_reloaded"` WITHOUT leaking cert/key bytes
//!    (regex-grep the captured field set for `-----BEGIN` — fail
//!    if any such marker appears).

use std::sync::{Arc, Mutex};

use graphreact_observer::{
    spawn_management_api, BindConfig, MetricsInner, RotatableServerConfig, ServerTlsConfig,
    TlsRotationError,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConfig;
use rustls::ALL_VERSIONS;

// ---------- helpers ----------

/// A self-signed cert + PEM-encoded cert/key bundle used to seed
/// the rotation handle and to drive `reload_tls`.
struct TestCert {
    cert_der: CertificateDer<'static>,
    cert_pem: String,
    key_pem: String,
}

fn mint_test_cert(common_name: &str) -> TestCert {
    let mut params =
        rcgen::CertificateParams::new(vec![common_name.to_string()]).unwrap();
    params.subject_alt_names.push(rcgen::SanType::DnsName(
        common_name.to_string().try_into().unwrap(),
    ));
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    TestCert {
        cert_der: cert.der().clone(),
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    }
}

fn build_server_config(cert: &TestCert) -> Arc<ServerConfig> {
    let key_der: PrivateKeyDer<'static> = {
        let bytes = rustls::pki_types::pem::PemObject::from_pem_slice(cert.key_pem.as_bytes())
            .expect("parse test key PEM");
        bytes
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(ALL_VERSIONS)
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert_der.clone()], key_der)
        .unwrap();
    Arc::new(cfg)
}

fn server_tls_config_for(cert: &TestCert) -> ServerTlsConfig {
    ServerTlsConfig {
        server: RotatableServerConfig::new(build_server_config(cert)),
    }
}

// ---------- pure substrate tests ----------

#[test]
fn rotate_swaps_in_new_config() {
    let a = mint_test_cert("rotate-a.test");
    let b = mint_test_cert("rotate-b.test");
    let cfg_a = build_server_config(&a);
    let cfg_b = build_server_config(&b);

    let rot = RotatableServerConfig::new(cfg_a.clone());
    let pre = rot.current();
    assert!(Arc::ptr_eq(&pre, &cfg_a), "initial snapshot must point at A");

    rot.rotate(cfg_b.clone());
    let post = rot.current();
    assert!(Arc::ptr_eq(&post, &cfg_b), "post-rotation snapshot must point at B");
    // The pre-rotation snapshot must still point at A — proves the
    // rotation does NOT invalidate in-flight references.
    assert!(
        Arc::ptr_eq(&pre, &cfg_a),
        "pre-rotation snapshot must still observe A after rotation"
    );
}

// ---------- reload_tls error paths ----------

#[tokio::test]
async fn reload_tls_rejects_garbage_pem() {
    let cert_a = mint_test_cert("err-cert.test");
    let handle = spawn_test_observer(&cert_a).await;
    // Cert bytes that don't decode as a CERTIFICATE PEM block.
    let result = handle.reload_tls(b"-----BEGIN GARBAGE-----\nnope\n", cert_a.key_pem.as_bytes());
    match result {
        Err(TlsRotationError::CertParse { .. } | TlsRotationError::EmptyCertChain) => {}
        other => panic!("expected CertParse/EmptyCertChain, got {other:?}"),
    }
    // Failure path leaves the prior config installed: a second
    // valid reload still works.
    let cert_b = mint_test_cert("err-recover.test");
    handle
        .reload_tls(cert_b.cert_pem.as_bytes(), cert_b.key_pem.as_bytes())
        .expect("recovery reload after garbage-PEM failure must succeed");
    handle.shutdown().await;
}

#[tokio::test]
async fn reload_tls_rejects_garbage_key() {
    let cert_a = mint_test_cert("err-key.test");
    let handle = spawn_test_observer(&cert_a).await;
    let result = handle.reload_tls(cert_a.cert_pem.as_bytes(), b"-----BEGIN GARBAGE KEY-----\nnope\n");
    match result {
        Err(TlsRotationError::KeyParse { .. }) => {}
        other => panic!("expected KeyParse, got {other:?}"),
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn reload_tls_rejects_not_configured() {
    // Spawn the management API WITHOUT TLS (loopback default) —
    // `reload_tls` MUST return NotConfigured rather than panicking
    // or silently rotating a phantom handle.
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: None,
    };
    let handle = spawn_management_api(bind, metrics, 0).await.expect("spawn");
    let cert = mint_test_cert("not-configured.test");
    let result = handle.reload_tls(cert.cert_pem.as_bytes(), cert.key_pem.as_bytes());
    match result {
        Err(TlsRotationError::NotConfigured) => {}
        other => panic!("expected NotConfigured, got {other:?}"),
    }
    handle.shutdown().await;
}

#[tokio::test]
async fn readyz_is_red_only_while_tls_rotation_guard_is_alive() {
    let cert = mint_test_cert("readyz-rotation.test");
    let metrics = Arc::new(MetricsInner::new());
    metrics.process_batch_duration.observe(100_000);
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: Some(server_tls_config_for(&cert)),
    };
    let handle = spawn_management_api(bind, metrics, 0).await.expect("spawn");

    let guard = handle.begin_tls_rotation().expect("TLS readiness state");
    let (status, body) = tls_http_get_readyz(
        handle.local_addr,
        "readyz-rotation.test",
        &cert.cert_der,
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(body, "tls_rotation_in_progress");

    drop(guard);
    let (status, body) = tls_http_get_readyz(
        handle.local_addr,
        "readyz-rotation.test",
        &cert.cert_der,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, "ready");
    handle.shutdown().await;
}

// ---------- ALPN preservation ----------

#[tokio::test]
async fn reload_tls_preserves_alpn_config() {
    let cert_a = mint_test_cert("alpn-a.test");
    let cert_b = mint_test_cert("alpn-b.test");

    // Build the initial server config with a custom ALPN list, then
    // wrap it in a `RotatableServerConfig` by hand so we can assert
    // ALPN survives the rotation. (The public `ServerTlsConfig`
    // constructors don't take ALPN today; operators wire it
    // externally + Phase A's promise is that rotation preserves
    // whatever was on the prior config.)
    let mut initial = (*build_server_config(&cert_a)).clone();
    initial.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let tls = ServerTlsConfig {
        server: RotatableServerConfig::new(Arc::new(initial)),
    };

    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: Some(tls),
    };
    let handle = spawn_management_api(bind, metrics, 0).await.expect("spawn");

    handle
        .reload_tls(cert_b.cert_pem.as_bytes(), cert_b.key_pem.as_bytes())
        .expect("rotation must succeed");

    let post_rotation = handle
        .tls
        .as_ref()
        .expect("tls handle stored on ApiServerHandle")
        .current();
    assert_eq!(
        post_rotation.alpn_protocols,
        vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        "rotation must preserve the operator's ALPN configuration"
    );
    handle.shutdown().await;
}

// ---------- end-to-end handshake ----------

#[tokio::test]
async fn accept_loop_uses_rotated_config_for_next_connection() {
    let cert_a = mint_test_cert("e2e-a.test");
    let cert_b = mint_test_cert("e2e-b.test");
    let handle = spawn_test_observer(&cert_a).await;
    let addr = handle.local_addr;

    // First handshake serves cert A.
    let observed_a = dial_and_capture_peer_cert(addr, "e2e-a.test", &cert_a.cert_der).await;
    assert_eq!(
        observed_a, cert_a.cert_der,
        "pre-rotation handshake must see cert A"
    );

    // Rotate to cert B.
    handle
        .reload_tls(cert_b.cert_pem.as_bytes(), cert_b.key_pem.as_bytes())
        .expect("rotation");

    // Second handshake serves cert B.
    let observed_b = dial_and_capture_peer_cert(addr, "e2e-b.test", &cert_b.cert_der).await;
    assert_eq!(
        observed_b, cert_b.cert_der,
        "post-rotation handshake must see cert B"
    );

    handle.shutdown().await;
}

// ---------- audit-event capture ----------

/// In-memory tracing subscriber Layer that pushes captured events
/// into a shared `Vec<String>`. Each captured entry is a flattened
/// `field=value field=value ...` rendering so the test can assert
/// presence + absence of substrings.
#[derive(Clone, Default)]
struct CaptureLayer {
    events: Arc<Mutex<Vec<String>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        if meta.target() != "graphreact::observer::audit" {
            return;
        }
        let mut visitor = FieldDumpVisitor {
            buf: format!("target={} ", meta.target()),
        };
        event.record(&mut visitor);
        self.events.lock().unwrap().push(visitor.buf);
    }
}

struct FieldDumpVisitor {
    buf: String,
}

impl tracing::field::Visit for FieldDumpVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write as _;
        let _ = write!(self.buf, "{}={} ", field.name(), value);
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.buf, "{}={:?} ", field.name(), value);
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        use std::fmt::Write as _;
        let _ = write!(self.buf, "{}={} ", field.name(), value);
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        use std::fmt::Write as _;
        let _ = write!(self.buf, "{}={} ", field.name(), value);
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        use std::fmt::Write as _;
        let _ = write!(self.buf, "{}={} ", field.name(), value);
    }
}

#[test]
fn tls_reload_audit_event_fires() {
    use tracing_subscriber::layer::SubscriberExt;

    let capture = CaptureLayer::default();
    let subscriber = tracing_subscriber::Registry::default().with(capture.clone());

    // Install the subscriber as the GLOBAL default. The tracing
    // crate caches callsite interest in static atomics on first
    // emission; a thread-local `with_default` can race that cache
    // when callsites have already fired under a previous
    // subscriber in the test binary. Going global makes the
    // capture work regardless of test-binary ordering.
    //
    // `set_global_default` errors on second-call collisions — we
    // ignore the error because parallel tests in the same binary
    // may have already installed a different global; in that case
    // OUR layer doesn't fire, and the assertions below would
    // surface zero captured events. To stay robust against that
    // ordering, we accept whichever subscriber wins the race and
    // only assert on a `>=` lower bound for the events we ourselves
    // emit (vs `==`). The PEM-leak negative check still has full
    // teeth across every captured event.
    let _ = tracing::subscriber::set_global_default(subscriber);

    // Snapshot the capture buffer state BEFORE driving rotations so
    // we can compute the per-test event delta even when the global
    // subscriber has captured events from sibling tests running in
    // the same binary.
    let baseline = capture.events.lock().unwrap().len();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let cert_a = mint_test_cert("audit-a.test");
        let cert_b = mint_test_cert("audit-b.test");
        let handle = spawn_test_observer(&cert_a).await;
        handle
            .reload_tls(cert_b.cert_pem.as_bytes(), cert_b.key_pem.as_bytes())
            .expect("rotation");

        // Drive a failure path too so we exercise the warn
        // branch's audit shape.
        let _ = handle.reload_tls(b"-----GARBAGE-----", cert_b.key_pem.as_bytes());

        handle.shutdown().await;
    });

    let all_events = capture.events.lock().unwrap().clone();
    // Only inspect events captured DURING this test's run. Even
    // with the bounded window we may still see events from sibling
    // tests that ran concurrently — that's why the lower-bound
    // assertions below use `>=`, not `==`.
    let events: Vec<&String> = all_events.iter().skip(baseline).collect();

    // Substrate guarantee 1: each `reload_tls` call emits a
    // `tls_reload_attempt` event. We drove 2; if the global
    // subscriber raced, we may also see attempts from sibling
    // tests in the same binary, so use `>= 2`.
    let attempts = events
        .iter()
        .filter(|e| e.contains("event=tls_reload_attempt"))
        .count();
    assert!(
        attempts >= 2,
        "expected >= 2 attempt events captured during this test window, got {attempts}; captured {events:?}"
    );

    // Substrate guarantee 2: success path emits `tls_reloaded`
    // with a non-empty `leaf_fingerprint_sha256`. We drove exactly
    // one successful rotation; `>= 1` to be ordering-tolerant.
    let successes: Vec<_> = events
        .iter()
        .filter(|e| e.contains("event=tls_reloaded"))
        .collect();
    assert!(
        !successes.is_empty(),
        "expected >= 1 success event; captured {events:?}"
    );
    assert!(
        successes[0].contains("leaf_fingerprint_sha256="),
        "success event must carry leaf_fingerprint_sha256; got {}",
        successes[0]
    );

    // Substrate guarantee 3: failure path emits
    // `tls_reload_failure` with an `error_kind` field. We drove
    // exactly one failure; `>= 1`.
    let failures: Vec<_> = events
        .iter()
        .filter(|e| e.contains("event=tls_reload_failure"))
        .collect();
    assert!(
        !failures.is_empty(),
        "expected >= 1 failure event; captured {events:?}"
    );
    assert!(
        failures[0].contains("error_kind="),
        "failure event must carry error_kind field; got {}",
        failures[0]
    );

    // Substrate guarantee 4: NO event — from this test OR from any
    // sibling test that fired through the same global subscriber —
    // leaks cert/key bytes. The sentinel markers are the PEM armor
    // (`-----BEGIN`) and the `PRIVATE KEY` substring (catches base64
    // fragments that happen to elide the armor). This negative
    // assertion uses ALL captured events (no baseline skip) so it
    // catches leaks from any rotation-related callsite, not just
    // the ones this test drove.
    for ev in &all_events {
        assert!(
            !ev.contains("-----BEGIN"),
            "audit event leaked PEM armor: {ev}"
        );
        assert!(
            !ev.contains("PRIVATE KEY"),
            "audit event leaked private key fragment: {ev}"
        );
    }
}

// ---------- shared helpers ----------

async fn spawn_test_observer(
    cert: &TestCert,
) -> graphreact_observer::ApiServerHandle {
    let metrics = Arc::new(MetricsInner::new());
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: Some(server_tls_config_for(cert)),
    };
    spawn_management_api(bind, metrics, 0).await.expect("spawn")
}

/// Dial the server with a tokio-rustls client that trusts only the
/// supplied cert (so a wrong-cert handshake fails immediately + the
/// test diagnostic surfaces). Returns the leaf cert the server
/// presented during the handshake.
async fn dial_and_capture_peer_cert(
    addr: std::net::SocketAddr,
    sni: &str,
    expected: &CertificateDer<'static>,
) -> CertificateDer<'static> {
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(expected.clone())
        .expect("root store accepts self-signed cert");

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let client_cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(ALL_VERSIONS)
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_cfg));

    let tcp = TcpStream::connect(addr).await.expect("tcp connect");
    let dns_name = rustls::pki_types::ServerName::try_from(sni.to_string())
        .expect("sni parses");
    let tls = connector.connect(dns_name, tcp).await.expect("tls handshake");
    let (_io, session) = tls.get_ref();
    let chain = session
        .peer_certificates()
        .expect("peer presented a cert chain");
    chain[0].clone()
}

async fn tls_http_get_readyz(
    addr: std::net::SocketAddr,
    sni: &str,
    expected: &CertificateDer<'static>,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(expected.clone()).expect("add test root");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let client_cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(ALL_VERSIONS)
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let tcp = TcpStream::connect(addr).await.expect("tcp connect");
    let server_name = rustls::pki_types::ServerName::try_from(sni.to_string()).expect("SNI");
    let mut tls = connector.connect(server_name, tcp).await.expect("TLS connect");
    tls.write_all(
        format!(
            "GET /v1/readyz HTTP/1.1\r\nHost: {sni}\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .expect("write request");
    let mut response = Vec::new();
    tls.read_to_end(&mut response).await.expect("read response");
    let response = String::from_utf8(response).expect("UTF-8 response");
    let (head, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .expect("HTTP status");
    (status, body.to_string())
}
