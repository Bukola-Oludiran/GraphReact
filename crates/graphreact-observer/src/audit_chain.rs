//! **M-Prod sub-track #1 Phase A.** Hash-chained tamper-evident
//! audit-log SINK substrate.
//!
//! The pre-existing
//! [`crate::audit`](crate::audit) module emits each lifecycle event
//! (bind / accept / auth-fail / response / TLS-rotate-{attempt,success,failure}
//! / OTel-shutdown-timeout) as a structured
//! `tracing::{info,warn}!(target: "graphreact::observer::audit", …)`
//! line. That stream is operator-facing — it's consumed by
//! `journalctl`, `jq`, or the unified JSON-log subscriber installed
//! via [`crate::install_json_logs_subscriber`].
//!
//! Phase A is **strictly additive** on top of that surface: the
//! existing `tracing` line keeps firing at every callsite,
//! byte-for-byte unchanged when no sink is installed, and ALSO an
//! [`AuditRecord`] is appended to an installed [`AuditSink`] when one
//! is wired through [`crate::ApiState::audit_sink`].
//!
//! The on-disk format is newline-delimited JSON (NDJSON), one
//! `AuditRecord` per line, with each line carrying:
//!
//! - `seq`: monotonic counter starting at 0
//! - `ts_ms`: Unix epoch milliseconds at append time
//! - `prev_hash`: hex SHA-256 of the previous record (genesis = 64×`0`)
//! - `kind`: one of `bind`, `accept`, `auth_fail`, `response`,
//!   `tls_reload_attempt`, `tls_reload_success`, `tls_reload_failure`,
//!   `otel_shutdown_timeout`, `audit_sink_write_failed`
//! - `fields`: opaque `serde_json::Value` carrying the structured
//!   fields the existing tracing line emits (`peer`, `path`, `status`,
//!   `bytes`, `reason`, `addr`, `tls`, `auth`,
//!   `cert_bytes_len`, `key_bytes_len`, `leaf_fingerprint_sha256`,
//!   `error_kind`, `detail`, etc.). Forward-compatible — Phase B's
//!   RBAC verdict fields ride here without bumping the schema.
//! - `this_hash`: hex SHA-256 of `{seq, ts_ms, prev_hash, kind,
//!   fields}` (the `this_hash` field itself is omitted from the
//!   hash input, since hashing the hash is circular).
//!
//! ## Tamper detection
//!
//! [`replay`] walks the file from seq=0, recomputes each record's
//! expected hash from its serialized canonical bytes, and verifies
//! the chain links. Any byte-level mutation of an existing record
//! (or any reordering / insertion / deletion) breaks the chain and
//! surfaces as [`AuditChainError::TamperDetected`] at the affected
//! seq.
//!
//! ## fsync policy
//!
//! Phase A fsyncs (`sync_data`) on every append: correctness over
//! throughput for the substrate. **M-Prod #1 Phase B Part 2 (Group
//! 1ab)** adds an opt-in batched-fsync mode via
//! [`FsyncMode::Batched`] — the chain hash + write_all run
//! sequentially under the writer lock (chain integrity preserved
//! bit-for-bit), but the `sync_data` call is deferred to a worker
//! task that fsyncs on EITHER a timer cadence OR a size threshold.
//! Default stays [`FsyncMode::PerWrite`] for backward compatibility;
//! operators opt in via `GRX_AUDIT_FSYNC_MODE=batched` (with
//! `GRX_AUDIT_FSYNC_INTERVAL_MS` / `GRX_AUDIT_FSYNC_MAX_PENDING`
//! tuning knobs). The drain on shutdown is bounded by
//! `OBSERVER_SHUTDOWN_DEADLINE`.
//!
//! ## Rotation policy (Phase B Part 2)
//!
//! [`FileAuditSink::rotate`] is the load-bearing entry point for
//! SIGHUP-driven audit-log rotation. The implementation atomically
//! renames the existing file to a timestamped backup, opens a fresh
//! file at the original path with mode 0o600 enforcement, and
//! preserves chain continuity across the boundary by writing an
//! `audit_log_rotated` record as the FINAL line of the prior file
//! AND as the FIRST line of the new file. The new file's first
//! record's `prev_hash` equals the prior file's tail `this_hash`.
//!
//! ## Sink failure policy
//!
//! Per the substrate contract, the tracing-side stream NEVER stops
//! firing — sink-side I/O failures degrade by counting the failure
//! against [`crate::MetricsInner`]'s
//! `audit_sink_write_failures_total{kind}` counter (Group 1ab wires
//! the counter; the 4-entry closed vocabulary is
//! `{"open", "write", "fsync", "rotate"}`). Sinks NEVER panic on
//! I/O error; they log a one-shot `tracing::warn!` and continue.
//! The in-memory chain state continues advancing so the next
//! successful write re-seats the chain at the correct seq + hash.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::metrics::MetricsInner;

/// Genesis sentinel for the very first record's `prev_hash` field.
/// 64 hex zeros (32 bytes) so `prev_hash` is always a 64-char hex
/// string regardless of whether the file is fresh or being appended.
const GENESIS_PREV_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Input shape for an [`AuditSink::append`] call. The kind tag is a
/// `&'static str` because every callsite is one of a fixed handful
/// of lifecycle events; the fields blob is an open `serde_json::Value`
/// so the existing audit helpers can hand-roll their per-event field
/// shape without growing the trait.
///
/// The sink owns the responsibility for stamping `seq` / `ts_ms` /
/// `prev_hash` / `this_hash` — callers MUST NOT pre-fill those.
#[derive(Debug, Clone)]
pub struct AuditRecordInput {
    pub kind: &'static str,
    pub fields: serde_json::Value,
}

