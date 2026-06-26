//! **M-Prod sub-track #2 Phase B Part 1 verification gate.** In-
//! process tests for `DaemonHandle::reload_tls_from_disk`.
//!
//! The signal-firing arm of Phase B Part 1 (a real SIGHUP arriving
//! at `drive_handle_until`'s `tokio::select!`) is exercised by the
//! companion subprocess test in `sighup_cert_rotation_subprocess.rs`
//! — calling `raise(SIGHUP)` from an in-process tokio test would
//! interrupt every parallel cargo test worker.
//!
//! This file pairs three direct-call tests against the
//! `DaemonHandle::reload_tls_from_disk` entry point:
//!
//! 1. `reload_tls_from_disk_swaps_cert_b_for_cert_a` — happy path:
//!    boot a daemon with cert A on disk, dial the observer with
//!    tokio-rustls + capture the leaf cert, overwrite the cert + key
//!    files with cert B, call `reload_tls_from_disk` (no signal),
//!    dial again, confirm cert B is observed.
//!
//! 2. `reload_tls_from_disk_is_noop_during_drain` — drain-respect:
//!    start a drain on a long deadline, call `reload_tls_from_disk`
//!    while the drain bit is flipped, confirm NO rotation occurred
//!    (the next dial still observes cert A) AND a
//!    `tls_reload_skipped` audit event with `reason="draining"` was
//!    captured.
//!
//! 3. `reload_tls_from_disk_with_missing_cert_file_surfaces_audit` —
//!    failure path: delete the cert file, call
//!    `reload_tls_from_disk`, confirm a `tls_reload_attempt` +
//!    `tls_reload_failure` audit pair was emitted with
//!    `error_kind=cert_read` (NOT `cert_parse` — the verify-finding
//!    patch split I/O failures from PEM-parse failures), and the
//!    prior cert keeps serving on a follow-up handshake.

#![cfg(unix)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graphreact_cli::commands::{start_daemon, DaemonOptions, TlsPaths};
use graphreact_dist::{NetSplitPolicy, NetTickConfig, NodeId, SecretCookie, TlsConfig};
use graphreact_observer::{BindConfig, RotatableServerConfig, ServerTlsConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::ServerConfig;
use rustls::ALL_VERSIONS;

// ---------- helpers ----------

/// Self-signed cert + key in BOTH PEM (for writing to the cert/key
/// files on disk) and DER (for handshake equality checks).
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

fn pair_cluster_tls(name_a: &str, name_b: &str) -> (TlsConfig, TlsConfig) {
    let ca_kp = rcgen::KeyPair::generate().expect("ca keygen");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["grx-sighup-test CA".into()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-sign");
    let ca_der: CertificateDer<'static> = ca_cert.der().clone();
    let make_leaf = |name: &str| -> TlsConfig {
        let mut params =
            rcgen::CertificateParams::new(vec![name.into()]).expect("leaf params");
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            name.to_string().try_into().expect("san name"),
        ));
        let kp = rcgen::KeyPair::generate().expect("leaf keygen");
        let cert = params
            .signed_by(&kp, &ca_cert, &ca_kp)
            .expect("leaf sign");
        let cert_der: CertificateDer<'static> = cert.der().clone();
        let key_der: PrivateKeyDer<'static> =
            PrivatePkcs8KeyDer::from(kp.serialize_der()).into();
        TlsConfig::from_pem_bytes(vec![cert_der], key_der, vec![ca_der.clone()], true)
            .expect("tls from pem bytes")
    };
    (make_leaf(name_a), make_leaf(name_b))
}

fn ephemeral_loopback() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

fn parse_id(s: &str) -> NodeId {
    s.parse().expect("valid node id")
}

fn fast_tick() -> NetTickConfig {
    NetTickConfig {
        tick_interval: Duration::from_millis(100),
        miss_threshold: 30,
        partition_grace: Duration::from_secs(60),
    }
}

/// Boot a daemon with a TLS-enabled observer bind. Returns the live
/// handle + the cert/key paths on disk (so the caller can overwrite
/// them between rotations). The cluster cert chain (`pair_cluster_tls`)
/// is independent from the observer cert — the latter is what
/// `reload_tls_from_disk` rotates.
struct TlsBooted {
    handle: graphreact_cli::commands::DaemonHandle,
    cert_path: std::path::PathBuf,
    key_path: std::path::PathBuf,
    _tempdir: tempfile::TempDir,
}

