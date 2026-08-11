//! **M-Ingress phase 4 closeout — real-binary E2E test.**
//!
//! Spawns `grx cluster daemon` as a subprocess with the full M-Ingress
//! stack: a Python `.grx` file mounted with a single node declaring
//! `accepts_external: ['ping']`, the observer HTTP server bound on
//! loopback with a bearer token, and the management API live. Then
//! the test:
//!
//! 1. POSTs `{type: "ping", payload: {...}}` to
//!    `/v1/nodes/<id>/messages` and asserts 202 Accepted.
//! 2. Polls `/v1/nodes/<id>/messages/recent` until the per-node
//!    ring buffer surfaces the injected message — confirms the
//!    end-to-end path from HTTP → bridge → scheduler → manifest gate
//!    → telemetry sink.
//! 3. POSTs a `type` NOT declared in `accepts_external` and asserts
//!    delivery rejection surfaces in the ring buffer with
//!    `accepted=false`.
//! 4. POSTs an unknown node id and asserts HTTP 404.
//!
//! Loopback bind only (no-TLS path) so the test doesn't need cert
//! artifacts beyond the cluster handshake's leaf cert. Same shape as
//! `observer_e2e_subprocess.rs` — pid-derived ports + duration-secs
//! self-termination + assert_cmd cargo_bin spawn.

#![cfg(unix)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;

/// Minimal `.grx` fixture: one node accepting `ping` from external
/// HTTP injection (`POST /v1/nodes/:id/messages`). Python body so the
/// CPython engine compiled into the binary can mount it.
const INGRESS_FIXTURE: &str = r#"<Receiver lang="python">
def body(ctx):
    def on_ping(msg):
        pass
    use_message('ping', on_ping)
    return {
        'accepts': ['ping'],
        'emits': [],
        'accepts_external': ['ping'],
    }
</Receiver>

<Graph id="ingress-e2e">
  <Receiver id="receiver" />
</Graph>
"#;

struct PemArtifacts {
    _tempdir: tempfile::TempDir,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub ca: PathBuf,
    pub grx_file: PathBuf,
}

fn write_pem_artifacts(name: &str) -> PemArtifacts {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_kp = rcgen::KeyPair::generate().expect("ca keygen");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["grx ingress-e2e CA".into()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-sign");
    let ca_pem = ca_cert.pem();
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, ca_pem).expect("write ca pem");

    let mut params = rcgen::CertificateParams::new(vec![name.into()]).expect("leaf params");
    params.subject_alt_names.push(rcgen::SanType::DnsName(
        name.to_string().try_into().expect("san name"),
    ));
    let kp = rcgen::KeyPair::generate().expect("leaf keygen");
    let cert = params.signed_by(&kp, &ca_cert, &ca_kp).expect("leaf sign");
    let cert_pem = cert.pem();
    let key_pem = kp.serialize_pem();
    let cert_path = dir.path().join("leaf-cert.pem");
    let key_path = dir.path().join("leaf-key.pem");
    std::fs::write(&cert_path, cert_pem).expect("write cert pem");
    std::fs::write(&key_path, key_pem).expect("write key pem");

    let grx_path = dir.path().join("ingress-e2e.grx");
    std::fs::write(&grx_path, INGRESS_FIXTURE).expect("write grx fixture");

    PemArtifacts {
        _tempdir: dir,
        cert: cert_path,
        key: key_path,
        ca: ca_path,
        grx_file: grx_path,
    }
}

/// Pick an ephemeral-but-stable loopback port. Spread distinct from
/// the existing `observer_e2e_subprocess.rs` test (40_000+ range) by
/// adding 200; collision risk among parallel Cargo tests is bounded.
fn pid_derived_port() -> u16 {
    let pid = std::process::id();
    40_200_u16 + u16::try_from(pid % 9_000).unwrap_or(0)
}