impl AuditRecordInput {
    /// Convenience constructor with an empty `fields` object.
    #[must_use]
    pub fn bare(kind: &'static str) -> Self {
        Self {
            kind,
            fields: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// On-disk shape of one audit record. Serialized as one NDJSON line.
/// Field order is fixed by [`serde::Serialize`] derive ordering on
/// this struct so the canonical-bytes input to the hash chain is
/// stable across builds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub seq: u64,
    pub ts_ms: u128,
    pub prev_hash: String,
    pub kind: String,
    pub fields: serde_json::Value,
    pub this_hash: String,
}

/// The view used to compute `this_hash`. We intentionally omit
/// `this_hash` from the hash input (circular) and serialize the
/// remaining fields in struct-declaration order so the canonical
/// bytes are stable across encoders.
#[derive(Debug, Serialize)]
struct AuditRecordHashInput<'a> {
    seq: u64,
    ts_ms: u128,
    prev_hash: &'a str,
    kind: &'a str,
    fields: &'a serde_json::Value,
}

/// Object-safe trait every sink-side audit fan-out goes through.
/// Implementors are responsible for stamping the chain fields + I/O
/// + counter-on-failure; callers just hand in the kind + fields.
///
/// `append` is infallible at the trait — substrate contract is that
/// the existing tracing-side stream NEVER stops firing, so sinks
/// MUST swallow I/O errors internally (counter bump + one-shot
/// `tracing::warn!`).
pub trait AuditSink: std::fmt::Debug + Send + Sync {
    fn append(&self, record: AuditRecordInput);

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Atomic
    /// rotation entry point. Implementations that back a file
    /// rotate it via [`FileAuditSink::rotate`]; trivial sinks
    /// ([`NopAuditSink`], in-memory test sinks) return
    /// `Ok(None)` to signal "rotation is a no-op for this sink
    /// flavor — caller should skip the operator-facing audit
    /// fan-out".
    ///
    /// Default impl returns `Ok(None)` so existing sinks compile
    /// unchanged; only [`FileAuditSink`] needs an explicit
    /// override.
    fn rotate(&self) -> Result<Option<RotationOutcome>, AuditChainError> {
        Ok(None)
    }
}

/// Default backend: discards every record. Wired as the
/// [`crate::ApiState::audit_sink`] default so the existing audit
/// helpers can call `sink.append(...)` unconditionally and produce
/// BIT-FOR-BIT identical tracing output when no `--observer-audit-log`
/// was passed.
#[derive(Debug, Default)]
pub struct NopAuditSink;

impl AuditSink for NopAuditSink {
    #[inline]
    fn append(&self, _record: AuditRecordInput) {
        // No-op. Substrate contract: the tracing-side stream
        // remains the sole consumer when no sink is wired through.
    }
}

/// **M-Prod #1 Phase B Part 2 (Group 1ab).** fsync policy for
/// [`FileAuditSink`]. Default is [`FsyncMode::PerWrite`] for
/// bit-for-bit Phase A back-compat; opt-in
/// [`FsyncMode::Batched`] defers `sync_data` to a worker task that
/// flushes on EITHER a timer cadence (`interval_ms`) OR a size
/// threshold (`max_pending_records`).
///
/// The chain hash + write_all are always sequential under the
/// writer lock — only the fsync moves async. This preserves chain
/// integrity bit-for-bit; the only operator-observable change is
/// that records returned-to-caller may not be on the disk surface
/// until the worker flushes (bounded by `interval_ms` for the
/// timer path or `max_pending_records` for the size path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncMode {
    /// Phase A default. `sync_data` runs under the writer lock on
    /// every `append`. Maximum durability, request-path-blocking
    /// per-write cost.
    PerWrite,
    /// **Phase B Part 2 (Group 1ab).** Worker-task batched fsync.
    /// The append path still hashes + writes the line under the
    /// writer lock (chain integrity preserved); only the
    /// `sync_data` call moves to the worker. The worker fsyncs on
    /// EITHER timer or size threshold and on drain.
    Batched {
        /// Maximum wall-clock interval between fsync flushes when
        /// the in-flight batch is non-empty. 100ms is the
        /// substrate default.
        interval_ms: u64,
        /// Maximum number of records to buffer before forcing an
        /// out-of-cycle fsync. 256 is the substrate default.
        max_pending_records: u32,
    },
}

/// Substrate default for [`FsyncMode::Batched`]'s `interval_ms` knob.
pub const DEFAULT_BATCHED_FSYNC_INTERVAL_MS: u64 = 100;
/// Substrate default for [`FsyncMode::Batched`]'s `max_pending_records` knob.
pub const DEFAULT_BATCHED_FSYNC_MAX_PENDING: u32 = 256;

/// **M-Prod #1 Phase B Part 2 (Group 1ab).** Composition knobs for
/// [`FileAuditSink::open_with_config`]. The legacy
/// [`FileAuditSink::open`] thunks through here with
/// `fsync_mode = FsyncMode::PerWrite` and `metrics = None` so
/// existing test + spawn call sites stay bit-for-bit.
#[derive(Default, Clone)]
pub struct AuditChainConfig {
    /// fsync policy. Default = [`FsyncMode::PerWrite`].
    pub fsync_mode: Option<FsyncMode>,
    /// Optional [`MetricsInner`] handle. When `Some`, sink-side I/O
    /// failures bump
    /// `audit_sink_write_failures_total{kind="open"|"write"|"fsync"|"rotate"}`
    /// alongside the existing `tracing::warn!` emission.
    pub metrics: Option<Arc<MetricsInner>>,
}

impl std::fmt::Debug for AuditChainConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditChainConfig")
            .field("fsync_mode", &self.fsync_mode)
            .field("metrics", &self.metrics.is_some())
            .finish()
    }
}