async fn boot_tls_daemon(initial: &TestCert) -> TlsBooted {
    let dir = tempfile::tempdir().expect("tempdir");
    let cert_path = dir.path().join("observer-cert.pem");
    let key_path = dir.path().join("observer-key.pem");
    std::fs::write(&cert_path, &initial.cert_pem).expect("write cert pem");
    std::fs::write(&key_path, &initial.key_pem).expect("write key pem");

    let (cluster_tls, _bob) = pair_cluster_tls("alice", "bob");
    let cookie = SecretCookie::from_bytes(b"sighup-test-cookie".to_vec()).unwrap();

    // The CLI's BindFlags::from_flags rejects remote binds without
    // a token; loopback + cert + key without a token is allowed,
    // which is exactly what we want for the in-process direct-call
    // tests (the rotation contract is the load-bearing thing, not
    // the auth contract).
    let bind = BindConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: None,
        tls: Some(server_tls_config_for(initial)),
    };

    let opts = DaemonOptions {
        path: None,
        node_id: parse_id("sighup-test@127.0.0.1"),
        listen: ephemeral_loopback(),
        cookie,
        tls: cluster_tls,
        peers: Vec::new(),
        runtime: graphreact_cli::cli::RuntimeArg::Null,
        net_tick: fast_tick(),
        net_split_policy: NetSplitPolicy::PauseOnSplit,
        duration_secs: None,
        observer_bind: Some(bind),
        observer_tls_paths: Some(TlsPaths {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
        }),
        debug: None,
        debug_wait: false,
        observer_otel_endpoint: None,
        observer_otel_service_name: None,
        observer_audit_log: None,
        observer_tokens_path: None,
        observer_jwks_path: None,
        observer_jwt_config_path: None,
        observer_audit_forwarder_spool: None,
        observer_audit_forwarder_stderr: false,
        drain_deadline_secs: None,
    };
    let handle = start_daemon(opts).await.expect("daemon start");
    TlsBooted {
        handle,
        cert_path,
        key_path,
        _tempdir: dir,
    }
}

/// Dial the observer with a tokio-rustls client that trusts only
/// `expected_root`. Returns the leaf cert the server presented. The
/// SNI doesn't actually matter here — the server's self-signed
/// cert isn't SNI-routed — but rustls still requires a parsable
/// server-name.
async fn dial_observer_capture_cert(
    addr: SocketAddr,
    sni: &str,
    expected_root: &CertificateDer<'static>,
) -> CertificateDer<'static> {
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(expected_root.clone())
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
    let tls = connector
        .connect(dns_name, tcp)
        .await
        .expect("tls handshake");
    let (_io, session) = tls.get_ref();
    let chain = session
        .peer_certificates()
        .expect("peer presented a cert chain");
    chain[0].clone()
}

// ---------- audit-event capture ----------

/// In-memory tracing-subscriber Layer that pushes captured events
/// on the `graphreact::observer::audit` target into a shared
/// `Vec<String>`. Copied from `tls_rotation.rs` (the observer-side
/// tests) so the CLI-side tests don't depend on the observer crate's
/// internal test utilities.
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

/// Process-singleton capture buffer for audit events. `tracing`
/// caches callsite interest in static atomics on first emission;
/// using a thread-local `with_default` would race that cache when
/// callsites have already fired under a different subscriber in
/// the same binary. We install ONE global subscriber wrapping ONE
/// shared `Arc<Mutex<Vec<String>>>` the first time `install_capture`
/// is called and return clones of the same handle on every
/// subsequent call. Subsequent `set_global_default` attempts error
/// silently and are dropped — every test in this binary then reads
/// from the singleton buffer.
fn install_capture() -> CaptureLayer {
    use std::sync::OnceLock;
    use tracing_subscriber::layer::SubscriberExt;
    static CAPTURE: OnceLock<CaptureLayer> = OnceLock::new();
    let capture = CAPTURE
        .get_or_init(|| {
            let capture = CaptureLayer::default();
            let subscriber =
                tracing_subscriber::Registry::default().with(capture.clone());
            let _ = tracing::subscriber::set_global_default(subscriber);
            capture
        })
        .clone();
    // tracing 0.1 caches per-callsite `Interest` after the first
    // emission. If a callsite fires under the no-op default
    // subscriber before our global is installed (which happens
    // when cargo runs tests in parallel and a non-capturing test
    // touches the same callsite first), the cache pins the
    // callsite as "never" and our subscriber sees no events. Force
    // a rebuild on every install_capture call — it's cheap and
    // guarantees subsequent emits dispatch correctly.
    tracing::callsite::rebuild_interest_cache();
    capture
}