#[allow(clippy::zombie_processes, clippy::too_many_arguments)]
fn spawn_ingress_daemon(
    cookie: &str,
    listen_port: u16,
    observer_port: u16,
    bearer_token: &str,
    pem: &PemArtifacts,
    grx_file: &Path,
) -> (std::process::Child, SocketAddr) {
    let bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let listen = format!("127.0.0.1:{listen_port}");
    let node_id = format!("ingress-e2e@127.0.0.1:{listen_port}");
    let observer_bind = format!("127.0.0.1:{observer_port}");
    let mut child = StdCommand::new(bin.get_program())
        .args([
            "cluster",
            "daemon",
            "--node-id",
            &node_id,
            "--listen",
            &listen,
            "--cookie-env",
            "GRX_INGRESS_E2E_COOKIE",
            "--cert",
        ])
        .arg(&pem.cert)
        .args(["--key"])
        .arg(&pem.key)
        .args(["--ca"])
        .arg(&pem.ca)
        .args(["--path"])
        .arg(grx_file)
        .args([
            "--runtime",
            "cpython",
            "--duration-secs",
            "10",
            "--observer-bind",
            &observer_bind,
            "--observer-token-env",
            "GRX_INGRESS_E2E_TOKEN",
        ])
        .env("GRX_INGRESS_E2E_COOKIE", cookie)
        .env("GRX_INGRESS_E2E_TOKEN", bearer_token)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grx cluster daemon");

    let stdout = child.stdout.take().expect("daemon stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(20);
    let observer_addr: SocketAddr = loop {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let mut stderr_buf = String::new();
            if let Some(stderr) = child.stderr.take() {
                let mut r = BufReader::new(stderr);
                let _ = std::io::Read::read_to_string(&mut r, &mut stderr_buf);
            }
            panic!("daemon did not emit `grx-observer-ready` within 20s; stderr:\n{stderr_buf}");
        }
        let mut line = String::new();
        if reader.read_line(&mut line).expect("read daemon stdout") == 0 {
            let _ = child.kill();
            let mut stderr_buf = String::new();
            if let Some(stderr) = child.stderr.take() {
                let mut r = BufReader::new(stderr);
                let _ = std::io::Read::read_to_string(&mut r, &mut stderr_buf);
            }
            panic!("daemon stdout closed before ready signal; stderr:\n{stderr_buf}");
        }
        if let Some(rest) = line.trim().strip_prefix("grx-observer-ready ") {
            let addr: SocketAddr = rest.parse().expect("parse observer addr");
            child.stdout = Some(reader.into_inner());
            break addr;
        }
    };
    (child, observer_addr)
}