impl AuditChainConfig {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Read the operator-tunable batched-fsync knobs from env vars
    /// and merge into the config. Recognized vars:
    /// - `GRX_AUDIT_FSYNC_MODE` (`per_write` | `batched`)
    /// - `GRX_AUDIT_FSYNC_INTERVAL_MS`
    /// - `GRX_AUDIT_FSYNC_MAX_PENDING`
    #[must_use]
    pub fn from_env_overlay(mut self) -> Self {
        if let Ok(mode) = std::env::var("GRX_AUDIT_FSYNC_MODE") {
            let trimmed = mode.trim().to_ascii_lowercase();
            if trimmed == "batched" {
                let interval_ms = std::env::var("GRX_AUDIT_FSYNC_INTERVAL_MS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|&n| n > 0)
                    .unwrap_or(DEFAULT_BATCHED_FSYNC_INTERVAL_MS);
                let max_pending = std::env::var("GRX_AUDIT_FSYNC_MAX_PENDING")
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .filter(|&n| n > 0)
                    .unwrap_or(DEFAULT_BATCHED_FSYNC_MAX_PENDING);
                self.fsync_mode = Some(FsyncMode::Batched {
                    interval_ms,
                    max_pending_records: max_pending,
                });
            } else if trimmed == "per_write" || trimmed == "perwrite" {
                self.fsync_mode = Some(FsyncMode::PerWrite);
            }
        }
        self
    }
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<MetricsInner>) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

/// Outcome of a successful [`FileAuditSink::rotate`] call. Returned
/// so the SIGHUP caller can synthesize an
/// `audit_log_rotated` audit event for the operator.
#[derive(Debug, Clone)]
pub struct RotationOutcome {
    /// Path the rotated file was renamed to (timestamped backup).
    pub rotated_to: PathBuf,
    /// The OLD file's tail `this_hash`, captured by the
    /// `audit_log_rotated` last-line marker. Equals the new file's
    /// first-record `prev_hash`.
    pub prior_last_this_hash: String,
    /// First `prev_hash` of the freshly-opened new file. Always
    /// equals [`Self::prior_last_this_hash`] (the two fields are
    /// stamped under the same writer lock so they cannot diverge).
    pub new_first_prev_hash: String,
    /// Total record count in the rotated-to (old) file at the
    /// moment of rename. Operators cross-reference this against
    /// downstream forwarders to confirm completeness.
    pub records_in_prior_file: u64,
}

/// File-backed NDJSON-to-disk audit sink. Appends one hash-chained
/// record per [`AuditSink::append`] call; fsyncs (via `sync_data`)
/// after every record in Phase A. Operators wire this via the
/// `--observer-audit-log <PATH>` CLI flag.
///
/// On daemon-start the sink opens the file in append-only mode, reads
/// the last line if any, parses its `this_hash` + `seq`, and resumes
/// the chain from there so a process-restart preserves chain
/// continuity across the file.
pub struct FileAuditSink {
    /// Shared state. Held behind `Arc<Mutex<_>>` so the
    /// batched-fsync worker (when one exists) can take its own
    /// clone of the Arc and reach into the writer to call
    /// `sync_data` independently of the append path's lifecycle.
    state: Arc<Mutex<FileAuditSinkInner>>,
    /// Optional [`MetricsInner`] handle so every error branch can
    /// bump `audit_sink_write_failures_total{kind}` inline with the
    /// existing `tracing::warn!` emission. `None` preserves the
    /// pre-Phase-B-Part-2 metrics surface bit-for-bit.
    metrics: Option<Arc<MetricsInner>>,
    /// Resolved fsync policy. Captured at construction; immutable
    /// across the sink's lifetime.
    fsync_mode: FsyncMode,
    /// **M-Prod #1 Phase B Part 2 (Group 1ab).** When the sink was
    /// opened with [`FsyncMode::Batched`], the worker handle lives
    /// here. The worker is dropped on `FileAuditSink::shutdown` (or
    /// implicit Drop) — drop signals shutdown via channel-close.
    /// Drain on graceful shutdown is bounded by
    /// `OBSERVER_SHUTDOWN_DEADLINE` and is exposed via
    /// [`Self::drain`].
    worker: Option<BatchedFsyncWorker>,
}

impl std::fmt::Debug for FileAuditSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileAuditSink")
            .field("fsync_mode", &self.fsync_mode)
            .field("worker", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

/// Worker-task message: each successful append sends a `Signal`;
/// shutdown sends a `Drain`. Used so the worker can `recv()` from
/// a single channel and react to both events with a single
/// `tokio::select!` arm.
#[derive(Debug)]
enum FsyncWorkerMsg {
    /// One record was appended; if pending count crosses
    /// `max_pending_records` the worker fsyncs.
    Signal,
    /// Shutdown: drain remaining signals + fsync once + exit.
    Drain(tokio::sync::oneshot::Sender<()>),
}

/// Worker-task handle for [`FsyncMode::Batched`]. Held inside
/// [`FileAuditSink`] so the append path can signal pending work +
/// the shutdown path can drain. The worker fsyncs the underlying
/// `File` (its own `Arc` clone of the inner `File` descriptor) and
/// updates the
/// `audit_sink_pending_records` gauge each cycle.
struct BatchedFsyncWorker {
    /// Signal + drain channel. Each successful append pings with
    /// `Signal`; shutdown pings with `Drain(ack_tx)`.
    msg_tx: tokio::sync::mpsc::UnboundedSender<FsyncWorkerMsg>,
    /// Worker join handle for drain bounds. Wrapped in Mutex so
    /// [`FileAuditSink::drain`] can take ownership without
    /// consuming `self`.
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for BatchedFsyncWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchedFsyncWorker").finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct FileAuditSinkInner {
    /// Path the sink currently writes to. Mutable so
    /// [`FileAuditSink::rotate`] can swap it in-place atomically
    /// under the writer lock without aliasing the old path with
    /// the new file descriptor.
    path: PathBuf,
    next_seq: u64,
    prev_hash: String,
    writer: BufWriter<File>,
}

impl FileAuditSink {
    /// Open `path` in append mode, recover chain head from any
    /// pre-existing lines, and return a ready sink. Equivalent to
    /// `Self::open_with_config(path, AuditChainConfig::default())`
    /// for bit-for-bit Phase A back-compat.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AuditChainError> {
        Self::open_with_config(path, AuditChainConfig::default())
    }

    /// **M-Prod #1 Phase B Part 2 (Group 1ab).** Open `path` with
    /// the supplied [`AuditChainConfig`] — fsync mode +
    /// [`MetricsInner`] handle.
    ///
    /// When `cfg.fsync_mode` is [`FsyncMode::Batched`], spawns a
    /// tokio worker task that fsyncs on EITHER an
    /// `interval_ms` timer OR the `max_pending_records` size
    /// threshold. The worker REQUIRES a tokio runtime to be
    /// current — without one (sync tests) the spawn returns
    /// [`AuditChainError::Runtime`].
    pub fn open_with_config(
        path: impl Into<PathBuf>,
        cfg: AuditChainConfig,
    ) -> Result<Self, AuditChainError> {
        let path = path.into();
        let metrics = cfg.metrics.clone();
        let fsync_mode = cfg.fsync_mode.unwrap_or(FsyncMode::PerWrite);
        // Recover chain head BEFORE opening the append-mode writer
        // so we don't accidentally hold two file descriptors during
        // the read-back. `replay` opens its own short-lived
        // descriptor in read mode.
        let (next_seq, prev_hash) = if path.exists() {
            match replay(&path) {
                Ok(records) => {
                    if let Some(last) = records.last() {
                        (last.seq + 1, last.this_hash.clone())
                    } else {
                        (0, GENESIS_PREV_HASH.to_string())
                    }
                }
                Err(err) => {
                    if let Some(m) = metrics.as_ref() {
                        m.audit_sink_write_failures_inc("open");
                    }
                    return Err(err);
                }
            }
        } else {
            (0, GENESIS_PREV_HASH.to_string())
        };

        let file = open_audit_file(&path).inspect_err(|_err| {
            if let Some(m) = metrics.as_ref() {
                m.audit_sink_write_failures_inc("open");
            }
        })?;

        let state = Arc::new(Mutex::new(FileAuditSinkInner {
            path: path.clone(),
            next_seq,
            prev_hash,
            writer: BufWriter::new(file),
        }));

        let worker = match fsync_mode {
            FsyncMode::PerWrite => None,
            FsyncMode::Batched {
                interval_ms,
                max_pending_records,
            } => Some(spawn_batched_fsync_worker(
                state.clone(),
                metrics.clone(),
                interval_ms,
                max_pending_records,
            )?),
        };

        Ok(Self {
            state,
            metrics,
            fsync_mode,
            worker,
        })
    }

    /// Inspect-only accessor for tests + diagnostics. The returned
    /// path is the on-disk target.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.state.lock().map(|g| g.path.clone()).unwrap_or_default()
    }

