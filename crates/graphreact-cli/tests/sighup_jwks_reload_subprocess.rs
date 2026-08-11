//! **M-Prod sub-track #1 Phase B Part 2 (Group 1c) subprocess
//! gate.** End-to-end test for the SIGHUP-fires-JWKS-reload path.
//!
//! Mirrors `sighup_token_reload_subprocess.rs`: spawn the `grx`
//! binary as a real child process with `--observer-bind` +
//! `--observer-jwks` + `--observer-jwt-config` pointing at on-disk
//! JWKS + bridge-config files. Watch for the
//! `grx-sighup-ready <pid>` line, mutate the JWKS file in place,
//! `kill(pid, SIGHUP)` via the libc inline-FFI, and confirm the
//! daemon stays alive (the SIGHUP did not terminate it — the
//! Group 1ab + 1c fan-out consumed it).

#![cfg(unix)]
#![allow(unsafe_code)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;

const TEST_JWKS_INITIAL: &str = r#"{"keys":[{"kty":"RSA","kid":"test-kid-2026-q1","use":"sig","alg":"RS256","n":"qy3Ol9hpZ-YyDrMnDVH5uvKY2Rh1U2KjPiIt5krvGvIDQQPR4EJcD0coz7R1uKAZTOh147tczsmEy4zO6x8lFQS5AYM2fVMUGiXqGoi3JJoCVoF95hT5g98r2NXSViXyuh2EIf6z1EocII4svG70nhVWxQw46pK29Auq7hkiwKTERXo_tEp8TeiCbaEg5Sgb6A_r_1cWd_BPdJNi7-cZMr-HqcDrNXm_u45prECnNlVVofXiowUBfbOSpW9iWwpd3G7vfTlNIt8TlccDkp9HlUDHOIy1FrUU5EbkF0ZbI96Gmw2y0Hu8nLzJLuAAeIo0FcbNwMRQ3pI-o3sZ5ZTpxw","e":"AQAB"}]}"#;

const TEST_JWKS_ROTATED: &str = r#"{"keys":[{"kty":"RSA","kid":"test-kid-2026-q1","use":"sig","alg":"RS256","n":"qy3Ol9hpZ-YyDrMnDVH5uvKY2Rh1U2KjPiIt5krvGvIDQQPR4EJcD0coz7R1uKAZTOh147tczsmEy4zO6x8lFQS5AYM2fVMUGiXqGoi3JJoCVoF95hT5g98r2NXSViXyuh2EIf6z1EocII4svG70nhVWxQw46pK29Auq7hkiwKTERXo_tEp8TeiCbaEg5Sgb6A_r_1cWd_BPdJNi7-cZMr-HqcDrNXm_u45prECnNlVVofXiowUBfbOSpW9iWwpd3G7vfTlNIt8TlccDkp9HlUDHOIy1FrUU5EbkF0ZbI96Gmw2y0Hu8nLzJLuAAeIo0FcbNwMRQ3pI-o3sZ5ZTpxw","e":"AQAB"},{"kty":"RSA","kid":"test-kid-2026-q2","use":"sig","alg":"RS256","n":"qy3Ol9hpZ-YyDrMnDVH5uvKY2Rh1U2KjPiIt5krvGvIDQQPR4EJcD0coz7R1uKAZTOh147tczsmEy4zO6x8lFQS5AYM2fVMUGiXqGoi3JJoCVoF95hT5g98r2NXSViXyuh2EIf6z1EocII4svG70nhVWxQw46pK29Auq7hkiwKTERXo_tEp8TeiCbaEg5Sgb6A_r_1cWd_BPdJNi7-cZMr-HqcDrNXm_u45prECnNlVVofXiowUBfbOSpW9iWwpd3G7vfTlNIt8TlccDkp9HlUDHOIy1FrUU5EbkF0ZbI96Gmw2y0Hu8nLzJLuAAeIo0FcbNwMRQ3pI-o3sZ5ZTpxw","e":"AQAB"}]}"#;

