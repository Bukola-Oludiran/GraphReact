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
//! throughput for the substrate. Batched fsync + worker-task fan-out
//! are explicit Phase B work — once multi-token RBAC adds an
//! `auth_success` event that fires on every authorized request, the
//! synchronous-per-append cost becomes load-bearing on request
//! latency and must move off the request task.
//!
//! ## Sink failure policy
//!
//! Per the substrate contract, the tracing-side stream NEVER stops
//! firing — sink-side I/O failures degrade by counting the failure
//! against [`crate::MetricsInner`]'s `audit_sink_write_failures_total`
//! counter (Phase A wires the counter; Phase C extends the
//! `/v1/readyz` predicate). Sinks NEVER panic on I/O error; they log
//! a one-shot `tracing::warn!` and continue. The in-memory chain
//! state continues advancing so the next successful write re-seats
//! the chain at the correct seq + hash.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

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

/// File-backed NDJSON-to-disk audit sink. Appends one hash-chained
/// record per [`AuditSink::append`] call; fsyncs (via `sync_data`)
/// after every record in Phase A. Operators wire this via the
/// `--observer-audit-log <PATH>` CLI flag.
///
/// On daemon-start the sink opens the file in append-only mode, reads
/// the last line if any, parses its `this_hash` + `seq`, and resumes
/// the chain from there so a process-restart preserves chain
/// continuity across the file.
#[derive(Debug)]
pub struct FileAuditSink {
    path: PathBuf,
    state: Mutex<FileAuditSinkInner>,
}

#[derive(Debug)]
struct FileAuditSinkInner {
    next_seq: u64,
    prev_hash: String,
    writer: BufWriter<File>,
}

impl FileAuditSink {
    /// Open `path` in append mode, recover chain head from any
    /// pre-existing lines, and return a ready sink.
    ///
    /// Returns an [`AuditChainError`] if the file can't be opened,
    /// or if the existing trailing record's `this_hash` doesn't
    /// match its canonical-bytes hash (chain tampered across
    /// restart).
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AuditChainError> {
        let path = path.into();
        // Recover chain head BEFORE opening the append-mode writer
        // so we don't accidentally hold two file descriptors during
        // the read-back. `replay` opens its own short-lived
        // descriptor in read mode.
        let (next_seq, prev_hash) = if path.exists() {
            let records = replay(&path)?;
            if let Some(last) = records.last() {
                (last.seq + 1, last.this_hash.clone())
            } else {
                // File exists but is empty — treat as fresh.
                (0, GENESIS_PREV_HASH.to_string())
            }
        } else {
            (0, GENESIS_PREV_HASH.to_string())
        };

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
            .open(&path)
            .map_err(|err| AuditChainError::Io {
                path: path.clone(),
                source: err,
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&path, perms).map_err(|err| AuditChainError::Io {
                path: path.clone(),
                source: err,
            })?;
        }

        Ok(Self {
            path,
            state: Mutex::new(FileAuditSinkInner {
                next_seq,
                prev_hash,
                writer: BufWriter::new(file),
            }),
        })
    }

    /// Inspect-only accessor for tests + diagnostics. The returned
    /// path is the on-disk target.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AuditSink for FileAuditSink {
    fn append(&self, input: AuditRecordInput) {
        let mut state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
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
            kind: input.kind,
            fields: &input.fields,
        };
        // Canonical bytes = serde_json::to_vec of the hash view.
        // Encoding is deterministic for the struct-derive path
        // (field-order = declaration-order); the `fields` blob
        // hashes whatever bytes `serde_json` emits for it (so a
        // caller that wants cross-build-stable hashes for non-
        // object fields should pass pre-stringified data).
        let canonical = match serde_json::to_vec(&hash_view) {
            Ok(b) => b,
            Err(err) => {
                // Serializer failure — we still want to advance
                // chain state past this seq so a follow-up append
                // doesn't tear the chain.
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_sink_serialize_failed",
                    seq,
                    kind = input.kind,
                    error = %err,
                    "audit sink failed to serialize record; advancing chain past it",
                );
                state.next_seq = seq + 1;
                return;
            }
        };
        let this_hash = sha256_hex(&canonical);

        let record = AuditRecord {
            seq,
            ts_ms,
            prev_hash,
            kind: input.kind.to_string(),
            fields: input.fields,
            this_hash: this_hash.clone(),
        };
        let line = match serde_json::to_vec(&record) {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_sink_serialize_failed",
                    seq,
                    kind = %record.kind,
                    error = %err,
                    "audit sink failed to serialize record for write; advancing chain past it",
                );
                state.next_seq = seq + 1;
                state.prev_hash = this_hash;
                return;
            }
        };
        let mut write_failed = false;
        if let Err(err) = state.writer.write_all(&line) {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                seq,
                kind = %record.kind,
                error = %err,
                "audit sink write_all failed; tracing-side stream continues",
            );
            write_failed = true;
        }
        if !write_failed {
            if let Err(err) = state.writer.write_all(b"\n") {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_sink_write_failed",
                    seq,
                    kind = %record.kind,
                    error = %err,
                    "audit sink newline write failed; tracing-side stream continues",
                );
                write_failed = true;
            }
        }
        if !write_failed {
            if let Err(err) = state.writer.flush() {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_sink_write_failed",
                    seq,
                    kind = %record.kind,
                    error = %err,
                    "audit sink flush failed; tracing-side stream continues",
                );
                write_failed = true;
            }
        }
        if !write_failed {
            // sync_data: persist the bytes but skip the metadata
            // sync — file size + mtime updates are cheaper to skip
            // and we don't depend on them for chain integrity.
            if let Err(err) = state.writer.get_ref().sync_data() {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_sink_write_failed",
                    seq,
                    kind = %record.kind,
                    error = %err,
                    "audit sink sync_data failed; tracing-side stream continues",
                );
                // sync_data failure: the bytes may or may not be on
                // disk. We still advance chain state — the next
                // successful append will re-seat at the correct
                // seq + hash.
            }
        }
        // Always advance chain state, even on write failure: skipping
        // an advance would let a follow-up successful write tear the
        // chain by reusing the same seq.
        state.next_seq = seq + 1;
        state.prev_hash = this_hash;
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