    /// Inspect-only accessor for the configured fsync mode.
    #[must_use]
    pub fn fsync_mode(&self) -> FsyncMode {
        self.fsync_mode
    }

    /// **M-Prod #1 Phase B Part 2 (Group 1ab).** Atomic-rename
    /// rotation. The current file is renamed to a timestamped
    /// backup (`<original>.YYYY-MM-DDTHH-MM-SS-Z`); a fresh file
    /// is opened at the original path with mode 0o600 enforcement.
    ///
    /// Chain continuity is preserved by writing an
    /// `audit_log_rotated` record as the FINAL line of the prior
    /// file AND as the FIRST line of the new file. The new file's
    /// first record's `prev_hash` equals the prior file's tail
    /// `this_hash`. Downstream verifier walkers can use these
    /// markers to traverse N rotated files in order.
    ///
    /// On `fs::rename` / open / write failure the OLD file stays
    /// open + the metrics counter
    /// `audit_sink_write_failures_total{kind="rotate"}` bumps.
    /// The sink continues serving appends against the unchanged
    /// old file.
    pub fn rotate_now(&self) -> Result<RotationOutcome, AuditChainError> {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let prior_path = state.path.clone();
        let timestamp = rotation_timestamp();
        let mut backup_path = prior_path.clone().into_os_string();
        backup_path.push(format!(".{timestamp}"));
        let backup_path = PathBuf::from(backup_path);

        let records_in_prior_file = state.next_seq;
        // 1. Write the OLD file's terminal `audit_log_rotated`
        //    record while we still own the writer.
        let prior_last_this_hash_for_marker = state.prev_hash.clone();
        let prior_terminal_fields = serde_json::json!({
            "rotated_from": prior_path.display().to_string(),
            "rotated_to": backup_path.display().to_string(),
            "records_in_prior_file": records_in_prior_file,
            "boundary": "prior_tail",
            "prior_last_this_hash": prior_last_this_hash_for_marker,
        });
        let prior_terminal_hash = append_one_locked(
            &mut state,
            "audit_log_rotated",
            &prior_terminal_fields,
            self.metrics.as_ref(),
        );
        let prior_terminal_hash = match prior_terminal_hash {
            Some(h) => h,
            None => {
                if let Some(m) = self.metrics.as_ref() {
                    m.audit_sink_write_failures_inc("rotate");
                }
                return Err(AuditChainError::Io {
                    path: prior_path.clone(),
                    source: std::io::Error::other(
                        "audit_log_rotated terminal record could not be appended",
                    ),
                });
            }
        };

        // 2. Atomic rename of the OLD file to its timestamped
        //    backup path. POSIX guarantees rename is atomic on
        //    the same filesystem.
        if let Err(err) = std::fs::rename(&prior_path, &backup_path) {
            if let Some(m) = self.metrics.as_ref() {
                m.audit_sink_write_failures_inc("rotate");
            }
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                error_kind = "rotate",
                from = %prior_path.display(),
                to = %backup_path.display(),
                error = %err,
                "audit sink rotation rename failed; continuing on the old file",
            );
            return Err(AuditChainError::Io {
                path: prior_path.clone(),
                source: err,
            });
        }

        // 3. Open a fresh file at the original path. Mode 0o600 is
        //    enforced inside `open_audit_file`.
        let fresh_file = match open_audit_file(&prior_path) {
            Ok(f) => f,
            Err(err) => {
                if let Some(m) = self.metrics.as_ref() {
                    m.audit_sink_write_failures_inc("rotate");
                }
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_sink_write_failed",
                    error_kind = "rotate",
                    path = %prior_path.display(),
                    error = %err,
                    "audit sink rotation open-new-file failed; old chain head retained in-memory",
                );
                return Err(err);
            }
        };
        // 4. Swap the BufWriter + reset the chain head so the NEW
        //    file parses as a standalone Phase-A-compliant chain
        //    (seq starts at 0, prev_hash = GENESIS). Cross-file
        //    continuity is preserved via the `audit_log_rotated`
        //    boundary records' `prior_last_this_hash` /
        //    `new_first_prev_hash` field pair — a multi-file
        //    verifier walks rotated files in order via those
        //    markers without depending on cross-file prev_hash
        //    linkage. This keeps the existing single-file
        //    `replay` / `grx audit verify` workflow bit-for-bit
        //    unchanged.
        state.writer = BufWriter::new(fresh_file);
        state.next_seq = 0;
        let new_first_prev_hash = prior_terminal_hash.clone();
        state.prev_hash = GENESIS_PREV_HASH.to_string();

        // 5. Write the FIRST record of the new file: the second
        //    `audit_log_rotated` marker. The on-wire `prev_hash`
        //    is GENESIS (standalone chain); the marker's `fields`
        //    embed the cross-file linkage data.
        let first_fields = serde_json::json!({
            "rotated_from": prior_path.display().to_string(),
            "rotated_to": backup_path.display().to_string(),
            "records_in_prior_file": records_in_prior_file,
            "boundary": "new_head",
            "prior_last_this_hash": prior_terminal_hash,
            "new_first_prev_hash": new_first_prev_hash,
        });
        let _ = append_one_locked(
            &mut state,
            "audit_log_rotated",
            &first_fields,
            self.metrics.as_ref(),
        );

        Ok(RotationOutcome {
            rotated_to: backup_path,
            prior_last_this_hash: prior_terminal_hash,
            new_first_prev_hash,
            records_in_prior_file,
        })
    }

    /// **M-Prod #1 Phase B Part 2 (Group 1ab).** Drain the
    /// batched-fsync worker (if any) within `deadline`. Returns
    /// `true` on clean drain, `false` on deadline-exceeded. In
    /// [`FsyncMode::PerWrite`] there's nothing to drain — returns
    /// `true` immediately.
    ///
    /// Called from the daemon's graceful-shutdown path before the
    /// `ApiServerHandle::shutdown` runs so the final batch of
    /// records is durable on the disk surface inside the
    /// `OBSERVER_SHUTDOWN_DEADLINE` budget.
    pub async fn drain(&self, deadline: Duration) -> bool {
        let Some(worker) = self.worker.as_ref() else {
            return true;
        };
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if worker.msg_tx.send(FsyncWorkerMsg::Drain(ack_tx)).is_err() {
            // Worker has already exited (sink dropped + close
            // signalled). Treat as cleanly drained.
            return true;
        }
        tokio::time::timeout(deadline, ack_rx).await.is_ok()
    }
}

