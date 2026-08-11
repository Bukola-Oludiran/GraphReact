//! **M-Prod sub-track #2 Phase B Part 1 subprocess gate.** End-to-
//! end test for the SIGHUP-fires-real-signal path.
//!
//! Mirrors `cluster_cli_subprocess.rs`: spawn the `grx` binary as a
//! real child process with `--observer-bind` + `--observer-cert` +
//! `--observer-key` pointing at on-disk PEM files, watch for the
//! `grx-observer-ready <addr>` line on stdout, then overwrite the
//! cert + key files with a fresh pair and `libc::kill(pid, SIGHUP)`
//! the child. After a brief settle window, a fresh TLS handshake
//! against the observer addr MUST see the new leaf cert.
//!
//! `raise(SIGHUP)` from an in-process tokio test would interrupt
//! every parallel cargo test worker — the subprocess pattern keeps
//! the signal isolated to the child PID.

#![cfg(unix)]
// `libc::kill(pid, SIGHUP)` is the only POSIX-portable way to fire
// SIGHUP at a child process; mirror's `graphreact-engine-go`'s
// stress-test precedent (libc::_exit watchdog allow).
#![allow(unsafe_code)]

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;
use rustls::pki_types::CertificateDer;
use rustls::ALL_VERSIONS;

/// A CA-signed leaf observer-TLS pair. The webpki verifier in
/// `tokio_rustls` requires a real CA->leaf chain (it rejects
/// self-signed end-entity certs and CA-as-end-entity certs alike);
/// we mint a fresh CA + leaf per pair and surface BOTH the leaf
/// PEM (what the daemon serves) and the CA DER (what the client
/// trusts as the root).
struct TestPair {
    cert_path: PathBuf,
    key_path: PathBuf,
    ca_der: CertificateDer<'static>,
    sni: String,
}

