//! M14.8 boundary closeout — **real-binary** CLI subprocess test.
//!
//! The library-level tests in `cluster_cli.rs` exercise
//! `start_daemon` / `resolve_cookie` directly. This test spawns
//! the `grx` binary itself via `assert_cmd::Command` so the
//! clap-derived argument schema, the binary's runtime
//! initialisation (multi-thread tokio, signal handlers), and the
//! readiness-print contract are all exercised end-to-end.
//!
//! Acceptance: `grx cluster daemon` on port A binds + emits
//! `grx-daemon-ready <addr>` to stdout; `grx cluster connect`
//! against that address handshakes + exits success after the
//! configured `--duration-secs`.
//!
//! Skipped on non-macOS targets because the JSC engine path
//! (`mount_with_scheduler`) is macOS-only today; the daemon's
//! engine-free path (`--runtime null`) is portable but doesn't
//! exercise the engine-hosted shape.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;

/// Render a fresh CA + two leaf certs to PEM files in a tempdir,
/// returning paths the binary can read via `--cert` / `--key` / `--ca`.
struct PemArtifacts {
    _tempdir: tempfile::TempDir,
    pub alice_cert: PathBuf,
    pub alice_key: PathBuf,
    pub bob_cert: PathBuf,
    pub bob_key: PathBuf,
    pub ca: PathBuf,
}

fn write_pem_artifacts(name_a: &str, name_b: &str) -> PemArtifacts {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_kp = rcgen::KeyPair::generate().expect("ca keygen");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["grx-cli subprocess CA".into()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-sign");
    let ca_pem = ca_cert.pem();
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, ca_pem).expect("write ca pem");

    let mk_leaf = |name: &str, base: &str| -> (PathBuf, PathBuf) {
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).expect("leaf params");
        params.subject_alt_names.push(rcgen::SanType::DnsName(
            name.to_string().try_into().expect("san name"),
        ));
        let kp = rcgen::KeyPair::generate().expect("leaf keygen");
        let cert = params.signed_by(&kp, &ca_cert, &ca_kp).expect("leaf sign");
        let cert_pem = cert.pem();
        let key_pem = kp.serialize_pem();
        let cert_path = dir.path().join(format!("{base}-cert.pem"));
        let key_path = dir.path().join(format!("{base}-key.pem"));
        std::fs::write(&cert_path, cert_pem).expect("write cert pem");
        std::fs::write(&key_path, key_pem).expect("write key pem");
        (cert_path, key_path)
    };
    let (alice_cert, alice_key) = mk_leaf(name_a, "alice");
    let (bob_cert, bob_key) = mk_leaf(name_b, "bob");

    PemArtifacts {
        _tempdir: dir,
        alice_cert,
        alice_key,
        bob_cert,
        bob_key,
        ca: ca_path,
    }
}

/// Pick an ephemeral-but-stable loopback port for this test
/// process. The daemon's NodeId AND --listen pin to the same
/// `host:port` form so handshake validation (which compares full
/// `NodeId`s) matches the connector's peer spec verbatim.
fn pid_derived_port() -> u16 {
    let pid = std::process::id();
    // Spread to a low-conflict range. Port-0 would be cleaner but
    // requires the connector to learn the port AFTER the daemon
    // emits it on stdout, AND the daemon's --node-id to use the
    // SAME port — a chicken-and-egg.
    30_000_u16 + (u16::try_from(pid % 9_000).unwrap_or(0))
}