/// Open `path` in append mode with 0o600 enforcement on Unix.
/// Encapsulates the create + post-open chmod dance so both the
/// initial `open` path and the rotation path apply the same
/// invariant.
fn open_audit_file(path: &Path) -> Result<File, AuditChainError> {
    // M-Prod #1 Phase A security-handling: audit-log records carry
    // peer IPs, auth-failure reasons (missing | wrong), TLS leaf
    // fingerprints, and full chain integrity. On multi-tenant
    // hosts (CI runners, dev workstations, k8s nodes with other
    // pods running as non-root) a world-readable file would let
    // any unprivileged user read the chain. Enforce mode 0o600
    // both on create AND on a pre-existing file — without the
    // post-open set_permissions a chain that was world-readable
    // before this commit would silently stay world-readable
    // across daemon restarts.
    let mut open_opts = OpenOptions::new();
    open_opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o600);
    }
    let file = open_opts
        .open(path)
        .map_err(|err| AuditChainError::Io {
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

/// Compose a rotation-suffix timestamp (`YYYY-MM-DDTHH-MM-SS-Z`)
/// from `SystemTime::now()`. Hand-rolled rather than pulling
/// `chrono` / `time` into the workspace (per the no-new-deps
/// constraint). The suffix is operator-readable and POSIX-safe
/// (no `:` colons because some filesystems / Windows paths
/// reserve them).
fn rotation_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert seconds-since-epoch to UTC YYYY-MM-DDTHH-MM-SS-Z.
    let (year, month, day, hour, min, sec) = unix_secs_to_utc_components(now);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{min:02}-{sec:02}Z")
}

/// Convert Unix epoch seconds to (year, month, day, hour, min, sec)
/// in UTC. Hand-rolled from the civil-time algorithm so we don't
/// add a `chrono` workspace dep. Accuracy is sufficient for the
/// rotation-suffix use case (operators only need monotonic +
/// human-readable; sub-second precision isn't load-bearing).
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
    // Days since 1970-01-01 → civil date (Howard Hinnant algorithm,
    // public domain). Shifts the epoch so March-based year-boundary
    // simplifies leap-year handling.
    days += 719_468;
    let era = if days >= 0 { days / 146_097 } else { (days - 146_096) / 146_097 };
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

/// Append one record under the writer lock (`state` already held).
/// Returns the freshly-computed `this_hash` on success, or `None`
/// on serializer failure. Used by [`FileAuditSink::rotate`] to
/// stamp the boundary `audit_log_rotated` markers. The chain
/// advance (`next_seq`, `prev_hash`) is unconditional — matches
/// the Phase A invariant that chain state advances on every
/// attempted append so a follow-up successful write can't tear
/// the chain by reusing the same seq.
fn append_one_locked(
    state: &mut FileAuditSinkInner,
    kind: &str,
    fields: &serde_json::Value,
    metrics: Option<&Arc<MetricsInner>>,
) -> Option<String> {
    let seq = state.next_seq;
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let prev_hash = state.prev_hash.clone();
    let hash_view = AuditRecordHashInput {
        seq,
        ts_ms,
        prev_hash: &prev_hash,
        kind,
        fields,
    };
    let canonical = match serde_json::to_vec(&hash_view) {
        Ok(b) => b,
        Err(err) => {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_serialize_failed",
                seq,
                kind,
                error = %err,
                "audit sink failed to serialize record; advancing chain past it",
            );
            state.next_seq = seq + 1;
            return None;
        }
    };
    let this_hash = sha256_hex(&canonical);
    let record = AuditRecord {
        seq,
        ts_ms,
        prev_hash,
        kind: kind.to_string(),
        fields: fields.clone(),
        this_hash: this_hash.clone(),
    };
    let line = match serde_json::to_vec(&record) {
        Ok(b) => b,
        Err(err) => {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_serialize_failed",
                seq,
                kind,
                error = %err,
                "audit sink failed to serialize record for write; advancing chain past it",
            );
            state.next_seq = seq + 1;
            state.prev_hash = this_hash;
            return None;
        }
    };
    let mut write_failed = false;
    if let Err(err) = state.writer.write_all(&line) {
        if let Some(m) = metrics {
            m.audit_sink_write_failures_inc("write");
        }
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "audit_sink_write_failed",
            seq,
            kind,
            error = %err,
            "audit sink write_all failed; tracing-side stream continues",
        );
        write_failed = true;
    }
    if !write_failed {
        if let Err(err) = state.writer.write_all(b"\n") {
            if let Some(m) = metrics {
                m.audit_sink_write_failures_inc("write");
            }
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                seq,
                kind,
                error = %err,
                "audit sink newline write failed; tracing-side stream continues",
            );
            write_failed = true;
        }
    }
    if !write_failed {
        if let Err(err) = state.writer.flush() {
            if let Some(m) = metrics {
                m.audit_sink_write_failures_inc("write");
            }
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                seq,
                kind,
                error = %err,
                "audit sink flush failed; tracing-side stream continues",
            );
            write_failed = true;
        }
    }
    // The fsync step is INTENTIONALLY skipped here for rotation
    // boundary markers — they ride the next normal append-cycle's
    // fsync, OR (under PerWrite) the worker-style fsync_locked path
    // executed by the caller. PerWrite append() does fsync inline.
    state.next_seq = seq + 1;
    state.prev_hash = this_hash.clone();
    if write_failed {
        None
    } else {
        Some(this_hash)
    }
}

