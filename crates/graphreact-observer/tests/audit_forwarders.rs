//! **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Integration
//! tests for the audit-log forwarder substrate.
//!
//! Load-bearing invariants:
//! - Happy-path: every appended record reaches every registered
//!   forwarder.
//! - Back-pressure: bounded queue + drop-on-full → counter +
//!   no panic.
//! - Drain bound: hub drain completes inside the deadline budget
//!   when forwarders are idle.
//! - Rotation continuity: SpoolFileForwarder rotation preserves
//!   on-disk records via atomic rename.
//! - Multi-forwarder fan-out: spool + stderr forwarders both
//!   receive every record without interference.
//! - Hash-chain visibility: the AuditRecord values seen by
//!   forwarders match the in-process file byte-for-byte (same
//!   prev_hash / this_hash chain).

use std::sync::Arc;
use std::time::Duration;

use graphreact_observer::{
    replay_audit_chain, AuditChainConfig, AuditForwarder, AuditRecord, AuditRecordInput, AuditSink,
    FileAuditSink, ForwarderHub, MetricsInner, SpoolFileForwarder,
};

/// In-tree minimal tempdir shim — no new dev-dep.
struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("{tag}-{pid}-{n}"));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Capturing forwarder for assertion: records every fanned record
/// into a `Vec<AuditRecord>` behind a `Mutex`. Reports as
/// "spool_file" so the metrics + hub.spool_forwarder() lookup
/// exercise the same code path as the real spool forwarder.
#[derive(Debug)]
struct CaptureForwarder {
    records: Arc<std::sync::Mutex<Vec<AuditRecord>>>,
    name: &'static str,
}

impl CaptureForwarder {
    fn new(name: &'static str) -> Self {
        Self {
            records: Arc::new(std::sync::Mutex::new(Vec::new())),
            name,
        }
    }
}

impl AuditForwarder for CaptureForwarder {
    fn forward(&self, record: &AuditRecord) {
        self.records.lock().unwrap().push(record.clone());
    }
    fn name(&self) -> &'static str {
        self.name
    }
}

/// Sleeps before recording — used to exercise drop-on-full
/// back-pressure semantics by saturating the bounded mpsc queue.
#[derive(Debug)]
struct SlowForwarder {
    sleep: Duration,
    forwarded: Arc<std::sync::Mutex<u64>>,
}

impl AuditForwarder for SlowForwarder {
    fn forward(&self, _record: &AuditRecord) {
        std::thread::sleep(self.sleep);
        *self.forwarded.lock().unwrap() += 1;
    }
    fn name(&self) -> &'static str {
        "spool_file"
    }
}

