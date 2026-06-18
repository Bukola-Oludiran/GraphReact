//! M14.8 CLI acceptance gate.
//!
//! Calls `start_daemon` / `run_connect_blocking` directly so the
//! tests don't depend on the binary being built. Generates TLS
//! material via `rcgen` (a dev-dep) — same shape as the dist
//! crate's `dev_tls` helper, inlined here so the dist module can
//! stay test-only.
//!
//! Coverage:
//!   1. `start_daemon` on port A accepts `start_daemon` from port B
//!      with matching cookie + cert chain → both sides register the
//!      peer in their `NodeRegistry`.
//!   2. Cookie-mismatch (different shared secret on each side)
//!      surfaces a printable `CommandError`.
//!   3. World-readable cookie file is refused with a
//!      `CommandError::Cluster(DistError::Cookie(_))` carrying a
//!      `mode` hint string.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use graphreact_cli::commands::{
    parse_peer_entry, resolve_cookie, start_daemon, CommandError, DaemonOptions,
};
use graphreact_dist::{NetSplitPolicy, NetTickConfig, NodeId, SecretCookie, TlsConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// Inline analogue of `graphreact_dist::dev_tls::pair`. Issues a
/// fresh CA + two leaf certs sharing it, returning the
/// configurations callers can stuff into `DaemonOptions::tls`.
fn pair_tls(name_a: &str, name_b: &str) -> (TlsConfig, TlsConfig) {
    let ca_kp = rcgen::KeyPair::generate().expect("ca keygen");
    let mut ca_params = rcgen::CertificateParams::new(vec!["grx-cli-test CA".into()])
        .expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-sign");
    let ca_der: CertificateDer<'static> = ca_cert.der().clone();

    let make_leaf = |name: &str| -> TlsConfig {
        let mut params = rcgen::CertificateParams::new(vec![name.into()])
            .expect("leaf params");
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

#[tokio::test]
async fn daemon_accepts_peer_with_matching_cookie() {
    let (alice_tls, bob_tls) = pair_tls("alice", "bob");
    let cookie_bytes = b"shared-cookie".to_vec();

    let alice_opts = DaemonOptions {
        path: None,
        node_id: parse_id("alice@127.0.0.1"),
        listen: ephemeral_loopback(),
        cookie: SecretCookie::from_bytes(cookie_bytes.clone()).unwrap(),
        tls: alice_tls,
        peers: Vec::new(),
        runtime: graphreact_cli::cli::RuntimeArg::Null,
        net_tick: fast_tick(),
        net_split_policy: NetSplitPolicy::PauseOnSplit,
        duration_secs: None,
        observer_bind: None,
        debug: None,
        debug_wait: false,
        observer_otel_endpoint: None,
        observer_otel_service_name: None,
        drain_deadline_secs: None,
    };
    let alice = start_daemon(alice_opts).await.expect("alice daemon start");
    let alice_addr = alice.cluster().local_addr();

    // Bob is a "daemon" too, with `peers = [alice]`. start_daemon
    // dials alice during the boot phase.
    let bob_opts = DaemonOptions {
        path: None,
        node_id: parse_id("bob@127.0.0.1"),
        listen: ephemeral_loopback(),
        cookie: SecretCookie::from_bytes(cookie_bytes).unwrap(),
        tls: bob_tls,
        peers: vec![(parse_id("alice@127.0.0.1"), alice_addr)],
        runtime: graphreact_cli::cli::RuntimeArg::Null,
        net_tick: fast_tick(),
        net_split_policy: NetSplitPolicy::PauseOnSplit,
        duration_secs: None,
        observer_bind: None,
        debug: None,
        debug_wait: false,
        observer_otel_endpoint: None,
        observer_otel_service_name: None,
        drain_deadline_secs: None,
    };
    let bob = start_daemon(bob_opts).await.expect("bob daemon start");

    // Both sides should converge on a registered peer entry.
    for _ in 0..50 {
        if alice.cluster().registry().get_by_name("bob").is_some()
            && bob.cluster().registry().get_by_name("alice").is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        alice.cluster().registry().get_by_name("bob").is_some(),
        "alice should see bob"
    );
    assert!(
        bob.cluster().registry().get_by_name("alice").is_some(),
        "bob should see alice"
    );

    bob.shutdown().await;
    alice.shutdown().await;
}

#[tokio::test]
async fn daemon_rejects_peer_with_cookie_mismatch() {
    let (alice_tls, bob_tls) = pair_tls("alice", "bob");

    let alice_opts = DaemonOptions {
        path: None,
        node_id: parse_id("alice@127.0.0.1"),
        listen: ephemeral_loopback(),
        cookie: SecretCookie::from_bytes(b"alice-secret".to_vec()).unwrap(),
        tls: alice_tls,
        peers: Vec::new(),
        runtime: graphreact_cli::cli::RuntimeArg::Null,
        net_tick: fast_tick(),
        net_split_policy: NetSplitPolicy::PauseOnSplit,
        duration_secs: None,
        observer_bind: None,
        debug: None,
        debug_wait: false,
        observer_otel_endpoint: None,
        observer_otel_service_name: None,
        drain_deadline_secs: None,
    };
    let alice = start_daemon(alice_opts).await.expect("alice daemon start");
    let alice_addr = alice.cluster().local_addr();

    let bob_opts = DaemonOptions {
        path: None,
        node_id: parse_id("bob@127.0.0.1"),
        listen: ephemeral_loopback(),
        cookie: SecretCookie::from_bytes(b"bob-different-secret".to_vec()).unwrap(),
        tls: bob_tls,
        peers: vec![(parse_id("alice@127.0.0.1"), alice_addr)],
        runtime: graphreact_cli::cli::RuntimeArg::Null,
        net_tick: fast_tick(),
        net_split_policy: NetSplitPolicy::PauseOnSplit,
        duration_secs: None,
        observer_bind: None,
        debug: None,
        debug_wait: false,
        observer_otel_endpoint: None,
        observer_otel_service_name: None,
        drain_deadline_secs: None,
    };
    let result = start_daemon(bob_opts).await;
    let err = match result {
        Ok(_handle) => panic!("expected handshake reject, got success"),
        Err(e) => e,
    };
    let rendered = format!("{err}");
    // Some printable error must be returned. Match-flexible: the
    // dist crate's handshake / cookie / cluster errors all flow
    // through `DistError::Handshake`; we only need a printable
    // "cluster error" line so the user sees something actionable.
    assert!(
        rendered.starts_with("cluster error"),
        "expected printable cluster error, got: {rendered}"
    );

    alice.shutdown().await;
}

#[tokio::test]
#[cfg(unix)]
async fn world_readable_cookie_file_rejected_with_printable_error() {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!(
        "grx-cli-test-cookie-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("cookie");
    let mut f = std::fs::File::create(&path).expect("create cookie file");
    f.write_all(b"some-secret").expect("write cookie");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("chmod 644");

    let err = resolve_cookie(None, Some(&path)).expect_err("expected mode rejection");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("mode"),
        "expected mode-related error, got: {rendered}"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

#[tokio::test]
async fn parse_peer_entry_round_trip() {
    let (id, addr) = parse_peer_entry("bob@127.0.0.1:9700").expect("ok");
    assert_eq!(id.name(), "bob");
    assert_eq!(addr, "127.0.0.1:9700".parse::<SocketAddr>().unwrap());

    let err = parse_peer_entry("not-a-node-id").expect_err("missing @");
    assert!(matches!(err, CommandError::ClusterArg(_)));

    let err = parse_peer_entry("bob@not-an-addr").expect_err("bad addr");
    assert!(matches!(err, CommandError::ClusterArg(_)));
}

/// Tiny, dependency-free random suffix for unique scratch dirs in
/// world-readable-cookie test (running in parallel with other tests
/// could otherwise collide).
fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0xc14_0000);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[allow(dead_code)]
fn _arc_alive() -> Arc<()> {
    Arc::new(())
}