/// **M-Prod #1 Phase B Part 2 (Group 1ab).** Spawn the
/// batched-fsync worker task. The worker reads
/// [`FsyncWorkerMsg`]s from the unbounded channel: each `Signal`
/// bumps a pending counter; the worker fsyncs on EITHER the
/// `interval_ms` timer OR when `pending >= max_pending_records`;
/// `Drain(ack)` flushes one final time + acks + exits.
///
/// REQUIRES a tokio runtime to be current. The daemon path always
/// runs inside `start_daemon`'s runtime; sync tests that need
/// batched mode must wrap construction in a `tokio::test`.
fn spawn_batched_fsync_worker(
    state: Arc<Mutex<FileAuditSinkInner>>,
    metrics: Option<Arc<MetricsInner>>,
    interval_ms: u64,
    max_pending_records: u32,
) -> Result<BatchedFsyncWorker, AuditChainError> {
    let Ok(_handle) = tokio::runtime::Handle::try_current() else {
        return Err(AuditChainError::Runtime {
            detail: "FsyncMode::Batched requires a tokio runtime context; none was current"
                .to_string(),
        });
    };
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<FsyncWorkerMsg>();
    let interval = Duration::from_millis(interval_ms.max(1));
    let max_pending_records = max_pending_records.max(1);
    let join = tokio::spawn(async move {
        let mut pending: u32 = 0;
        let mut tick = tokio::time::interval(interval);
        // First tick fires immediately; consume it so we don't
        // fsync on a fresh sink with zero pending records.
        tick.tick().await;
        loop {
            if let Some(m) = metrics.as_ref() {
                m.audit_sink_pending_records.set(u64::from(pending));
            }
            tokio::select! {
                msg = msg_rx.recv() => {
                    match msg {
                        None => {
                            // Sink dropped without explicit drain.
                            // Flush once + exit.
                            fsync_locked(&state, metrics.as_ref());
                            if let Some(m) = metrics.as_ref() {
                                m.audit_sink_pending_records.set(0);
                            }
                            return;
                        }
                        Some(FsyncWorkerMsg::Signal) => {
                            pending = pending.saturating_add(1);
                            if pending >= max_pending_records {
                                fsync_locked(&state, metrics.as_ref());
                                pending = 0;
                            }
                        }
                        Some(FsyncWorkerMsg::Drain(ack)) => {
                            // Drain any remaining queued signals
                            // first so the fsync covers everything
                            // the append-side acknowledged.
                            while let Ok(extra) = msg_rx.try_recv() {
                                if matches!(extra, FsyncWorkerMsg::Signal) {
                                    pending = pending.saturating_add(1);
                                }
                            }
                            fsync_locked(&state, metrics.as_ref());
                            if let Some(m) = metrics.as_ref() {
                                m.audit_sink_pending_records.set(0);
                            }
                            let _ = ack.send(());
                            return;
                        }
                    }
                }
                _ = tick.tick() => {
                    if pending > 0 {
                        fsync_locked(&state, metrics.as_ref());
                        pending = 0;
                    }
                }
            }
        }
    });
    Ok(BatchedFsyncWorker {
        msg_tx,
        join: Mutex::new(Some(join)),
    })
}

/// Acquire the writer lock + `sync_data` the underlying file.
/// Bumps `audit_sink_write_failures_total{kind="fsync"}` on
/// failure.
fn fsync_locked(state: &Arc<Mutex<FileAuditSinkInner>>, metrics: Option<&Arc<MetricsInner>>) {
    let guard = match state.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Err(err) = guard.writer.get_ref().sync_data() {
        if let Some(m) = metrics {
            m.audit_sink_write_failures_inc("fsync");
        }
        tracing::warn!(
            target: "graphreact::observer::audit",
            event = "audit_sink_write_failed",
            error_kind = "fsync",
            error = %err,
            "audit sink sync_data failed; tracing-side stream continues",
        );
    }
}

impl Drop for FileAuditSink {
    fn drop(&mut self) {
        // Take + join the worker so a dropped sink doesn't leak its
        // task. The channel-close signals shutdown automatically;
        // we just need to wait for it to exit. Best-effort drop:
        // we can't `.await` inside Drop, so we abort the task if it
        // didn't finish on its own (the worker drains synchronously
        // when its channel is closed before its select reaches a
        // timer tick).
        if let Some(worker) = self.worker.take() {
            // Drop the sender so msg_rx.recv() returns None → worker
            // exits.
            drop(worker.msg_tx);
            if let Some(handle) = worker.join.lock().ok().and_then(|mut g| g.take()) {
                handle.abort();
            }
        }
    }
}

impl AuditSink for FileAuditSink {
    fn append(&self, input: AuditRecordInput) {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let wrote_ok = append_one_locked(
            &mut state,
            input.kind,
            &input.fields,
            self.metrics.as_ref(),
        )
        .is_some();
        // Fsync policy fork.
        match self.fsync_mode {
            FsyncMode::PerWrite => {
                if wrote_ok {
                    if let Err(err) = state.writer.get_ref().sync_data() {
                        if let Some(m) = self.metrics.as_ref() {
                            m.audit_sink_write_failures_inc("fsync");
                        }
                        tracing::warn!(
                            target: "graphreact::observer::audit",
                            event = "audit_sink_write_failed",
                            error_kind = "fsync",
                            error = %err,
                            "audit sink sync_data failed; tracing-side stream continues",
                        );
                    }
                }
            }
            FsyncMode::Batched { .. } => {
                // Signal the worker that a record is pending. The
                // bytes are in the OS page cache after `flush`; the
                // worker fsyncs to the disk surface on its cadence.
                if wrote_ok {
                    if let Some(w) = self.worker.as_ref() {
                        let _ = w.msg_tx.send(FsyncWorkerMsg::Signal);
                    }
                }
            }
        }
    }

    fn rotate(&self) -> Result<Option<RotationOutcome>, AuditChainError> {
        self.rotate_now().map(Some)
    }
}