/// Launch `grx cluster daemon` as a subprocess on the given port
/// and read its stdout until the `grx-daemon-ready <addr>` line
/// appears (used as a barrier — the addr is already known).
///
/// The caller is responsible for `kill()` + `wait()` on the
/// returned child to avoid zombie processes; `clippy::zombie_processes`
/// would flag if the test panicked between spawn and reap, but
/// the test always reaps via the explicit `kill` + `wait` pair
/// in the test body.
#[allow(clippy::zombie_processes)]
fn spawn_daemon(
    cookie: &str,
    node_id_with_port: &str,
    listen: &str,
    cert: &PathBuf,
    key: &PathBuf,
    ca: &PathBuf,
) -> std::process::Child {
    let bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let mut child = StdCommand::new(bin.get_program())
        .args([
            "cluster",
            "daemon",
            "--node-id",
            node_id_with_port,
            "--listen",
            listen,
            "--cookie-env",
            "GRX_DAEMON_COOKIE",
            "--cert",
        ])
        .arg(cert)
        .args(["--key"])
        .arg(key)
        .args(["--ca"])
        .arg(ca)
        // Long enough that the connect subprocess can complete
        // its handshake + duration window before the daemon
        // self-terminates. 10s is way more than enough.
        .args(["--duration-secs", "10"])
        .env("GRX_DAEMON_COOKIE", cookie)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grx cluster daemon");

    let stdout = child.stdout.take().expect("daemon stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("daemon did not emit `grx-daemon-ready` within 15s");
        }
        let mut line = String::new();
        if reader.read_line(&mut line).expect("read daemon stdout") == 0 {
            let stderr = child
                .stderr
                .take()
                .map(|stderr| std::io::read_to_string(stderr).unwrap_or_default())
                .unwrap_or_default();
            let _ = child.kill();
            panic!("daemon stdout closed before ready signal; stderr: {stderr}");
        }
        if let Some(rest) = line.trim().strip_prefix("grx-daemon-ready ") {
            // Sanity: the bound addr matches what we asked for.
            let _: SocketAddr = rest.parse().expect("parse addr from ready line");
            // Re-attach stdout so subsequent lines can be drained.
            child.stdout = Some(reader.into_inner());
            return child;
        }
    }
}

#[test]
fn grx_cluster_daemon_accepts_connect_subprocess() {
    let pem = write_pem_artifacts("alice", "bob");
    let cookie = "subprocess-cookie";
    let port = pid_derived_port();
    let listen = format!("127.0.0.1:{port}");
    let alice_node_id = format!("alice@127.0.0.1:{port}");
    let addr = listen.clone();

    let mut daemon = spawn_daemon(
        cookie,
        &alice_node_id,
        &listen,
        &pem.alice_cert,
        &pem.alice_key,
        &pem.ca,
    );

    // Drain the daemon's stdout in a background thread so its
    // pipe buffer doesn't fill during the connect window. (The
    // daemon may emit additional log lines once the connector
    // attaches.)
    if let Some(stdout) = daemon.stdout.take() {
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("[daemon stdout] {line}");
            }
        });
    }
    if let Some(stderr) = daemon.stderr.take() {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("[daemon stderr] {line}");
            }
        });
    }

    // Run `grx cluster connect` against the daemon. With
    // matching cookie + cert chain, the handshake completes; we
    // pass `--duration-secs 2` so the connector exits success
    // after a brief settle window.
    let bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let connect_output = StdCommand::new(bin.get_program())
        .args([
            "cluster",
            "connect",
            // Match the daemon's announced NodeId verbatim so
            // handshake validation succeeds.
            &format!("alice@{addr}"),
            "--node-id",
            "connector@127.0.0.1",
            "--listen",
            "127.0.0.1:0",
            "--cookie-env",
            "GRX_CONNECT_COOKIE",
            "--cert",
        ])
        .arg(&pem.bob_cert)
        .args(["--key"])
        .arg(&pem.bob_key)
        .args(["--ca"])
        .arg(&pem.ca)
        .args(["--duration-secs", "2"])
        .env("GRX_CONNECT_COOKIE", cookie)
        .output()
        .expect("spawn + wait grx cluster connect");

    // Reap the daemon. It will still be running (10s duration);
    // SIGTERM is enough for the orderly shutdown path. On Unix
    // assert_cmd / Child::kill sends SIGKILL — that's also fine
    // here because we've already validated the connect
    // succeeded.
    let _ = daemon.kill();
    let _ = daemon.wait();

    assert!(
        connect_output.status.success(),
        "`grx cluster connect` exited with status {:?}\nstdout: {}\nstderr: {}",
        connect_output.status,
        String::from_utf8_lossy(&connect_output.stdout),
        String::from_utf8_lossy(&connect_output.stderr),
    );
}

#[test]
fn grx_cluster_help_prints_help_text() {
    // Smoke-check that clap's derive renders without panicking
    // for the new subcommand group.
    let bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let child = StdCommand::new(bin.get_program())
        .args(["cluster", "--help"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grx cluster --help");
    let output = child.wait_with_output().expect("wait help");
    assert!(output.status.success(), "grx cluster --help failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("daemon") && stdout.contains("connect"),
        "expected help text to list daemon + connect subcommands"
    );
}

/// Convince the linter the import is used even when the test
/// body uses `tempfile::tempdir` only via path inference.
#[allow(dead_code)]
fn _io_write_used() {
    let mut buf = Vec::<u8>::new();
    let _ = buf.write_all(b"x");
}