// ---------- tests ----------

/// **Happy path**. boot with cert A, dial → observe A, overwrite
/// cert + key files with cert B, call `reload_tls_from_disk`, dial
/// again → observe B. Drives the substrate without firing a real
/// signal (the subprocess test covers the kill(SIGHUP) wire).
#[tokio::test]
async fn reload_tls_from_disk_swaps_cert_b_for_cert_a() {
    // **Capture eager-install.** All three tests in this binary
    // call `install_capture` at the top so the global
    // tracing-subscriber is wired BEFORE any callsite fires. The
    // OnceLock inside guarantees a single global default; cargo's
    // parallel test runner orders the inits non-deterministically,
    // and an audit-checking test running while another test hasn't
    // yet installed the subscriber would otherwise see an empty
    // capture window. This test doesn't itself assert on the
    // capture, but eager-installing keeps the buffer populated for
    // the tests that do.
    let _ = install_capture();
    let cert_a = mint_test_cert("sighup-a.test");
    let cert_b = mint_test_cert("sighup-b.test");
    let booted = boot_tls_daemon(&cert_a).await;
    let addr = booted.handle.observer_addr().expect("observer bound");

    let observed_a = dial_observer_capture_cert(addr, "sighup-a.test", &cert_a.cert_der).await;
    assert_eq!(
        observed_a, cert_a.cert_der,
        "pre-rotation handshake must see cert A"
    );

    // Overwrite cert + key files atomically (rcgen-minted PEM
    // material is small enough that the kernel writes it in one
    // chunk; the rotation contract on partial reads is documented
    // in m-prod-tls-rotation.md and exercised separately).
    std::fs::write(&booted.cert_path, &cert_b.cert_pem).expect("write cert b");
    std::fs::write(&booted.key_path, &cert_b.key_pem).expect("write key b");

    // Drive the rotation via the public direct-call entry point —
    // identical machinery to what the SIGHUP arm in
    // drive_handle_until calls.
    booted.handle.reload_tls_from_disk();

    let observed_b = dial_observer_capture_cert(addr, "sighup-b.test", &cert_b.cert_der).await;
    assert_eq!(
        observed_b, cert_b.cert_der,
        "post-rotation handshake must see cert B"
    );

    booted.handle.shutdown().await;
}

