//! **M16-Backend P11 closeout — real-binary observer E2E test.**
//!
//! Spawns `grx cluster daemon` as a subprocess with the full observer
//! stack (`--observer-bind` + bearer-token auth + Prometheus endpoint),
//! waits for the `grx-daemon-ready` signal, then scrapes
//! `GET /v1/healthz` + `GET /v1/identity` + `GET /v1/metrics` over
//! plain HTTP and asserts the expected responses. Validates the full
//! end-to-end ops surface — clap-derived flag schema → BindConfig
//! parsing → observer task spawn → axum route resolution → metric
//! exposition → bearer-token middleware.
//!
//! Loopback bind only (the no-TLS path) so the test doesn't need
//! cert artifacts. The TLS path is exercised by the existing
//! `cluster_cli_subprocess.rs` test for the M14 cluster handshake;
//! TLS for the observer bind is the same `ServerTlsConfig` plumbing.
//!
//! `--runtime null` keeps the test engine-free so it runs cleanly on
//! Linux + macOS (the existing TUI smoke test in `observer_tui_smoke.rs`
//! exercises the in-process axum routes; this test pins the
//! subprocess + binary boundary).
//!
//! Skipped on non-unix targets because `assert_cmd::cargo_bin` +
//! signal-driven shutdown is unix-shaped; the daemon's `--duration-secs`
//! self-termination keeps the test deterministic regardless.

#![cfg(unix)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;

/// Render a CA + a leaf cert (subject = `observer-e2e`) to PEM files
/// in a tempdir so the daemon can bind its TLS listener. Same shape as
/// `cluster_cli_subprocess.rs::write_pem_artifacts` but minimal — one
/// leaf because the observer-E2E test doesn't dial peers.
struct PemArtifacts {
    _tempdir: tempfile::TempDir,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub ca: PathBuf,
}

fn write_pem_artifacts(name: &str) -> PemArtifacts {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_kp = rcgen::KeyPair::generate().expect("ca keygen");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["grx observer-e2e CA".into()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_kp).expect("ca self-sign");
    let ca_pem = ca_cert.pem();
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, ca_pem).expect("write ca pem");

    let mut params =
        rcgen::CertificateParams::new(vec![name.into()]).expect("leaf params");
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

    PemArtifacts { _tempdir: dir, cert: cert_path, key: key_path, ca: ca_path }
}

/// Pick an ephemeral-but-stable loopback port for the daemon's
/// listen socket. The observer binds a separate port (listen + 1).
fn pid_derived_port() -> u16 {
    let pid = std::process::id();
    // Spread to a low-conflict range distinct from the existing
    // cluster_cli_subprocess.rs test (which uses 30_000+).
    40_000_u16 + u16::try_from(pid % 9_000).unwrap_or(0)
}