const TEST_JWT_CONFIG: &str = r#"{
  "expected_issuers": ["https://issuer.example.com"],
  "expected_audiences": ["grx-management-api"],
  "allowed_algorithms": ["RS256"],
  "leeway_seconds": 60,
  "role_mapping": {
    "role_claim": "role",
    "admin_values": ["grx-admin"],
    "read_only_values": ["grx-reader"],
    "custom_role_mappings": {}
  }
}"#;

fn write_cluster_pem(dir: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
    let ca_kp = rcgen::KeyPair::generate().expect("ca keygen");
    let mut ca_params = rcgen::CertificateParams::new(vec!["grx-jwks-reload subprocess CA".into()])
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

/// Write a JWKS file with default mode (NOT 0o600 — JWKS keys are
/// PUBLIC by convention; ADR-0010 documents the deliberate
/// deviation).
fn write_jwks(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("write jwks");
}

fn write_tokens_0600(path: &std::path::Path) {
    write_0600(
        path,
        r#"{"id":"alice","token":"AAAAAAAAAA","role":"Admin"}
"#,
    );
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
    44_000_u16 + (u16::try_from(pid % 9_000).unwrap_or(0))
}

#[test]
fn grx_daemon_sighup_reloads_jwks_subprocess() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert, key, ca) = write_cluster_pem(dir.path());

    // Tokens file is required for RBAC dispatch — the JWT bridge
    // sits INSIDE auth_layer_rbac, so the daemon needs an RBAC
    // registry to even reach the JWT path. Loading a minimal
    // static registry alongside the JWT bridge is the realistic
    // production shape.
    let tokens_path = dir.path().join("tokens.ndjson");
    write_tokens_0600(&tokens_path);

    let jwks_path = dir.path().join("jwks.json");
    write_jwks(&jwks_path, TEST_JWKS_INITIAL);

    let jwt_config_path = dir.path().join("jwt-config.json");
    write_0600(&jwt_config_path, TEST_JWT_CONFIG);

    let cluster_port = pid_derived_port();
    let cluster_listen = format!("127.0.0.1:{cluster_port}");
    let node_id = format!("alice@127.0.0.1:{cluster_port}");

    let cookie = "sighup-jwks-cookie";
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
        .args(["--observer-jwks"])
        .arg(&jwks_path)
        .args(["--observer-jwt-config"])
        .arg(&jwt_config_path)
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

    // Rotate the JWKS in place (add a second kid) + fire SIGHUP.
    write_jwks(&jwks_path, TEST_JWKS_ROTATED);
    send_sighup(pid);

    // Settle window. The daemon's `drive_handle_until` ticks every
    // 50ms; SIGHUP is dispatched in-loop. The fan-out runs audit →
    // tokens → jwks → TLS.
    std::thread::sleep(Duration::from_millis(500));

    // Daemon MUST still be running — SIGHUP didn't terminate it,
    // the Group 1ab + 1c fan-out consumed it.
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

#[test]
fn grx_daemon_half_jwt_config_fails_fast() {
    // --observer-jwt-config without --observer-jwks must fail
    // fast at boot (the bridge needs BOTH halves). clap's
    // `requires = "observer_jwks"` enforces this at the parse
    // level, so the daemon exits with a non-zero status before
    // ever printing the readiness line.
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert, key, ca) = write_cluster_pem(dir.path());
    let jwt_config_path = dir.path().join("jwt-config.json");
    write_0600(&jwt_config_path, TEST_JWT_CONFIG);

    let cluster_port = pid_derived_port() + 1;
    let cluster_listen = format!("127.0.0.1:{cluster_port}");
    let node_id = format!("alice@127.0.0.1:{cluster_port}");

    let bin = StdCommand::cargo_bin("grx").expect("locate grx");
    let output = StdCommand::new(bin.get_program())
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
        .args(["--observer-jwt-config"])
        .arg(&jwt_config_path)
        .args(["--duration-secs", "5"])
        .env("GRX_DAEMON_COOKIE", "fail-fast-cookie")
        .output()
        .expect("run grx");
    assert!(
        !output.status.success(),
        "daemon should fail-fast when --observer-jwt-config is set without --observer-jwks; stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