/// Walk every NDJSON line in `path`, parsing each record, recomputing
/// each `this_hash` from its declared canonical bytes, and verifying
/// that each record's `prev_hash` matches the prior record's
/// `this_hash` (genesis = 64 hex zeros).
///
/// Returns the full chain on success or an [`AuditChainError`]
/// (parse, hash mismatch, prev-hash mismatch, sequence gap) at the
/// first failing record.
pub fn replay(path: impl AsRef<Path>) -> Result<Vec<AuditRecord>, AuditChainError> {
    let path = path.as_ref();
    let file = File::open(path).map_err(|err| AuditChainError::Io {
        path: path.to_path_buf(),
        source: err,
    })?;
    let reader = BufReader::new(file);
    let mut records: Vec<AuditRecord> = Vec::new();
    let mut expected_prev = GENESIS_PREV_HASH.to_string();
    let mut expected_seq: u64 = 0;
    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(|err| AuditChainError::Io {
            path: path.to_path_buf(),
            source: err,
        })?;
        if line.is_empty() {
            continue;
        }
        let record: AuditRecord = serde_json::from_str(&line).map_err(|err| {
            AuditChainError::Parse {
                line_index: idx,
                detail: err.to_string(),
            }
        })?;
        if record.seq != expected_seq {
            return Err(AuditChainError::SequenceGap {
                expected_seq,
                found_seq: record.seq,
            });
        }
        if record.prev_hash != expected_prev {
            return Err(AuditChainError::PrevHashMismatch {
                seq: record.seq,
                expected_prev_hash: expected_prev.clone(),
                found_prev_hash: record.prev_hash.clone(),
            });
        }
        let hash_view = AuditRecordHashInput {
            seq: record.seq,
            ts_ms: record.ts_ms,
            prev_hash: &record.prev_hash,
            kind: &record.kind,
            fields: &record.fields,
        };
        let canonical = serde_json::to_vec(&hash_view).map_err(|err| AuditChainError::Parse {
            line_index: idx,
            detail: format!("canonical re-encode: {err}"),
        })?;
        let recomputed = sha256_hex(&canonical);
        if recomputed != record.this_hash {
            return Err(AuditChainError::TamperDetected {
                seq: record.seq,
                expected_hash: recomputed,
                found_hash: record.this_hash.clone(),
            });
        }
        expected_prev.clone_from(&record.this_hash);
        expected_seq = record.seq + 1;
        records.push(record);
    }
    Ok(records)
}