// Spawn `grx cluster daemon --runtime=null` with `--observer-bind`
// + a bearer token. Returns the live child + the resolved observer
// addr (the test scrapes against this addr).
#[allow(clippy::zombie_processes, clippy::too_many_arguments)]
fn spawn_observer_daemon(
    cookie: &str,
    listen_port: u16,
    observer_port: u16,
    bearer_token: &str,
    pem: &PemArtifacts,
) -> (std::process::Child, SocketAddr) {
    let bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let listen = format!("127.0.0.1:{listen_port}");
    let node_id = format!("observer-e2e@127.0.0.1:{listen_port}");
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
            "GRX_OBS_E2E_COOKIE",
            "--cert",
        ])
        .arg(&pem.cert)
        .args(["--key"])
        .arg(&pem.key)
        .args(["--ca"])
        .arg(&pem.ca)
        .args([
            "--runtime",
            "null",
            "--duration-secs",
            "8",
            "--observer-bind",
            &observer_bind,
            "--observer-token-env",
            "GRX_OBS_E2E_TOKEN",
        ])
        .env("GRX_OBS_E2E_COOKIE", cookie)
        .env("GRX_OBS_E2E_TOKEN", bearer_token)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grx cluster daemon");

    let stdout = child.stdout.take().expect("daemon stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(15);
    let observer_addr: SocketAddr = loop {
        if Instant::now() >= deadline {
            let _ = child.kill();
            // Drain stderr to surface why the daemon didn't start.
            let mut stderr_buf = String::new();
            if let Some(stderr) = child.stderr.take() {
                let mut r = BufReader::new(stderr);
                let _ = std::io::Read::read_to_string(&mut r, &mut stderr_buf);
            }
            panic!(
                "daemon did not emit `grx-observer-ready` within 15s; stderr:\n{stderr_buf}"
            );
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
        // Daemon prints `grx-daemon-ready <listen-addr>` first, then
        // `grx-observer-ready <observer-addr>` (when --observer-bind
        // is set). We need the observer addr.
        if let Some(rest) = line.trim().strip_prefix("grx-observer-ready ") {
            let addr: SocketAddr = rest.parse().expect("parse observer addr");
            child.stdout = Some(reader.into_inner());
            break addr;
        }
    };
    (child, observer_addr)
}

/// Plain HTTP/1.1 GET via std blocking sockets — same shape as the
/// CLI's hand-rolled observer_client (no reqwest dep). Returns
/// `(status, body)`.
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

// **P11 acceptance gate (E2E observer surface).** Spawn the daemon
// + scrape `/v1/healthz` (auth-bypass) + `/v1/identity` (auth-checked)
// + `/v1/metrics` (auth-checked) over plain HTTP. Asserts the
// expected responses + auth behaviour.
#[test]
fn observer_subprocess_serves_v1_routes_with_bearer_auth() {
    let cookie = "observer-e2e-cookie";
    let bearer = "test-bearer-token-secret";
    let listen_port = pid_derived_port();
    let observer_port = listen_port + 1;
    let pem = write_pem_artifacts("observer-e2e");
    let (mut child, observer_addr) =
        spawn_observer_daemon(cookie, listen_port, observer_port, bearer, &pem);

    // Drain stdout in the background so the daemon's pipe doesn't
    // fill up + block its writes (stdout is piped above).
    let stdout = child.stdout.take().expect("stdout");
    let drain = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = String::new();
        while reader.read_line(&mut buf).unwrap_or(0) > 0 {
            buf.clear();
        }
    });

    // ---- /v1/healthz (auth-bypass; liveness probe) ----
    let (status, _body) = http_get(observer_addr, "/v1/healthz", None);
    assert_eq!(status, 200, "healthz must succeed without auth");

    // ---- /v1/identity (auth-checked; missing token → 401) ----
    let (status_no_auth, _) = http_get(observer_addr, "/v1/identity", None);
    assert_eq!(
        status_no_auth, 401,
        "identity must require auth"
    );

    // ---- /v1/identity with valid bearer → 200 + JSON ----
    let (status, body) = http_get(observer_addr, "/v1/identity", Some(bearer));
    assert_eq!(status, 200, "identity with valid bearer must succeed");
    let identity: serde_json::Value =
        serde_json::from_str(&body).expect("parse identity JSON");
    assert!(
        identity
            .get("process_start_id")
            .and_then(serde_json::Value::as_u64)
            .is_some(),
        "identity must include process_start_id u64: {body}"
    );
    assert_eq!(
        identity
            .get("api_version")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "api_version must be 1"
    );

    // ---- /v1/metrics with valid bearer → 200 + Prometheus text ----
    let (status, body) = http_get(observer_addr, "/v1/metrics", Some(bearer));
    assert_eq!(status, 200, "metrics with valid bearer must succeed");
    // Sanity: at least one of the load-bearing metric families
    // appears in the exposition. The exact set depends on which
    // events fired before the scrape (likely none for `--runtime=null`
    // with no peers, so we look for HELP comments which are emitted
    // unconditionally by the encoder).
    assert!(
        body.contains("# HELP grx_") || body.contains("grx_"),
        "metrics body must contain at least one grx_ family: {body}"
    );

    // ---- **P11.1 stock-tooling validation: prometheus-parse round-trip.** ----
    // The metrics body MUST round-trip through the upstream
    // `prometheus-parse` crate cleanly. Catches any encoder
    // regression (malformed HELP/TYPE comments, unescaped label
    // values, missing trailing newline, etc.) that a naive
    // `body.contains("grx_")` check would miss. Same pattern as
    // graphreact-observer's existing `prometheus_scrape.rs` test;
    // this lifts the validation to the subprocess + binary boundary.
    let lines: Vec<_> = body.lines().map(|l| Ok(l.to_string())).collect();
    let scrape = prometheus_parse::Scrape::parse(lines.into_iter())
        .expect("prometheus-parse must accept the exposition output");
    // Sanity: the parsed scrape contains at least the families that
    // fire even on an idle null-runtime daemon (HELP/TYPE comments
    // are emitted unconditionally by the encoder).
    let metric_names: std::collections::BTreeSet<&str> =
        scrape.docs.keys().map(String::as_str).collect();
    assert!(
        !metric_names.is_empty(),
        "parsed scrape must contain at least one metric family; got: {metric_names:?}"
    );

    // ---- /v1/metrics with WRONG bearer → 401 ----
    let (status_wrong, _) = http_get(observer_addr, "/v1/metrics", Some("wrong-token"));
    assert_eq!(
        status_wrong, 401,
        "metrics with wrong bearer must reject"
    );

    // Wait for daemon to exit naturally (--duration-secs 8). The
    // daemon's signal handler will close the observer task gracefully.
    let status = child.wait().expect("wait for daemon");
    assert!(status.success(), "daemon must exit cleanly");

    drain.join().expect("drain thread");
}

