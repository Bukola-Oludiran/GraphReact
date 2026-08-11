//! **M-Prod sub-track #4 drain verification gate.** Integration
//! tests for [`DaemonHandle::start_drain`] + the observer-side drain
//! contract.
//!
//! Mirrors `cluster_cli.rs`: spin up a real daemon via
//! `start_daemon` (engine-free / Null runtime, no peers, no `.grx`
//! mount), wire an observer bind so `DrainFlag` is meaningful, then
//! call `start_drain` from the test directly. Asserts on
//! `DrainOutcome` fields + the `/v1/readyz` 503 `draining` body.

use std::net::SocketAddr;
use std::time::Duration;

use graphreact_cli::commands::{start_daemon, DaemonOptions};
use graphreact_dist::{DistError, NetSplitPolicy, NetTickConfig, NodeId, SecretCookie, TlsConfig};
use graphreact_observer::{BindConfig, BindFlags};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// Inline TLS pair generator — same shape as the cluster_cli test
/// helper. These tests don't exercise mTLS rotation; we just need a
/// valid cluster cert chain so `start_daemon`'s `Cluster::start`
/// boots cleanly.
fn pair_tls(name_a: &str, name_b: &str) -> (TlsConfig, TlsConfig) {
    let ca_kp = rcgen::KeyPair::generate().expect("ca keygen");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["grx-cli-test CA".into()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-sign");
    let ca_der: CertificateDer<'static> = ca_cert.der().clone();

    let make_leaf = |name: &str| -> TlsConfig {
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).expect("leaf params");
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            name.to_string().try_into().expect("san name"),
        ));
        let kp = rcgen::KeyPair::generate().expect("leaf keygen");
        let cert = params.signed_by(&kp, &ca_cert, &ca_kp).expect("leaf sign");
        let cert_der: CertificateDer<'static> = cert.der().clone();
        let key_der: PrivateKeyDer<'static> = PrivatePkcs8KeyDer::from(kp.serialize_der()).into();
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

/// Construct a daemon with an observer bound on a loopback
/// ephemeral port (so the drain bit is meaningful). No peers, no
/// `.grx` mount, Null engine. Single-node clusters pass the
/// cluster-readiness predicate trivially.
async fn boot_daemon(with_observer: bool) -> graphreact_cli::commands::DaemonHandle {
    let (tls, _bob_tls) = pair_tls("alice", "bob");
    let cookie_bytes = b"shared-cookie".to_vec();
    let observer_bind = with_observer.then(|| {
        BindConfig::from_flags(BindFlags {
            addr: Some(ephemeral_loopback()),
            token_env: None,
            token_file: None,
            cert: None,
            key: None,
        })
        .expect("observer bind config")
    });

    let opts = DaemonOptions {
        path: None,
        node_id: parse_id("drain-test@127.0.0.1"),
        listen: ephemeral_loopback(),
        cookie: SecretCookie::from_bytes(cookie_bytes).unwrap(),
        tls,
        peers: Vec::new(),
        runtime: graphreact_cli::cli::RuntimeArg::Null,
        go_build: Default::default(),
        net_tick: fast_tick(),
        net_split_policy: NetSplitPolicy::PauseOnSplit,
        duration_secs: None,
        observer_bind,
        observer_tls_paths: None,
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
    start_daemon(opts).await.expect("daemon start")
}

/// **`start_drain_waits_for_inflight_then_shutdown_completes`** —
/// healthy fast-path drain: no in-flight ingress at drain-start,
/// the poll loop hits quiescence on the first 25ms iteration, and
/// the outcome reports `in_flight_settled = true` /
/// `deadline_exceeded = false`. Duration is bounded below by the
/// initial `poll.tick().await` skip (~25ms) and above by 100ms
/// for CI flake-tolerance.
#[tokio::test]
async fn start_drain_completes_immediately_when_inflight_empty() {
    let handle = boot_daemon(true).await;
    let outcome = handle.start_drain(Duration::from_secs(5)).await;
    assert!(
        outcome.in_flight_settled,
        "no in-flight requests → drain should settle (got {outcome:?})"
    );
    assert!(
        !outcome.deadline_exceeded,
        "deadline was 5s; nothing pending"
    );
    assert!(
        outcome.cluster_quiesced,
        "empty cluster handshake population should quiesce"
    );
    assert!(
        outcome.drain_duration < Duration::from_millis(200),
        "fast-path drain should be near-instant, got {:?}",
        outcome.drain_duration
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn start_drain_without_observer_still_closes_cluster_admission() {
    let handle = boot_daemon(false).await;
    let outcome = handle.start_drain(Duration::from_secs(1)).await;
    assert!(outcome.in_flight_settled);
    assert!(outcome.cluster_quiesced);
    assert!(!outcome.deadline_exceeded);

    let (client, _server) = tokio::io::duplex(1_024);
    let bob_id = parse_id("bob@127.0.0.1");
    assert!(matches!(
        handle.cluster().connect_with_stream(client, bob_id).await,
        Err(DistError::Draining)
    ));
    handle.shutdown().await;
}

/// **`start_drain_respects_deadline`** — synthetic always-pending
/// in-flight: bump the ingress counter without ever dropping the
/// guard so the quiescence loop is forced into deadline expiry.
/// Outcome carries `deadline_exceeded = true` +
/// `in_flight_settled = false` + a `drain_duration` ≈ the
/// configured 100ms deadline.
#[tokio::test]
async fn start_drain_respects_deadline_on_stuck_inflight() {
    let handle = boot_daemon(true).await;
    // Hold an in-flight guard for the entire drain window — the
    // counter never drops to zero so the orchestrator times out.
    // We grab the counter through the public surface: it's a
    // `Clone` of the same `Arc<AtomicUsize>` the observer
    // increments.
    let counter = handle
        .observer_ingress_counter()
        .expect("observer was wired so the counter is present");
    let _stuck_guard = counter.enter();

    let deadline = Duration::from_millis(100);
    let started = std::time::Instant::now();
    let outcome = handle.start_drain(deadline).await;
    let elapsed = started.elapsed();

    assert!(
        outcome.deadline_exceeded,
        "stuck in-flight should hit the deadline, got {outcome:?}"
    );
    assert!(
        !outcome.in_flight_settled,
        "stuck request leaves in_flight_settled=false"
    );
    assert!(
        outcome.drain_duration >= Duration::from_millis(90),
        "drain_duration ≈ deadline (100ms), got {:?}",
        outcome.drain_duration
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "test should not vastly exceed the deadline, got {elapsed:?}"
    );
    handle.shutdown().await;
}

/// **`start_drain_is_idempotent_across_double_call`** — second
/// invocation observes the CAS gate failure and returns the no-op
/// outcome immediately. Models duplicate-SIGTERM handling: the
/// kubelet may signal twice; the second hit doesn't re-emit
/// `drain_started` or reset the deadline.
#[tokio::test]
async fn start_drain_is_idempotent_across_double_call() {
    let handle = boot_daemon(true).await;
    let first = handle.start_drain(Duration::from_secs(2)).await;
    assert!(first.in_flight_settled);
    let second = handle.start_drain(Duration::from_secs(2)).await;
    assert!(
        second.in_flight_settled && !second.deadline_exceeded,
        "second drain returns no-op outcome immediately, got {second:?}"
    );
    assert!(
        second.drain_duration < Duration::from_millis(50),
        "no-op drain should be near-zero (CAS short-circuit), got {:?}",
        second.drain_duration
    );
    handle.shutdown().await;
}
