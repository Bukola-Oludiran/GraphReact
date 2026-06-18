//! **M-Prod sub-track #1 Phase A.** End-to-end test for the
//! `grx audit verify <PATH>` subcommand. Constructs a chained
//! NDJSON file via the public
//! [`graphreact_observer::FileAuditSink`] API, then drives the
//! built `grx` binary against it and asserts:
//!
//! - exit 0 + stdout `audit chain OK: <N> records` for an
//!   untouched chain.
//! - exit 1 + stderr `tamper detected at seq=<K>: …` after one
//!   byte is flipped mid-file.

use std::sync::atomic::{AtomicU64, Ordering};

use assert_cmd::Command;
use graphreact_observer::{AuditRecordInput, AuditSink, FileAuditSink};

struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("{tag}-{pid}-{n}"));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn audit_verify_succeeds_on_clean_chain() {
    let dir = TempDir::new("grx-cli-audit-ok");
    let path = dir.path.join("audit.ndjson");
    {
        let sink = FileAuditSink::open(&path).expect("open");
        sink.append(AuditRecordInput {
            kind: "bind",
            fields: serde_json::json!({"addr": "127.0.0.1:9090", "tls": false, "auth": true}),
        });
        sink.append(AuditRecordInput {
            kind: "auth_fail",
            fields: serde_json::json!({"peer": "10.0.0.1:1234", "reason": "wrong"}),
        });
        sink.append(AuditRecordInput {
            kind: "tls_reload_success",
            fields: serde_json::json!({
                "leaf_fingerprint_sha256": "deadbeef".repeat(8),
            }),
        });
        // Sink drops here, closing the BufWriter.
    }

    let mut cmd = Command::cargo_bin("grx").expect("binary present");
    let assertion = cmd
        .arg("audit")
        .arg("verify")
        .arg(&path)
        .assert()
        .success();
    let out = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();
    assert!(
        out.contains("audit chain OK: 3 records"),
        "stdout was: {out:?}"
    );
}

#[test]
fn audit_verify_fails_on_tampered_chain() {
    let dir = TempDir::new("grx-cli-audit-tamper");
    let path = dir.path.join("audit.ndjson");
    {
        let sink = FileAuditSink::open(&path).expect("open");
        sink.append(AuditRecordInput {
            kind: "bind",
            fields: serde_json::json!({"addr": "127.0.0.1:9090"}),
        });
        sink.append(AuditRecordInput {
            kind: "auth_fail",
            fields: serde_json::json!({"peer": "10.0.0.5:5555", "reason": "wrong"}),
        });
        sink.append(AuditRecordInput {
            kind: "response",
            fields: serde_json::json!({
                "peer": "10.0.0.5:5555",
                "path": "/v1/metrics",
                "status": 200,
                "bytes": 4096
            }),
        });
    }
    // Mutate one byte on the middle record. The bytes that survive
    // no longer match the recorded this_hash.
    let raw = std::fs::read_to_string(&path).expect("read");
    let mutated = raw.replacen("10.0.0.5", "10.0.0.9", 1);
    assert_ne!(raw, mutated, "test sentinel must have rewritten a byte");
    std::fs::write(&path, mutated).expect("write");

    let mut cmd = Command::cargo_bin("grx").expect("binary present");
    let assertion = cmd
        .arg("audit")
        .arg("verify")
        .arg(&path)
        .assert()
        .failure();
    let err = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        err.contains("tamper detected at seq=1"),
        "stderr was: {err:?}"
    );
}

#[test]
fn audit_verify_fails_on_missing_file() {
    let dir = TempDir::new("grx-cli-audit-missing");
    let path = dir.path.join("never-written.ndjson");
    let mut cmd = Command::cargo_bin("grx").expect("binary present");
    let assertion = cmd
        .arg("audit")
        .arg("verify")
        .arg(&path)
        .assert()
        .failure();
    let err = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        err.contains("audit chain I/O"),
        "stderr was: {err:?}"
    );
}
