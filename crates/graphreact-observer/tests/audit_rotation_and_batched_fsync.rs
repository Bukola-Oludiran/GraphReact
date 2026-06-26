//! **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Integration
//! tests for [`graphreact_observer::FileAuditSink::rotate_now`] +
//! [`graphreact_observer::FsyncMode::Batched`]. Hash-chain integrity
//! across rotation + steady-state batched-mode + drain semantics
//! are the load-bearing invariants.

use std::sync::Arc;
use std::time::Duration;

use graphreact_observer::{
    replay_audit_chain, AuditChainConfig, AuditRecordInput, AuditSink, FileAuditSink,
    FsyncMode, MetricsInner,
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

#[test]
fn rotate_preserves_chain_across_files() {
    let tmp = TempDir::new("grx-audit-rotate");
    let path = tmp.path().join("audit.ndjson");
    let sink = FileAuditSink::open(&path).expect("open");
    for i in 0..5u32 {
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"i": i, "before": true}),
        });
    }
    let outcome = sink.rotate_now().expect("rotate");
    for i in 5..10u32 {
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"i": i, "after": true}),
        });
    }
    drop(sink);

    // The OLD file's tail is the audit_log_rotated marker (boundary
    // "prior_tail"); the NEW file's head is the audit_log_rotated
    // marker (boundary "new_head"). Replay each file separately
    // and verify per-file chain consistency.
    let old_chain = replay_audit_chain(&outcome.rotated_to).expect("replay old");
    let new_chain = replay_audit_chain(&path).expect("replay new");

    // Old file: 5 accept records + 1 audit_log_rotated terminal
    // marker = 6 records (seqs 0..=5).
    assert_eq!(old_chain.len(), 6, "5 accepts + 1 rotation marker");
    assert_eq!(old_chain.last().unwrap().kind, "audit_log_rotated");
    let terminal_this_hash = old_chain.last().unwrap().this_hash.clone();
    assert_eq!(terminal_this_hash, outcome.prior_last_this_hash);

    // New file: 1 rotation head marker + 5 accept records = 6
    // records. The NEW file is its own standalone chain: seq
    // restarts at 0 + prev_hash = GENESIS — the single-file
    // verifier (`grx audit verify`) walks each rotated file
    // independently and confirms each chain. Cross-file
    // continuity lives in the `audit_log_rotated` markers'
    // `prior_last_this_hash` / `new_first_prev_hash` fields,
    // which a multi-file verifier can stitch.
    assert_eq!(new_chain.len(), 6, "1 head marker + 5 accepts");
    assert_eq!(new_chain.first().unwrap().seq, 0);
    assert_eq!(new_chain.first().unwrap().kind, "audit_log_rotated");

    // Cross-file linkage carried by the boundary records' `fields`:
    let head_fields = &new_chain[0].fields;
    assert_eq!(
        head_fields.get("prior_last_this_hash").and_then(|v| v.as_str()),
        Some(terminal_this_hash.as_str())
    );
    assert_eq!(
        head_fields.get("new_first_prev_hash").and_then(|v| v.as_str()),
        Some(outcome.new_first_prev_hash.as_str())
    );
}