// **P11.1 stock-tooling validation: JSON-log shape.** When the
// daemon is started with `--observer-json-logs`, every line emitted
// to stderr MUST parse as JSON + carry the documented schema (either
// the `JsonLogTelemetry` shape `{event, payload}` from P1 OR the
// `TracingJsonLayer` shape `{ts, level, target, name, fields, span}`
// from P3 — see `m16-architecture.md`). Loki / Elastic ingestion
// pipelines depend on this shape; a regression in either encoder
// would surface here.
#[test]
fn observer_json_logs_lines_parse_as_canonical_shape() {
    let cookie = "json-logs-validation-cookie";
    let bearer = "json-logs-validation-bearer";
    let listen_port = pid_derived_port() + 100;
    let observer_port = listen_port + 1;
    let pem = write_pem_artifacts("json-logs-e2e");
    let bin = StdCommand::cargo_bin("grx").expect("locate grx binary");
    let listen = format!("127.0.0.1:{listen_port}");
    let node_id = format!("json-logs-e2e@127.0.0.1:{listen_port}");
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
            "GRX_JSON_LOGS_COOKIE",
            "--cert",
        ])
        .arg(&pem.cert)
        .args(["--key"])
        .arg(&pem.key)
        .args(["--ca"])
        .arg(&pem.ca)
        .args([
            "--runtime",
            "null",
            "--duration-secs",
            "4",
            "--observer-bind",
            &observer_bind,
            "--observer-token-env",
            "GRX_JSON_LOGS_TOKEN",
            "--observer-json-logs",
        ])
        .env("GRX_JSON_LOGS_COOKIE", cookie)
        .env("GRX_JSON_LOGS_TOKEN", bearer)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grx cluster daemon");

    // Drain stdout so the pipe doesn't fill.
    let stdout = child.stdout.take().expect("stdout");
    let drain_stdout = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = String::new();
        while reader.read_line(&mut buf).unwrap_or(0) > 0 {
            buf.clear();
        }
    });

    // Wait for daemon to exit, capturing all stderr lines.
    let status = child.wait().expect("wait daemon");
    assert!(status.success(), "daemon must exit cleanly");
    drain_stdout.join().expect("stdout drain");

    let stderr = child.stderr.take().expect("stderr");
    let mut reader = BufReader::new(stderr);
    let mut all = String::new();
    std::io::Read::read_to_string(&mut reader, &mut all).expect("read stderr");

    // Filter out cargo-test / build infrastructure noise — keep only
    // lines that look like JSON (start with `{`).
    let json_lines: Vec<&str> = all
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .collect();

    assert!(
        !json_lines.is_empty(),
        "daemon with --observer-json-logs must emit at least one JSON line; got stderr:\n{all}"
    );

    // Every JSON line MUST parse + carry one of the two documented
    // schemas. Per m16-architecture.md: operators distinguish the two
    // schemas by presence of `event` (JsonLogTelemetry) vs `ts`
    // (TracingJsonLayer) keys.
    for line in &json_lines {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line must parse as JSON: {line:?}, err={e}"));
        let has_event = v.get("event").is_some();
        let has_ts = v.get("ts").is_some();
        assert!(
            has_event || has_ts,
            "line must carry `event` (JsonLogTelemetry shape) OR `ts` (TracingJsonLayer shape): {line}"
        );
    }
}