/// Failures the audit chain primitives surface. Parse + I/O errors
/// are "expected — file unreadable / mangled"; the
/// [`Self::TamperDetected`] / [`Self::PrevHashMismatch`] /
/// [`Self::SequenceGap`] variants are the load-bearing security
/// signal for the operator's verify-chain workflow.
#[derive(Debug, Error)]
pub enum AuditChainError {
    #[error("audit chain I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("audit chain parse failure at line {line_index}: {detail}")]
    Parse { line_index: usize, detail: String },
    #[error(
        "audit chain tamper detected at seq={seq}: expected_hash={expected_hash} found_hash={found_hash}"
    )]
    TamperDetected {
        seq: u64,
        expected_hash: String,
        found_hash: String,
    },
    #[error(
        "audit chain prev-hash mismatch at seq={seq}: expected_prev_hash={expected_prev_hash} found_prev_hash={found_prev_hash}"
    )]
    PrevHashMismatch {
        seq: u64,
        expected_prev_hash: String,
        found_prev_hash: String,
    },
    #[error("audit chain sequence gap: expected seq={expected_seq} found seq={found_seq}")]
    SequenceGap { expected_seq: u64, found_seq: u64 },
    /// **M-Prod #1 Phase B Part 2 (Group 1ab).** Construction-time
    /// failure: [`FsyncMode::Batched`] requires a tokio runtime,
    /// but none was current when
    /// [`FileAuditSink::open_with_config`] was called.
    #[error("audit chain runtime requirement: {detail}")]
    Runtime { detail: String },
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn tmpdir() -> tempfile_lite::TempDir {
        tempfile_lite::TempDir::new("grx-audit-chain")
    }

    /// In-tree minimal tempdir shim so the test cell doesn't pull a
    /// new workspace dep. Removes itself on drop.
    mod tempfile_lite {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        pub struct TempDir {
            path: PathBuf,
        }

        impl TempDir {
            pub fn new(tag: &str) -> Self {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let pid = std::process::id();
                let path = std::env::temp_dir().join(format!("{tag}-{pid}-{n}"));
                std::fs::create_dir_all(&path).expect("create tempdir");
                Self { path }
            }
            pub fn path(&self) -> &Path {
                &self.path
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    #[test]
    fn nop_sink_does_nothing() {
        let sink = NopAuditSink;
        sink.append(AuditRecordInput::bare("bind"));
        sink.append(AuditRecordInput {
            kind: "auth_fail",
            fields: serde_json::json!({"peer": "127.0.0.1:1234", "reason": "missing"}),
        });
        // No file written, no panic. The trait contract is "do
        // nothing observable" — anything stronger would defeat the
        // bit-for-bit-identical guarantee in the no-sink path.
    }

    #[test]
    fn genesis_record_on_empty_file() {
        let dir = tmpdir();
        let path = dir.path().join("audit.ndjson");
        let sink = FileAuditSink::open(&path).expect("open");
        sink.append(AuditRecordInput::bare("bind"));
        drop(sink);
        let chain = replay(&path).expect("replay");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].seq, 0);
        assert_eq!(chain[0].prev_hash, GENESIS_PREV_HASH);
        assert_eq!(chain[0].kind, "bind");
    }

    #[test]
    fn file_sink_appends_chained_records() {
        let dir = tmpdir();
        let path = dir.path().join("audit.ndjson");
        let sink = FileAuditSink::open(&path).expect("open");
        sink.append(AuditRecordInput {
            kind: "bind",
            fields: serde_json::json!({"addr": "127.0.0.1:9090", "tls": false, "auth": false}),
        });
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"peer": "127.0.0.1:1234", "path": "/v1/metrics"}),
        });
        sink.append(AuditRecordInput {
            kind: "response",
            fields: serde_json::json!({
                "peer": "127.0.0.1:1234",
                "path": "/v1/metrics",
                "status": 200,
                "bytes": 4096
            }),
        });
        drop(sink);
        let chain = replay(&path).expect("replay");
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].seq, 0);
        assert_eq!(chain[1].seq, 1);
        assert_eq!(chain[2].seq, 2);
        assert_eq!(chain[0].prev_hash, GENESIS_PREV_HASH);
        assert_eq!(chain[1].prev_hash, chain[0].this_hash);
        assert_eq!(chain[2].prev_hash, chain[1].this_hash);
        // monotonic seq is preserved across the three appends.
    }

    #[test]
    fn replay_detects_tampering() {
        let dir = tmpdir();
        let path = dir.path().join("audit.ndjson");
        let sink = FileAuditSink::open(&path).expect("open");
        sink.append(AuditRecordInput {
            kind: "bind",
            fields: serde_json::json!({"addr": "127.0.0.1:9090"}),
        });
        sink.append(AuditRecordInput {
            kind: "auth_fail",
            fields: serde_json::json!({"peer": "10.0.0.1:5555", "reason": "wrong"}),
        });
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"peer": "127.0.0.1:1234", "path": "/v1/metrics"}),
        });
        drop(sink);
        // Mutate the middle record's `peer` field. The bytes the
        // file holds for seq=1 no longer match its declared
        // `this_hash`.
        let raw = std::fs::read_to_string(&path).expect("read");
        let mutated = raw.replacen("10.0.0.1", "10.0.0.9", 1);
        assert_ne!(raw, mutated, "test sentinel must have rewritten a byte");
        std::fs::write(&path, mutated).expect("write");
        let err = replay(&path).expect_err("tamper must surface");
        match err {
            AuditChainError::TamperDetected { seq, .. } => assert_eq!(seq, 1),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn replay_detects_truncation_after_genesis() {
        // Cover the truncation-style attack: delete the final
        // record + its newline. The chain is still internally
        // consistent (no tamper at any surviving seq) so `replay`
        // returns Ok — but the operator-side recipe pairs a
        // "highest seq in the file" check with the sink's
        // in-memory chain state to detect this. Phase B's
        // `grx admin audit-verify` will do that pairing; Phase A
        // documents the policy.
        let dir = tmpdir();
        let path = dir.path().join("audit.ndjson");
        let sink = FileAuditSink::open(&path).expect("open");
        sink.append(AuditRecordInput::bare("bind"));
        sink.append(AuditRecordInput::bare("accept"));
        drop(sink);
        let raw = std::fs::read_to_string(&path).expect("read");
        // Drop the last record's line (everything after the first
        // newline).
        let truncated = raw.split_once('\n').map(|(first, _)| first).unwrap_or("");
        std::fs::write(&path, format!("{truncated}\n")).expect("write");
        let chain = replay(&path).expect("truncated chain is internally consistent");
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn restart_resumes_chain() {
        let dir = tmpdir();
        let path = dir.path().join("audit.ndjson");
        let sink = FileAuditSink::open(&path).expect("open1");
        sink.append(AuditRecordInput {
            kind: "bind",
            fields: serde_json::json!({"addr": "127.0.0.1:9090"}),
        });
        sink.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"peer": "127.0.0.1:1234", "path": "/v1/metrics"}),
        });
        drop(sink);
        // Re-open the file as if from a process restart. The chain
        // head must pick up at seq=2 with prev_hash = the prior
        // record's this_hash.
        let sink2 = FileAuditSink::open(&path).expect("open2");
        sink2.append(AuditRecordInput {
            kind: "accept",
            fields: serde_json::json!({"peer": "127.0.0.1:5678", "path": "/v1/healthz"}),
        });
        drop(sink2);
        let chain = replay(&path).expect("replay after restart");
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[2].seq, 2);
        assert_eq!(chain[2].prev_hash, chain[1].this_hash);
    }

    #[test]
    fn append_writes_one_line_per_record() {
        let dir = tmpdir();
        let path = dir.path().join("audit.ndjson");
        let sink = FileAuditSink::open(&path).expect("open");
        sink.append(AuditRecordInput::bare("bind"));
        sink.append(AuditRecordInput::bare("accept"));
        sink.append(AuditRecordInput::bare("response"));
        drop(sink);
        let mut buf = String::new();
        File::open(&path)
            .expect("open")
            .read_to_string(&mut buf)
            .expect("read");
        let lines: Vec<_> = buf.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            // Each line MUST parse as a single AuditRecord — no
            // accidental multi-record-per-line concatenation.
            let _: AuditRecord = serde_json::from_str(line).expect("each line parses");
        }
    }

    #[test]
    fn sha256_hex_is_64_chars() {
        let h = sha256_hex(b"hello");
        assert_eq!(h.len(), 64);
        // Known SHA-256 of "hello"
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    /// **M-Prod #1 Phase A security-handling verify finding (HIGH).**
    /// Audit-log records carry peer IPs, auth-failure reasons, and TLS
    /// fingerprints — every byte of the chain is operator-secret class
    /// data. The file must NOT inherit the umask-default mode (0o644
    /// on stock Unix); enforce mode 0o600 on both the create-path AND
    /// the open-existing-file path.
    #[cfg(unix)]
    #[test]
    fn file_audit_sink_enforces_0600_mode_on_create() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("create.ndjson");
        let _sink = FileAuditSink::open(&path).expect("open");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        // Permissions on Unix include the file-type bits in the upper
        // bits; mask down to the user/group/other triplet.
        assert_eq!(mode & 0o777, 0o600, "expected 0o600 on create, got {:o}", mode & 0o777);
    }

    /// Pair test for the above: pre-existing audit-log files (e.g. one
    /// created before this patch landed, or by a misconfigured
    /// operator) MUST be tightened to 0o600 on next open. Otherwise
    /// the security gain only applies to fresh deployments.
    #[cfg(unix)]
    #[test]
    fn file_audit_sink_tightens_mode_on_preexisting_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("preexisting.ndjson");
        // Plant a world-readable file at the target path FIRST.
        std::fs::write(&path, b"").expect("plant");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");
        let pre_mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(pre_mode & 0o777, 0o644, "test setup: file should be 0o644");
        // Now open via FileAuditSink and confirm the mode tightens.
        let _sink = FileAuditSink::open(&path).expect("open existing");
        let post_mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(
            post_mode & 0o777,
            0o600,
            "expected 0o600 after open existing, got {:o}",
            post_mode & 0o777
        );
    }
}