fn write_leaf_pair(dir: &std::path::Path, base: &str, sni: &str) -> TestPair {
    let ca_kp = rcgen::KeyPair::generate().expect("ca kp");
    let mut ca_params =
        rcgen::CertificateParams::new(vec![format!("{base}-CA")]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-sign");
    let ca_der: CertificateDer<'static> = ca_cert.der().clone();

    let mut leaf_params =
        rcgen::CertificateParams::new(vec![sni.to_string()]).expect("leaf params");
    leaf_params.subject_alt_names.push(rcgen::SanType::DnsName(
        sni.to_string().try_into().expect("san"),
    ));
    let leaf_kp = rcgen::KeyPair::generate().expect("leaf kp");
    let leaf_cert = leaf_params
        .signed_by(&leaf_kp, &ca_cert, &ca_kp)
        .expect("leaf sign");
    let cert_pem = leaf_cert.pem();
    let key_pem = leaf_kp.serialize_pem();
    let cert_path = dir.join(format!("{base}-cert.pem"));
    let key_path = dir.join(format!("{base}-key.pem"));
    std::fs::write(&cert_path, cert_pem).expect("write cert pem");
    std::fs::write(&key_path, key_pem).expect("write key pem");
    TestPair {
        cert_path,
        key_path,
        ca_der,
        sni: sni.to_string(),
    }
}

/// Cluster cert chain — separate from the observer cert. Uses a
/// real CA so the cluster handshake succeeds. (The cluster TLS
/// requires both leaves to share a CA so peers cross-validate; the
/// observer TLS is self-signed because the test client trusts the
/// leaf directly.)
struct ClusterPemPaths {
    alice_cert: PathBuf,
    alice_key: PathBuf,
    ca: PathBuf,
}

fn write_cluster_pem(dir: &std::path::Path) -> ClusterPemPaths {
    let ca_kp = rcgen::KeyPair::generate().expect("ca keygen");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["grx-sighup subprocess CA".into()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-sign");
    let ca_pem = ca_cert.pem();
    let ca_path = dir.join("cluster-ca.pem");
    std::fs::write(&ca_path, ca_pem).expect("write ca pem");

    let mut params = rcgen::CertificateParams::new(vec!["alice".into()]).expect("leaf params");
    params.subject_alt_names.push(rcgen::SanType::DnsName(
        "alice".to_string().try_into().expect("san"),
    ));
    let kp = rcgen::KeyPair::generate().expect("leaf keygen");
    let cert = params.signed_by(&kp, &ca_cert, &ca_kp).expect("leaf sign");
    let cert_pem = cert.pem();
    let key_pem = kp.serialize_pem();
    let alice_cert = dir.join("alice-cert.pem");
    let alice_key = dir.join("alice-key.pem");
    std::fs::write(&alice_cert, cert_pem).expect("write alice cert");
    std::fs::write(&alice_key, key_pem).expect("write alice key");
    ClusterPemPaths {
        alice_cert,
        alice_key,
        ca: ca_path,
    }
}

/// Dial the observer (HTTPS) trusting `expected_root` as the only
/// CA, and return the leaf cert the server actually presented.
/// Done synchronously via a current-thread tokio runtime so the
/// test driver (which is itself a `#[test]`, not `#[tokio::test]`)
/// stays simple.
fn dial_observer_capture_cert(
    addr: SocketAddr,
    sni: &str,
    expected_root: &CertificateDer<'static>,
) -> CertificateDer<'static> {
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt build");
    rt.block_on(async {
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
        let dns_name =
            rustls::pki_types::ServerName::try_from(sni.to_string()).expect("sni parses");
        let tls = connector
            .connect(dns_name, tcp)
            .await
            .expect("tls handshake");
        let (_io, session) = tls.get_ref();
        let chain = session
            .peer_certificates()
            .expect("peer presented a cert chain");
        let leaf = chain[0].clone();
        // Try to gracefully close so the next handshake on the
        // same socket doesn't see a half-closed peer (best-effort
        // — failure is fine, the cert capture is what we need).
        drop(tls);
        let _ = TcpStream::connect(addr).await; // keep-alive pause
        leaf
    })
}

/// Watch the child's stdout for the daemon readiness lines:
/// `grx-daemon-ready`, `grx-observer-ready <addr>`, AND
/// `grx-sighup-ready <pid>` (verify-finding patch — emitted by
/// `drive_handle_until` AFTER it registers the SIGHUP signal stream
/// so operator tooling can wait for it before firing SIGHUP, closing
/// the prior 200ms-sleep race window). Returns the observer's
/// bound socket addr once all three are seen. Each line passes
/// through to stderr for test-driver triage.
fn wait_for_observer_ready(child: &mut std::process::Child, deadline: Duration) -> SocketAddr {
    let stdout = child.stdout.take().expect("daemon stdout");
    let reader = BufReader::new(stdout);
    let start = Instant::now();
    let mut observer_addr: Option<SocketAddr> = None;
    let mut sighup_ready = false;
    for line in reader.lines() {
        if start.elapsed() > deadline {
            let _ = child.kill();
            panic!(
                "daemon did not emit `grx-observer-ready` + \
                 `grx-sighup-ready` within {deadline:?}"
            );
        }
        let line = line.expect("read stdout");
        eprintln!("[daemon stdout] {line}");
        if let Some(rest) = line.trim().strip_prefix("grx-observer-ready ") {
            observer_addr = Some(rest.parse().expect("observer addr"));
        }
        if line.trim().starts_with("grx-sighup-ready ") {
            sighup_ready = true;
        }
        if observer_addr.is_some() && sighup_ready {
            break;
        }
    }
    assert!(
        sighup_ready,
        "daemon emitted observer-ready but never grx-sighup-ready"
    );
    observer_addr.expect("observer-ready line")
}

/// Send SIGHUP to the child process. `libc::kill` is in the dep
/// graph via several engine crates; declared as a CLI dev-dep so
/// the use site is documented.
fn send_sighup(pid: u32) {
    // `libc::pid_t` is `i32` on every Unix; child PIDs returned by
    // `std::process::Child::id()` are always under `i32::MAX` for
    // POSIX (the OS allocates them from a 16- or 32-bit pid space
    // that never overflows on a healthy kernel). Use `try_into` so
    // a pathologically large pid surfaces a clean panic instead of
    // wrapping to negative (clippy::cast_possible_wrap).
    let signed_pid: libc::pid_t = libc::pid_t::try_from(pid).expect("pid fits libc::pid_t");
    // SAFETY: `libc::kill` with a valid pid + SIGHUP is a standard
    // POSIX operation. The child PID is owned by us (we spawned it);
    // SIGHUP is a no-op for any process that doesn't install a
    // handler, which our grx binary does via `drive_handle_until`.
    let rc = unsafe { libc::kill(signed_pid, libc::SIGHUP) };
    assert_eq!(
        rc,
        0,
        "libc::kill(pid={pid}, SIGHUP) returned errno={}",
        std::io::Error::last_os_error()
    );
}

fn pid_derived_port() -> u16 {
    let pid = std::process::id();
    30_000_u16 + (u16::try_from(pid % 9_000).unwrap_or(0))
}

#[test]
fn grx_daemon_sighup_swaps_observer_cert_subprocess() {
    let dir = tempfile::tempdir().expect("tempdir");
    let observer_pair_a = write_leaf_pair(dir.path(), "observer-a", "sighup-a.test");
    let observer_pair_b = write_leaf_pair(dir.path(), "observer-b", "sighup-b.test");
    let cluster = write_cluster_pem(dir.path());

    // The daemon's --observer-cert / --observer-key flags point at
    // stable filenames so the SIGHUP re-read targets the same
    // paths after we overwrite them with cert B material.
    let observer_cert_path = dir.path().join("observer-cert.pem");
    let observer_key_path = dir.path().join("observer-key.pem");
    std::fs::copy(&observer_pair_a.cert_path, &observer_cert_path).expect("seed cert A");
    std::fs::copy(&observer_pair_a.key_path, &observer_key_path).expect("seed key A");

    let cluster_port = pid_derived_port();
    let cluster_listen = format!("127.0.0.1:{cluster_port}");
    let node_id = format!("alice@127.0.0.1:{cluster_port}");

    // **Loopback observer bind + cert + key**. The CLI's
    // BindConfig::from_flags rule says ephemeral loopback does NOT
    // require a token; TLS-on-loopback is allowed for testing the
    // rotation contract without needing to mint a token file.
    let cookie = "sighup-subprocess-cookie";
    let bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let mut child = StdCommand::new(bin.get_program())
        .args([
            "cluster",
            "daemon",
            "--node-id",
            &node_id,
            "--listen",
            &cluster_listen,
            "--cookie-env",
            "GRX_DAEMON_COOKIE",
            "--cert",
        ])
        .arg(&cluster.alice_cert)
        .args(["--key"])
        .arg(&cluster.alice_key)
        .args(["--ca"])
        .arg(&cluster.ca)
        .args(["--observer-bind", "127.0.0.1:0"])
        .args(["--observer-cert"])
        .arg(&observer_cert_path)
        .args(["--observer-key"])
        .arg(&observer_key_path)
        // 20s window — gives us enough time to seed cert A, dial,
        // overwrite the file, send SIGHUP, dial again.
        .args(["--duration-secs", "20"])
        .env("GRX_DAEMON_COOKIE", cookie)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grx cluster daemon");

    let observer_addr = wait_for_observer_ready(&mut child, Duration::from_secs(20));

    // Drain daemon stderr in background so its pipe buffer
    // doesn't fill while we work.
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("[daemon stderr] {line}");
            }
        });
    }

    // **Verify-finding patch (MED).** The 200ms sleep is gone —
    // `wait_for_observer_ready` above now waits for the
    // `grx-sighup-ready <pid>` line emitted AFTER
    // `drive_handle_until` installs the SIGHUP signal stream. The
    // race window between `start_daemon` returning and the
    // SIGHUP handler being installed is now observable via stdout;
    // production operator tooling (cert-manager hooks, k8s preStop)
    // waits for the same line.

    // First handshake — must succeed under CA A's trust anchor.
    // We don't directly compare leaf bytes (the leaf cert DER
    // isn't surfaced from the helper; what matters for the
    // rotation contract is that the post-rotation client CAN'T
    // succeed under CA A but CAN succeed under CA B). The
    // assertion below proves the negative: dialing with CA A
    // post-rotation panics inside `dial_observer_capture_cert`
    // because the server now serves cert B signed by CA B.
    let _observed_a =
        dial_observer_capture_cert(observer_addr, &observer_pair_a.sni, &observer_pair_a.ca_der);

    // Overwrite cert + key files with cert B material.
    std::fs::copy(&observer_pair_b.cert_path, &observer_cert_path).expect("overwrite cert B");
    std::fs::copy(&observer_pair_b.key_path, &observer_key_path).expect("overwrite key B");

    // Fire the signal.
    let pid = child.id();
    send_sighup(pid);

    // Wait for the rotation to land. The daemon's
    // drive_handle_until ticks every 50ms; SIGHUP is dispatched
    // in-loop. Poll on a successful cert-B handshake; bound by
    // 5 seconds. Polling pattern matches `cluster_cli_subprocess`
    // — best-effort dial in a loop, surface a clear panic on
    // deadline expiry.
    let deadline = Instant::now() + Duration::from_secs(5);
    let post_handshake_ok = loop {
        // The dial helper unwraps internally; to poll for success
        // we wrap it in catch_unwind. The helper is panic-only;
        // any handshake error panics through the `.expect()`.
        let attempt = std::panic::catch_unwind(|| {
            dial_observer_capture_cert(observer_addr, &observer_pair_b.sni, &observer_pair_b.ca_der)
        });
        if attempt.is_ok() {
            break true;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("post-SIGHUP handshake never succeeded under CA B within 5s");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        post_handshake_ok,
        "post-SIGHUP handshake must succeed under CA B"
    );

    // Negative: under CA A's trust anchor, the post-rotation
    // handshake MUST fail (server now serves cert B signed by CA
    // B; the client trusts only CA A, so webpki rejects the
    // chain). Catch the panic so the test reports cleanly.
    let neg = std::panic::catch_unwind(|| {
        dial_observer_capture_cert(observer_addr, &observer_pair_a.sni, &observer_pair_a.ca_der)
    });
    assert!(
        neg.is_err(),
        "post-SIGHUP handshake under CA A's root MUST fail (server now serves cert B)"
    );

    // Reap. `kill` sends SIGKILL but we already validated; clean
    // exit is fine if the duration window happens to elapse first.
    let _ = child.kill();
    let _ = child.wait();
}