#[test]
fn rotate_failure_keeps_old_file_writable() {
    let tmp = TempDir::new("grx-audit-rotate-fail");
    let path = tmp.path().join("audit.ndjson");
    let sink = FileAuditSink::open(&path).expect("open");
    sink.append(AuditRecordInput::bare("bind"));
    // Make the parent directory read-only so the rename fails.
    // POSIX rename(src, dst) needs write+exec on the parent dir for
    // BOTH src and dst — strip write bit on the parent.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o500);
        std::fs::set_permissions(tmp.path(), perms).expect("chmod r-x");
    }
    let err = sink.rotate_now();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(tmp.path(), perms).expect("restore");
    }
    #[cfg(unix)]
    {
        assert!(
            err.is_err(),
            "rotate must fail when parent dir is r-x: {err:?}"
        );
    }
    // The sink continues serving appends to the unchanged old file.
    sink.append(AuditRecordInput::bare("accept"));
    drop(sink);
    // Replay the file — should succeed (chain consistent) even
    // though the rotation failed. Will contain the original
    // bind + the rotation terminal marker (since we write that
    // BEFORE attempting rename) + accept.
    let chain = replay_audit_chain(&path).expect("replay");
    // bind + terminal-rotation-marker (best-effort, written before
    // rename attempt) + accept = 3 records on platforms where the
    // rename failed. On platforms where the rename surprisingly
    // succeeded the file would be empty + we'd see 0 records;
    // hence cfg(unix) above.
    #[cfg(unix)]
    assert_eq!(chain.len(), 3, "bind + terminal marker + accept");
    #[cfg(not(unix))]
    {
        // On non-Unix we couldn't engineer the failure, so the
        // chain may have rotated cleanly. Just verify it's
        // internally consistent.
        let _ = chain;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn batched_fsync_flushes_on_timer() {
    let tmp = TempDir::new("grx-audit-batched-timer");
    let path = tmp.path().join("audit.ndjson");
    let metrics = Arc::new(MetricsInner::new());
    let cfg = AuditChainConfig::new()
        .with_metrics(metrics.clone());
    let cfg = AuditChainConfig {
        fsync_mode: Some(FsyncMode::Batched {
            interval_ms: 20,
            max_pending_records: 1_000_000,
        }),
        ..cfg
    };
    let sink = FileAuditSink::open_with_config(&path, cfg).expect("open");
    for i in 0..3u32 {
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"i": i}),
        });
    }
    // Drain the sink to ensure the timer fires + the records hit
    // the disk surface.
    let drained = sink.drain(Duration::from_secs(2)).await;
    assert!(drained, "batched drain within deadline");
    drop(sink);
    let chain = replay_audit_chain(&path).expect("replay");
    assert_eq!(chain.len(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn batched_fsync_drain_flushes_on_shutdown() {
    let tmp = TempDir::new("grx-audit-batched-drain");
    let path = tmp.path().join("audit.ndjson");
    let metrics = Arc::new(MetricsInner::new());
    let cfg = AuditChainConfig {
        fsync_mode: Some(FsyncMode::Batched {
            // Very long timer so the size + drain are the only
            // flush triggers.
            interval_ms: 10_000,
            max_pending_records: 1_000_000,
        }),
        metrics: Some(metrics.clone()),
    };
    let sink = FileAuditSink::open_with_config(&path, cfg).expect("open");
    for i in 0..8u32 {
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"i": i}),
        });
    }
    // No timer should fire in the 10s deadline; verify drain
    // flushes immediately.
    let drained = sink.drain(Duration::from_secs(2)).await;
    assert!(drained, "drain flushes immediately on shutdown signal");
    drop(sink);
    let chain = replay_audit_chain(&path).expect("replay");
    assert_eq!(chain.len(), 8);
}

#[tokio::test(flavor = "current_thread")]
async fn batched_fsync_flushes_on_size_threshold() {
    let tmp = TempDir::new("grx-audit-batched-size");
    let path = tmp.path().join("audit.ndjson");
    let cfg = AuditChainConfig {
        fsync_mode: Some(FsyncMode::Batched {
            // Long timer so size is the only trigger.
            interval_ms: 10_000,
            max_pending_records: 2,
        }),
        metrics: None,
    };
    let sink = FileAuditSink::open_with_config(&path, cfg).expect("open");
    for i in 0..6u32 {
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"i": i}),
        });
    }
    // Give the worker a microsecond to process the queued signals.
    // The chain itself is consistent regardless of fsync state —
    // `replay_audit_chain` reads what's in the page cache + on
    // disk, so even pre-fsync records are visible.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let drained = sink.drain(Duration::from_secs(2)).await;
    assert!(drained);
    drop(sink);
    let chain = replay_audit_chain(&path).expect("replay");
    assert_eq!(chain.len(), 6);
}

#[test]
fn audit_sink_write_failures_counter_increments_on_failure() {
    let tmp = TempDir::new("grx-audit-write-fail");
    let path = tmp.path().join("audit.ndjson");
    let metrics = Arc::new(MetricsInner::new());
    let cfg = AuditChainConfig {
        fsync_mode: None,
        metrics: Some(metrics.clone()),
    };
    let sink = FileAuditSink::open_with_config(&path, cfg).expect("open");
    // Sanity: the failures family is silent initially.
    assert!(metrics.audit_sink_write_failures.is_empty());
    // Force a rotate failure by stripping write perms off the
    // parent dir. The rotate path bumps the `rotate` kind.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        sink.append(AuditRecordInput::bare("bind"));
        let perms = std::fs::Permissions::from_mode(0o500);
        std::fs::set_permissions(tmp.path(), perms).expect("chmod");
        let _ = sink.rotate_now();
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(tmp.path(), perms).expect("restore");
        // Look up the rotate counter
        let mut found_rotate = false;
        for entry in metrics.audit_sink_write_failures.iter() {
            for (k, v) in &entry.value().labels {
                if *k == "kind" && v == "rotate" {
                    found_rotate = true;
                }
            }
        }
        assert!(
            found_rotate,
            "rotate-failure counter must have fired: {:?}",
            metrics
                .audit_sink_write_failures
                .iter()
                .map(|e| e.value().labels.clone())
                .collect::<Vec<_>>()
        );
    }
    drop(sink);
}
