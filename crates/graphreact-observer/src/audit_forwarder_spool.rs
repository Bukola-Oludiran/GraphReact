//! **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Reference
//! [`AuditForwarder`] writing `AuditRecord` values as NDJSON to a
//! separate spool file with mode `0o600` enforcement on Unix.
//!
//! Operators tail the spool with a sidecar (fluentd / vector /
//! promtail / opentelemetry-collector / fluent-bit /
//! aws-for-fluent-bit / google-cloud-ops-agent / etc.); the sidecar
//! handles back-pressure to S3 / Loki / GCS / OpenSearch / Splunk /
//! Datadog without the daemon ever needing an outbound HTTP path.
//!
//! ## Mode 0o600
//!
//! The spool file mirrors the in-process audit log: peer IPs, auth
//! reasons, TLS leaf fingerprints, full hash-chained record bytes.
//! Mode 0o600 enforced on create + on a pre-existing file + after
//! rotation so a daemon restart can't silently leave a world-
//! readable spool behind.
//!
//! ## Rotation
//!
//! SIGHUP-driven via [`crate::ForwarderHub::spool_forwarder`] +
//! [`crate::audit_forwarders::AuditForwarder::rotate`]. Atomic
//! rename (`<spool>.YYYY-MM-DDTHH-MM-SS-Z`) + fresh-file reopen
//! with 0o600 enforcement; mirrors
//! [`crate::FileAuditSink::rotate_now`]'s shape. The boundary
//! `audit_log_rotated` markers stamped by the in-process audit
//! log's rotation are forwarded to the spool through the normal
//! fan-out path so external verifiers see the same chain markers
//! the in-process file does — the spool's OWN rotation does NOT
//! re-stamp boundary markers (operators verify the chain via the
//! markers that ride in via the in-process rotation fan).

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::audit_chain::{AuditChainError, AuditRecord};
use crate::audit_forwarders::{AuditForwarder, ForwarderRotationOutcome};

/// Static name for [`AuditForwarder::name`] reporting +
/// `audit_forwarder_records_total{forwarder="spool_file"}`.
pub const SPOOL_FORWARDER_NAME: &str = "spool_file";

/// File-backed NDJSON spool forwarder. Each [`Self::forward`] call
/// appends one JSON line + `\n` + flush; sustained throughput is
/// page-cache-bound (the worker task runs OFF the audit-sink writer
/// lock so a slow spool disk never back-pressures
/// [`crate::FileAuditSink::append`]).
#[derive(Debug)]
pub struct SpoolFileForwarder {
    inner: Mutex<SpoolInner>,
}

/// Locked spool state. Held behind `Mutex` so [`Self::forward`]
/// (on the per-forwarder worker task) and [`Self::rotate`] (on the
/// SIGHUP fan-out arm) don't tear the BufWriter mid-write.
#[derive(Debug)]
struct SpoolInner {
    path: PathBuf,
    writer: BufWriter<File>,
    records_in_current_file: u64,
}

impl SpoolFileForwarder {
    /// Open `path` in append mode, enforcing mode `0o600` on Unix.
    /// Equivalent to [`crate::FileAuditSink::open`] on the chain-
    /// state side: a pre-existing file is opened in append mode
    /// (existing contents preserved) and the post-open chmod
    /// tightens permissions even if the file was previously
    /// world-readable.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AuditChainError> {
        let path = path.into();
        let file = open_spool_file(&path)?;
        Ok(Self {
            inner: Mutex::new(SpoolInner {
                path,
                writer: BufWriter::new(file),
                records_in_current_file: 0,
            }),
        })
    }

    /// Inspect-only accessor for tests + diagnostics.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.inner
            .lock()
            .map(|g| g.path.clone())
            .unwrap_or_default()
    }
}

