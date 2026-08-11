//! **M-Prod sub-track #1 Phase B Part 2 (Group 1d) subprocess
//! gate.** End-to-end test for the SIGHUP-fires-spool-rotation
//! path.
//!
//! Mirrors `sighup_jwks_reload_subprocess.rs`: spawn `grx` with
//! `--observer-bind` + `--observer-audit-log` +
//! `--observer-audit-forwarder-spool` pointing at on-disk files.
//! After `grx-sighup-ready`, send SIGHUP via libc inline-FFI and
//! confirm BOTH (a) the daemon stays alive (SIGHUP consumed by
//! the fan-out arm, not the kernel default) and (b) the spool
//! file was atomically rotated (a backup file with the timestamp
//! suffix exists alongside the fresh spool at the original path).

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
        rcgen::CertificateParams::new(vec!["grx-spool-rotate subprocess CA".into()])
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
    46_000_u16 + (u16::try_from(pid % 9_000).unwrap_or(0))
}

#[test]
fn grx_daemon_sighup_rotates_audit_forwarder_spool_subprocess() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert, key, ca) = write_cluster_pem(dir.path());

    let audit_log = dir.path().join("audit.ndjson");
    let spool = dir.path().join("spool.ndjson");

    let cluster_port = pid_derived_port();
    let cluster_listen = format!("127.0.0.1:{cluster_port}");
    let node_id = format!("alice@127.0.0.1:{cluster_port}");

    let cookie = "sighup-spool-rotate-cookie";
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
        .args(["--observer-audit-log"])
        .arg(&audit_log)
        .args(["--observer-audit-forwarder-spool"])
        .arg(&spool)
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

    // Wait until the spool has at least the bind record on it so
    // the rotation has non-empty content to capture.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if spool.exists()
            && std::fs::metadata(&spool)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    send_sighup(pid);

    // Settle window. The daemon's `drive_handle_until` ticks every
    // 50ms; SIGHUP is dispatched in-loop. The fan-out runs
    // audit-rotate → spool-rotate → tokens → JWKS → TLS. We need
    // the spool-rotate arm to land + write its backup file.
    std::thread::sleep(Duration::from_millis(800));

    // Daemon MUST still be running — SIGHUP didn't terminate it.
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

    // Spool rotation invariant: a backup file with the spool path
    // as prefix + a timestamp suffix exists in the dir.
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .expect("readdir")
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().into_string().unwrap_or_default())
        .collect();
    let rotated = entries
        .iter()
        .any(|name| name.starts_with("spool.ndjson.") && name.ends_with('Z'));
    assert!(
        rotated,
        "expected a spool.ndjson.<timestamp>Z backup file from the SIGHUP rotation; saw {entries:?}"
    );
}