async fn drain_inflight() {
    // Yield enough ticks for the worker task to drain its mpsc
    // queue under tokio's `current_thread` scheduler.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn happy_path_every_record_reaches_forwarder() {
    let tmp = TempDir::new("grx-fwd-happy");
    let path = tmp.path().join("audit.ndjson");
    let metrics = Arc::new(MetricsInner::new());
    let capture = Arc::new(CaptureForwarder::new("spool_file"));
    let captured = capture.records.clone();
    let hub = Arc::new(ForwarderHub::new(
        vec![capture.clone() as Arc<dyn AuditForwarder>],
        Some(metrics.clone()),
        16,
    ));
    let cfg = AuditChainConfig::new()
        .with_metrics(metrics.clone())
        .with_forwarder_hub(hub.clone());
    let sink = FileAuditSink::open_with_config(&path, cfg).expect("open");
    for i in 0..5u32 {
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"i": i}),
        });
    }
    drain_inflight().await;
    let chain = replay_audit_chain(&path).expect("replay");
    assert_eq!(chain.len(), 5);
    let captured = captured.lock().unwrap();
    assert_eq!(
        captured.len(),
        5,
        "every appended record reaches the forwarder"
    );
    for (idx, rec) in captured.iter().enumerate() {
        assert_eq!(rec.seq, idx as u64);
        assert_eq!(rec.this_hash, chain[idx].this_hash);
        assert_eq!(rec.prev_hash, chain[idx].prev_hash);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn queue_full_drops_record_and_bumps_counter() {
    let tmp = TempDir::new("grx-fwd-queue-full");
    let path = tmp.path().join("audit.ndjson");
    let metrics = Arc::new(MetricsInner::new());
    let forwarded = Arc::new(std::sync::Mutex::new(0u64));
    let slow = Arc::new(SlowForwarder {
        sleep: Duration::from_millis(200),
        forwarded: forwarded.clone(),
    });
    let hub = Arc::new(ForwarderHub::new(
        vec![slow.clone() as Arc<dyn AuditForwarder>],
        Some(metrics.clone()),
        2, // tiny queue so we can saturate quickly
    ));
    let cfg = AuditChainConfig::new()
        .with_metrics(metrics.clone())
        .with_forwarder_hub(hub.clone());
    let sink = FileAuditSink::open_with_config(&path, cfg).expect("open");
    // Burst 50 records — the slow forwarder cannot drain a queue
    // of depth 2 between fan calls under a 200ms-per-record sleep.
    for i in 0..50u32 {
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"i": i}),
        });
    }
    // Don't wait for drain — we want to observe the drop-on-full
    // back-pressure surfaced as the counter bump while the slow
    // forwarder is still chewing.
    let dropped = sum_counter(&metrics, "spool_file", "dropped_queue_full");
    assert!(
        dropped > 0,
        "expected dropped_queue_full bumps under sustained slow forwarder; got {dropped}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn drain_is_bounded_when_forwarders_idle() {
    let tmp = TempDir::new("grx-fwd-drain");
    let path = tmp.path().join("audit.ndjson");
    let metrics = Arc::new(MetricsInner::new());
    let capture = Arc::new(CaptureForwarder::new("spool_file"));
    let hub = Arc::new(ForwarderHub::new(
        vec![capture.clone() as Arc<dyn AuditForwarder>],
        Some(metrics.clone()),
        16,
    ));
    let cfg = AuditChainConfig::new()
        .with_metrics(metrics.clone())
        .with_forwarder_hub(hub.clone());
    let sink = FileAuditSink::open_with_config(&path, cfg).expect("open");
    for i in 0..3u32 {
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"i": i}),
        });
    }
    let drained = hub.drain(Duration::from_secs(2)).await;
    assert!(
        drained,
        "drain returns within the deadline when forwarders idle"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn spool_rotation_preserves_on_disk_records() {
    let tmp = TempDir::new("grx-fwd-spool-rotate");
    let audit_path = tmp.path().join("audit.ndjson");
    let spool_path = tmp.path().join("spool.ndjson");
    let metrics = Arc::new(MetricsInner::new());
    let spool = Arc::new(SpoolFileForwarder::open(&spool_path).expect("open spool"));
    let hub = Arc::new(ForwarderHub::new(
        vec![spool.clone() as Arc<dyn AuditForwarder>],
        Some(metrics.clone()),
        64,
    ));
    let cfg = AuditChainConfig::new()
        .with_metrics(metrics.clone())
        .with_forwarder_hub(hub.clone());
    let sink = FileAuditSink::open_with_config(&audit_path, cfg).expect("open");
    for i in 0..4u32 {
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"i": i}),
        });
    }
    drain_inflight().await;
    let outcome = spool.rotate().expect("rotate").expect("outcome");
    assert!(outcome.records_in_prior_file >= 4);
    let backup_bytes = std::fs::read_to_string(&outcome.rotated_to).expect("read backup");
    assert!(
        backup_bytes.lines().count() >= 4,
        "backup spool retains records appended before rotation"
    );
    // Continue appending — new records go to the freshly-opened
    // spool file at the original path.
    sink.append(AuditRecordInput {
        kind: "accept",
        fields: serde_json::json!({"after_rotation": true}),
    });
    drain_inflight().await;
    let new_bytes = std::fs::read_to_string(&spool_path).expect("read new spool");
    assert!(
        new_bytes.lines().count() >= 1,
        "fresh spool receives post-rotation records"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn multi_forwarder_fan_out_independent() {
    let tmp = TempDir::new("grx-fwd-multi");
    let audit_path = tmp.path().join("audit.ndjson");
    let spool_path = tmp.path().join("spool.ndjson");
    let metrics = Arc::new(MetricsInner::new());
    let spool = Arc::new(SpoolFileForwarder::open(&spool_path).expect("open spool"));
    let capture = Arc::new(CaptureForwarder::new("stderr_jsonl"));
    let captured = capture.records.clone();
    let hub = Arc::new(ForwarderHub::new(
        vec![
            spool.clone() as Arc<dyn AuditForwarder>,
            capture.clone() as Arc<dyn AuditForwarder>,
        ],
        Some(metrics.clone()),
        16,
    ));
    let cfg = AuditChainConfig::new()
        .with_metrics(metrics.clone())
        .with_forwarder_hub(hub.clone());
    let sink = FileAuditSink::open_with_config(&audit_path, cfg).expect("open");
    for i in 0..3u32 {
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"i": i}),
        });
    }
    drain_inflight().await;
    // Spool received via on-disk file.
    let spool_bytes = std::fs::read_to_string(&spool_path).expect("read spool");
    assert_eq!(spool_bytes.lines().count(), 3);
    // Capture received via in-memory vec.
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 3);
    // Spool forwarder accessible via hub.spool_forwarder().
    assert!(hub.spool_forwarder().is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn rotation_boundary_markers_fan_to_forwarders() {
    let tmp = TempDir::new("grx-fwd-boundary");
    let audit_path = tmp.path().join("audit.ndjson");
    let metrics = Arc::new(MetricsInner::new());
    let capture = Arc::new(CaptureForwarder::new("spool_file"));
    let captured = capture.records.clone();
    let hub = Arc::new(ForwarderHub::new(
        vec![capture.clone() as Arc<dyn AuditForwarder>],
        Some(metrics.clone()),
        32,
    ));
    let cfg = AuditChainConfig::new()
        .with_metrics(metrics.clone())
        .with_forwarder_hub(hub.clone());
    let sink = FileAuditSink::open_with_config(&audit_path, cfg).expect("open");
    sink.append(AuditRecordInput {
        kind: "accept",
        fields: serde_json::json!({"i": 0}),
    });
    drain_inflight().await;
    let outcome = sink.rotate().expect("rotate").expect("outcome");
    drain_inflight().await;
    sink.append(AuditRecordInput {
        kind: "accept",
        fields: serde_json::json!({"after": true}),
    });
    drain_inflight().await;
    let captured = captured.lock().unwrap();
    let boundary_kinds: Vec<&str> = captured
        .iter()
        .filter(|r| r.kind == "audit_log_rotated")
        .map(|r| r.kind.as_str())
        .collect();
    assert!(
        boundary_kinds.len() >= 2,
        "expected both audit_log_rotated boundary markers fanned to forwarder, got {}",
        boundary_kinds.len()
    );
    // The rotation outcome's prior_last_this_hash must equal one of
    // the boundary markers' this_hash — verifies the cross-file
    // chain visibility the substrate spec calls out.
    let prior_hash_seen = captured
        .iter()
        .any(|r| r.this_hash == outcome.prior_last_this_hash);
    assert!(
        prior_hash_seen,
        "forwarders should observe the prior_last_this_hash boundary marker"
    );
}

/// Sum the counter value across every key matching the
/// (forwarder, outcome) tuple. Defensive — the DashMap surface
/// doesn't let us read a single labeled counter by name, so we
/// iterate.
fn sum_counter(metrics: &Arc<MetricsInner>, forwarder: &str, outcome: &str) -> u64 {
    let mut total = 0u64;
    for entry in &metrics.audit_forwarder_records {
        let lc = entry.value();
        let mut matches = false;
        let mut hit_fwd = false;
        let mut hit_out = false;
        for (k, v) in &lc.labels {
            if *k == "forwarder" && v == forwarder {
                hit_fwd = true;
            }
            if *k == "outcome" && v == outcome {
                hit_out = true;
            }
        }
        if hit_fwd && hit_out {
            matches = true;
        }
        if matches {
            total += lc.counter.get();
        }
    }
    total
}