impl AuditForwarder for SpoolFileForwarder {
    fn name(&self) -> &'static str {
        SPOOL_FORWARDER_NAME
    }

    fn forward(&self, record: &AuditRecord) {
        let mut state = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let line = match serde_json::to_vec(record) {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_sink_write_failed",
                    error_kind = "spool_serialize",
                    seq = record.seq,
                    kind = %record.kind,
                    error = %err,
                    "spool forwarder failed to serialize record; dropping",
                );
                return;
            }
        };
        if let Err(err) = state.writer.write_all(&line) {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                error_kind = "spool_write",
                seq = record.seq,
                kind = %record.kind,
                error = %err,
                "spool forwarder write_all failed; tracing-side stream continues",
            );
            return;
        }
        if let Err(err) = state.writer.write_all(b"\n") {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                error_kind = "spool_write",
                seq = record.seq,
                kind = %record.kind,
                error = %err,
                "spool forwarder newline write failed",
            );
            return;
        }
        if let Err(err) = state.writer.flush() {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                error_kind = "spool_flush",
                seq = record.seq,
                kind = %record.kind,
                error = %err,
                "spool forwarder flush failed",
            );
            return;
        }
        state.records_in_current_file = state.records_in_current_file.saturating_add(1);
    }

    fn drain<'a>(
        &'a self,
        _deadline: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            // Final flush + fsync so the spool is durable on disk
            // before the daemon exits. The on-the-worker writes are
            // already flushed to the page cache via `BufWriter::flush`;
            // a single sync_data here graduates them to disk.
            let state = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Err(err) = state.writer.get_ref().sync_data() {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_sink_write_failed",
                    error_kind = "spool_drain",
                    error = %err,
                    "spool forwarder drain sync_data failed",
                );
                return false;
            }
            true
        })
    }

    fn rotate(&self) -> Result<Option<ForwarderRotationOutcome>, AuditChainError> {
        let mut state = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let prior_path = state.path.clone();
        let timestamp = rotation_timestamp();
        let mut backup_path = prior_path.clone().into_os_string();
        backup_path.push(format!(".{timestamp}"));
        let backup_path = PathBuf::from(backup_path);
        let records_in_prior_file = state.records_in_current_file;

        // Force any buffered bytes + the file metadata to disk so
        // an OS-level crash mid-rotation doesn't lose the spool's
        // tail.
        if let Err(err) = state.writer.flush() {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                error_kind = "spool_rotate_flush",
                error = %err,
                path = %prior_path.display(),
                "spool forwarder pre-rotation flush failed; continuing with rename",
            );
        }
        if let Err(err) = state.writer.get_ref().sync_data() {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                error_kind = "spool_rotate_fsync",
                error = %err,
                path = %prior_path.display(),
                "spool forwarder pre-rotation fsync failed; continuing with rename",
            );
        }
        if let Err(err) = std::fs::rename(&prior_path, &backup_path) {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                error_kind = "spool_rotate_rename",
                from = %prior_path.display(),
                to = %backup_path.display(),
                error = %err,
                "spool forwarder rotation rename failed; continuing on the old file",
            );
            return Err(AuditChainError::Io {
                path: prior_path,
                source: err,
            });
        }
        let fresh = open_spool_file(&prior_path)?;
        state.writer = BufWriter::new(fresh);
        state.records_in_current_file = 0;
        Ok(Some(ForwarderRotationOutcome {
            rotated_to: backup_path,
            records_in_prior_file,
        }))
    }
}

/// Mode 0o600 enforced spool file open. Mirrors
/// [`crate::audit_chain::open_audit_file`]'s shape bit-for-bit so
/// operators get the same security invariant on the spool as on
/// the in-process audit log.
fn open_spool_file(path: &Path) -> Result<File, AuditChainError> {
    let mut open_opts = OpenOptions::new();
    open_opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o600);
    }
    let file = open_opts.open(path).map_err(|err| AuditChainError::Io {
        path: path.to_path_buf(),
        source: err,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms).map_err(|err| AuditChainError::Io {
            path: path.to_path_buf(),
            source: err,
        })?;
    }
    Ok(file)
}

/// Compose a rotation-suffix timestamp (`YYYY-MM-DDTHH-MM-SS-Z`).
/// Same hand-rolled civil-time conversion as
/// [`crate::audit_chain`] so operators see consistent suffixes
/// across in-process audit log + spool rotations.
fn rotation_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, min, sec) = unix_secs_to_utc_components(now);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{min:02}-{sec:02}Z")
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn unix_secs_to_utc_components(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let sec = (secs % 60) as u32;
    let mins_total = secs / 60;
    let min = (mins_total % 60) as u32;
    let hours_total = mins_total / 60;
    let hour = (hours_total % 24) as u32;
    let mut days = (hours_total / 24) as i64;
    days += 719_468;
    let era = if days >= 0 {
        days / 146_097
    } else {
        (days - 146_096) / 146_097
    };
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if m <= 2 { (y + 1) as u32 } else { y as u32 };
    (year, m, d, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_chain::AuditRecord;

    struct TempDir {
        path: PathBuf,
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
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn rec(seq: u64) -> AuditRecord {
        AuditRecord {
            seq,
            ts_ms: 1_700_000_000_000 + u128::from(seq) * 1000,
            prev_hash: "0".repeat(64),
            kind: "bind".to_string(),
            fields: serde_json::json!({"i": seq}),
            this_hash: "f".repeat(64),
        }
    }

    #[test]
    fn spool_appends_ndjson() {
        let tmp = TempDir::new("grx-spool");
        let path = tmp.path.join("spool.ndjson");
        let f = SpoolFileForwarder::open(&path).expect("open");
        f.forward(&rec(0));
        f.forward(&rec(1));
        let bytes = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<_> = bytes.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: AuditRecord = serde_json::from_str(lines[0]).expect("parse");
        assert_eq!(first.seq, 0);
    }

    #[cfg(unix)]
    #[test]
    fn spool_enforces_0600_on_create() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("grx-spool-mode");
        let path = tmp.path.join("spool.ndjson");
        let _f = SpoolFileForwarder::open(&path).expect("open");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn rotate_renames_and_reopens() {
        let tmp = TempDir::new("grx-spool-rotate");
        let path = tmp.path.join("spool.ndjson");
        let f = SpoolFileForwarder::open(&path).expect("open");
        f.forward(&rec(0));
        f.forward(&rec(1));
        let outcome = f.rotate().expect("rotate").expect("outcome");
        assert_eq!(outcome.records_in_prior_file, 2);
        assert!(outcome.rotated_to != path);
        assert!(outcome.rotated_to.exists(), "backup file exists");
        // New file ready for further writes.
        f.forward(&rec(2));
        let new_bytes = std::fs::read_to_string(&path).expect("read new");
        assert_eq!(new_bytes.lines().count(), 1);
        let old_bytes = std::fs::read_to_string(&outcome.rotated_to).expect("read old");
        assert_eq!(old_bytes.lines().count(), 2);
    }
}