/// Plain HTTP/1.1 GET via std blocking sockets.
fn http_get(addr: SocketAddr, path: &str, bearer: Option<&str>) -> (u16, String) {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let mut stream = TcpStream::connect(addr).expect("connect observer");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(token) = bearer {
        let _ = write!(req, "Authorization: Bearer {token}\r\n");
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).expect("write request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, body.to_string())
}

/// Plain HTTP/1.1 POST via std blocking sockets.
fn http_post(addr: SocketAddr, path: &str, bearer: Option<&str>, body: &str) -> (u16, String) {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let mut stream = TcpStream::connect(addr).expect("connect observer");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Connection: close\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n",
        len = body.len(),
    );
    if let Some(token) = bearer {
        let _ = write!(req, "Authorization: Bearer {token}\r\n");
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).expect("write request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, body.to_string())
}

/// Poll `/v1/nodes/<id>/messages/recent` until the ring buffer
/// includes at least `expected_count` messages, OR the deadline
/// elapses. Returns the last-read snapshot list (parsed JSON).
fn poll_until_recent_count(
    observer_addr: SocketAddr,
    bearer: &str,
    node_id: &str,
    expected_count: usize,
    timeout: Duration,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let path = format!("/v1/nodes/{node_id}/messages/recent");
        let (status, body) = http_get(observer_addr, &path, Some(bearer));
        if status == 200 {
            let parsed: Vec<serde_json::Value> =
                serde_json::from_str(&body).expect("parse recent JSON");
            if parsed.len() >= expected_count {
                return parsed;
            }
        }
        assert!(
            Instant::now() < deadline,
            "ring buffer did not reach {expected_count} message(s) within {timeout:?}; \
             last body: {body}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// **Phase 4 acceptance gate (E2E ingress).** Spawn the daemon
/// with a `.grx` mounted, POST a manifest-allowed message, verify
/// it surfaces in the ring buffer; POST a manifest-rejected
/// message, verify the rejection surfaces with `accepted=false`;
/// POST to an unknown node, verify HTTP 404.
#[test]
fn ingress_subprocess_routes_message_to_node_inbox() {
    let cookie = "ingress-e2e-cookie";
    let bearer = "ingress-e2e-bearer-secret";
    let listen_port = pid_derived_port();
    let observer_port = listen_port + 1;
    let pem = write_pem_artifacts("ingress-e2e");
    let grx_file = pem.grx_file.clone();
    let (mut child, observer_addr) =
        spawn_ingress_daemon(cookie, listen_port, observer_port, bearer, &pem, &grx_file);

    // Drain stdout in the background.
    let stdout = child.stdout.take().expect("stdout");
    let drain = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = String::new();
        while reader.read_line(&mut buf).unwrap_or(0) > 0 {
            buf.clear();
        }
    });

    // ---- 1. POST a manifest-allowed message. ----
    let (status, body) = http_post(
        observer_addr,
        "/v1/nodes/receiver/messages",
        Some(bearer),
        r#"{"type":"ping","payload":{"hello":"world"}}"#,
    );
    assert_eq!(
        status, 202,
        "manifest-allowed inject must return 202 Accepted; body: {body}"
    );
    let resp: serde_json::Value = serde_json::from_str(&body).expect("parse 202 body");
    assert_eq!(resp["status"], "accepted");

    // ---- 2. Poll the ring buffer until the message surfaces. ----
    let recent =
        poll_until_recent_count(observer_addr, bearer, "receiver", 1, Duration::from_secs(5));
    let entry = recent
        .iter()
        .find(|e| e["type"] == "ping")
        .expect("ring buffer must surface the injected ping message");
    // The ring buffer entry includes type + accepted + from + ttl;
    // we don't pin every field shape (ring_buffer.rs's
    // MessageSnapshot is the authoritative type) but the
    // accepted=true assertion is the load-bearing one — confirms
    // the manifest gate passed end-to-end.
    assert_eq!(
        entry["accepted"], true,
        "manifest-gated ping must be accepted at delivery: {entry}"
    );
    assert_eq!(
        entry["from"], "__ingress__",
        "ingress-injected message must carry from = __ingress__: {entry}"
    );

    // ---- 3. POST a manifest-rejected message (type not in accepts_external). ----
    let (status_rej, _) = http_post(
        observer_addr,
        "/v1/nodes/receiver/messages",
        Some(bearer),
        r#"{"type":"forbidden","payload":{}}"#,
    );
    // Phase 1 contract: 202 Accepted regardless of manifest gate
    // (the gate runs at delivery, surfacing in MessageDelivered
    // events). The ring buffer captures the rejected delivery.
    assert_eq!(
        status_rej, 202,
        "Phase 1 single-shot returns 202 even for manifest-rejected types"
    );
    let recent_after =
        poll_until_recent_count(observer_addr, bearer, "receiver", 2, Duration::from_secs(5));
    let rejected = recent_after
        .iter()
        .find(|e| e["type"] == "forbidden")
        .expect("ring buffer must surface the forbidden message delivery attempt");
    assert_eq!(
        rejected["accepted"], false,
        "non-allowlisted message must be rejected at delivery: {rejected}"
    );

    // ---- 4. POST to an unknown node id. ----
    let (status_404, body_404) = http_post(
        observer_addr,
        "/v1/nodes/ghost/messages",
        Some(bearer),
        r#"{"type":"ping","payload":{}}"#,
    );
    assert_eq!(status_404, 404, "unknown node must surface as HTTP 404");
    let resp_404: serde_json::Value = serde_json::from_str(&body_404).expect("parse 404 body");
    assert_eq!(resp_404["error"], "node_not_found");

    // ---- 5. /v1/healthz still serves (auth-bypass), confirming the
    // daemon is healthy through the run. ----
    let (status_h, _) = http_get(observer_addr, "/v1/healthz", None);
    assert_eq!(status_h, 200);

    // Wait for daemon to exit naturally.
    let status = child.wait().expect("wait for daemon");
    assert!(status.success(), "daemon must exit cleanly");
    drain.join().expect("drain thread");
}