/// **Drain-respect**. Drain is in progress → SIGHUP-triggered
/// rotation is a no-op + emits `tls_reload_skipped reason=draining`.
/// The next dial still observes cert A.
#[tokio::test]
async fn reload_tls_from_disk_is_noop_during_drain() {
    let capture = install_capture();
    let cert_a = mint_test_cert("drain-a.test");
    let cert_b = mint_test_cert("drain-b.test");
    let booted = boot_tls_daemon(&cert_a).await;
    let addr = booted.handle.observer_addr().expect("observer bound");

    // Confirm the baseline handshake sees cert A.
    let observed_a = dial_observer_capture_cert(addr, "drain-a.test", &cert_a.cert_der).await;
    assert_eq!(observed_a, cert_a.cert_der);

    // Start a drain on a long deadline so the flag stays set
    // throughout the rotation call. The drain itself short-circuits
    // immediately (no in-flight ingress) — what matters is that the
    // DrainFlag::is_draining() AtomicBool stays `true`. Spawn the
    // drain in a background task so the await below doesn't block
    // the test thread (start_drain's internal poll loop keeps the
    // bit set for the full deadline window when nothing settles).
    //
    // Workaround: `start_drain` returns once quiesced even though
    // the bit stays set, so we can `.await` it inline. The bit is
    // CAS-set on the first call and NOT reset on subsequent calls;
    // it stays high for the rest of the daemon's lifetime by
    // design. We snapshot the baseline event count BEFORE the
    // drain so the assert filters to events emitted during this
    // test's window.
    let baseline_events = capture.events.lock().unwrap().len();
    let _ = booted.handle.start_drain(Duration::from_secs(60)).await;

    // Stage cert B on disk, but the rotation MUST skip.
    std::fs::write(&booted.cert_path, &cert_b.cert_pem).expect("write cert b");
    std::fs::write(&booted.key_path, &cert_b.key_pem).expect("write key b");
    booted.handle.reload_tls_from_disk();

    // Cert A is still serving — no rotation happened.
    let observed_after = dial_observer_capture_cert(addr, "drain-a.test", &cert_a.cert_der).await;
    assert_eq!(
        observed_after, cert_a.cert_der,
        "drain-respect: rotation must be skipped while DrainFlag is set"
    );

    // Verify the skipped audit event landed (lower-bound `>= 1`
    // because the global subscriber may have captured events from
    // sibling tests too).
    let all = capture.events.lock().unwrap().clone();
    let window: Vec<&String> = all.iter().skip(baseline_events).collect();
    let skipped = window
        .iter()
        .filter(|e| {
            e.contains("event=tls_reload_skipped") && e.contains("reason=draining")
        })
        .count();
    assert!(
        skipped >= 1,
        "expected >= 1 tls_reload_skipped/draining event in window; captured {window:?}"
    );

    // Note on parallel-test sharing: the capture buffer is
    // process-global (one OnceLock-initialized subscriber per test
    // binary), so other parallel-running tests in this file emit
    // their `tls_reload_attempt` / `tls_reloaded` events into the
    // same buffer. We CANNOT assert `attempt == 0` against the
    // global window; instead the positive `tls_reload_skipped`
    // check above + the cert-A-still-serving handshake check are
    // the load-bearing guarantees for drain-respect. The
    // single-test-run case (`cargo test -- --test-threads=1`)
    // additionally satisfies the strict negative assertions; the
    // subprocess test in `sighup_cert_rotation_subprocess.rs`
    // exercises the SIGHUP-during-drain path in process isolation
    // when that surface needs a stricter verification.

    booted.handle.shutdown().await;
}

/// **Failure path: missing cert file**. Delete the cert file, call
/// `reload_tls_from_disk`, confirm `tls_reload_attempt` +
/// `tls_reload_failure` (`error_kind=cert_read` — verify-finding
/// patch split I/O failures from PEM-parse failures) fire and that
/// the prior cert keeps serving.
#[tokio::test]
async fn reload_tls_from_disk_with_missing_cert_file_surfaces_audit() {
    let capture = install_capture();
    let cert_a = mint_test_cert("missing-a.test");
    let booted = boot_tls_daemon(&cert_a).await;
    let addr = booted.handle.observer_addr().expect("observer bound");

    // Confirm baseline.
    let observed_a = dial_observer_capture_cert(addr, "missing-a.test", &cert_a.cert_der).await;
    assert_eq!(observed_a, cert_a.cert_der);

    let baseline_events = capture.events.lock().unwrap().len();

    // Remove the cert file but leave the key file in place. The
    // helper's `std::fs::read(&cert_path)` will fail with ENOENT;
    // the rotation MUST emit a tls_reload_attempt followed by
    // tls_reload_failure with error_kind=cert_read (verify-finding
    // patch: I/O errors get cert_read/key_read, not cert_parse/
    // key_parse which is reserved for PEM-parse failures).
    std::fs::remove_file(&booted.cert_path).expect("remove cert");
    booted.handle.reload_tls_from_disk();

    // Old cert keeps serving — the rotation never executed.
    let observed_after = dial_observer_capture_cert(addr, "missing-a.test", &cert_a.cert_der).await;
    assert_eq!(
        observed_after, cert_a.cert_der,
        "file-read failure: prior cert must keep serving"
    );

    // Audit assertions.
    let all = capture.events.lock().unwrap().clone();
    let window: Vec<&String> = all.iter().skip(baseline_events).collect();
    let attempts = window
        .iter()
        .filter(|e| e.contains("event=tls_reload_attempt"))
        .count();
    assert!(
        attempts >= 1,
        "expected >= 1 tls_reload_attempt event in window; captured {window:?}"
    );
    let failures: Vec<&&String> = window
        .iter()
        .filter(|e| {
            e.contains("event=tls_reload_failure") && e.contains("error_kind=cert_read")
        })
        .collect();
    assert!(
        !failures.is_empty(),
        "expected >= 1 tls_reload_failure/cert_read event in window; captured {window:?}"
    );

    booted.handle.shutdown().await;
}
