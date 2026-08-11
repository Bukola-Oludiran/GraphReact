//! **M-Prod sub-track #1 Phase B Part 2 (Group 1ab) subprocess
//! gate.** End-to-end test for the SIGHUP-fires-token-reload path.
//!
//! Mirrors `sighup_cert_rotation_subprocess.rs`: spawn the `grx`
//! binary as a real child process with `--observer-bind` +
//! `--observer-tokens` pointing at an on-disk NDJSON registry,
//! watch for the `grx-sighup-ready <pid>` line, mutate the
//! registry file, `kill(pid, SIGHUP)` via the libc inline-FFI
//! declared in `admin_commands.rs`, and confirm the daemon
//! continues to run (the SIGHUP did not terminate it — the
//! reload arm consumed it).

#![cfg(unix)]
#![allow(unsafe_code)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;

fn write_cluster_pem(dir: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
    let ca_kp = rcgen::KeyPair::generate().expect("ca keygen");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["grx-token-reload subprocess CA".into()])
            .expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-sign");
    let ca_pem = ca_cert.pem();
    let ca_path = dir.join("ca.pem");
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
    (alice_cert, alice_key, ca_path)
}

fn write_0600(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

fn wait_for_sighup_ready(child: &mut std::process::Child, deadline: Duration) -> u32 {
    let stdout = child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);
    let start = Instant::now();
    let mut pid: Option<u32> = None;
    for line in reader.lines() {
        if start.elapsed() > deadline {
            let _ = child.kill();
            panic!("daemon never emitted grx-sighup-ready");
        }
        let line = line.expect("read");
        eprintln!("[daemon stdout] {line}");
        if let Some(rest) = line.trim().strip_prefix("grx-sighup-ready ") {
            pid = Some(rest.parse().expect("pid"));
            break;
        }
    }
    pid.expect("sighup-ready")
}

fn send_sighup(pid: u32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let pid_t: i32 = i32::try_from(pid).expect("pid_t");
    // SAFETY: kill with valid pid + SIGHUP is standard POSIX.
    let rc = unsafe { kill(pid_t, 1) };
    assert_eq!(
        rc,
        0,
        "kill SIGHUP failed: {}",
        std::io::Error::last_os_error()
    );
}

fn pid_derived_port() -> u16 {
    let pid = std::process::id();
    35_000_u16 + (u16::try_from(pid % 9_000).unwrap_or(0))
}

#[test]
fn grx_daemon_sighup_reloads_tokens_subprocess() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert, key, ca) = write_cluster_pem(dir.path());

    let tokens_path = dir.path().join("tokens.ndjson");
    write_0600(
        &tokens_path,
        r#"{"id":"alice","token":"AAAAAAAAAA","role":"Admin"}
"#,
    );

    let cluster_port = pid_derived_port();
    let cluster_listen = format!("127.0.0.1:{cluster_port}");
    let node_id = format!("alice@127.0.0.1:{cluster_port}");

    let cookie = "sighup-tokens-cookie";
    let bin = StdCommand::cargo_bin("grx").expect("locate grx");
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
        .arg(&cert)
        .args(["--key"])
        .arg(&key)
        .args(["--ca"])
        .arg(&ca)
        .args(["--observer-bind", "127.0.0.1:0"])
        .args(["--observer-tokens"])
        .arg(&tokens_path)
        .args(["--duration-secs", "10"])
        .env("GRX_DAEMON_COOKIE", cookie)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grx");

    let pid = wait_for_sighup_ready(&mut child, Duration::from_secs(20));

    // Drain stderr in background.
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("[daemon stderr] {line}");
            }
        });
    }

    // Append a second token + fire SIGHUP.
    write_0600(
        &tokens_path,
        r#"{"id":"alice","token":"AAAAAAAAAA","role":"Admin"}
{"id":"reader","token":"BBBBBBBBBB","role":"ReadOnly"}
"#,
    );
    send_sighup(pid);

    // Settle window. The daemon's `drive_handle_until` ticks every
    // 50ms; SIGHUP is dispatched in-loop.
    std::thread::sleep(Duration::from_millis(500));

    // Daemon MUST still be running (SIGHUP didn't terminate it —
    // the handler arm consumed it).
    match child.try_wait().expect("try_wait") {
        None => {
            // Still running — kill it cleanly.
            let _ = child.kill();
            let _ = child.wait();
        }
        Some(status) => {
            panic!(
                "daemon exited after SIGHUP with status {status:?}; \
                 SIGHUP handler arm should have consumed the signal"
            );
        }
    }
}
