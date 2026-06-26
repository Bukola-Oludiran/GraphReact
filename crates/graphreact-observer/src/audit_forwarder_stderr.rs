//! **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Reference
//! [`AuditForwarder`] writing `AuditRecord` values as JSON lines to
//! the process stderr stream.
//!
//! Kubernetes-native: every k8s node runs a log-collector DaemonSet
//! (`fluentd` / `fluent-bit` / `opentelemetry-collector` /
//! `vector` / `aws-for-fluent-bit` / `google-cloud-ops-agent`) that
//! tails container stderr and ships to any backend (S3 / Loki /
//! GCS / OpenSearch / Splunk / Datadog). Zero-config for the
//! kubernetes-shaped operator path: enable the forwarder via
//! `--observer-audit-forwarder-stderr` and the cluster's existing
//! log pipeline picks up the chain.
//!
//! ## File mode
//!
//! NOT enforced — stderr is a process file descriptor; OS handles
//! permissions on the controlling tty / pod log file. The
//! audit-log + spool 0o600 invariant doesn't apply here.

use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use crate::audit_chain::AuditRecord;
use crate::audit_forwarders::AuditForwarder;

/// Static name for [`AuditForwarder::name`] reporting +
/// `audit_forwarder_records_total{forwarder="stderr_jsonl"}`.
pub const STDERR_FORWARDER_NAME: &str = "stderr_jsonl";

/// Process-stderr JSON-lines forwarder. Each [`Self::forward`]
/// call writes one JSON line + `\n` to `io::stderr()`. The write
/// is locked behind an in-process `Mutex<()>` so concurrent
/// fan-out from multiple forwarders (each on its own worker task)
/// can't interleave bytes mid-line.
#[derive(Debug, Default)]
pub struct StderrJsonLinesForwarder {
    /// Guard so concurrent worker tasks don't interleave the
    /// `write_all + write_all(b"\n")` pair. The `io::stderr()`
    /// global already locks per-write under the hood, but a
    /// per-record TWO-write sequence needs explicit guarding to
    /// keep lines atomic. Empty payload — just the lock semantics.
    guard: Mutex<()>,
}

impl StderrJsonLinesForwarder {
    /// Construct a fresh stderr forwarder. Zero-config; the global
    /// `io::stderr()` handle is the on-write target.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl AuditForwarder for StderrJsonLinesForwarder {
    fn name(&self) -> &'static str {
        STDERR_FORWARDER_NAME
    }

    fn forward(&self, record: &AuditRecord) {
        let line = match serde_json::to_vec(record) {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_sink_write_failed",
                    error_kind = "stderr_serialize",
                    seq = record.seq,
                    kind = %record.kind,
                    error = %err,
                    "stderr forwarder failed to serialize record; dropping",
                );
                return;
            }
        };
        let _guard = self.guard.lock().unwrap_or_else(|p| p.into_inner());
        let mut handle = std::io::stderr().lock();
        if let Err(err) = handle.write_all(&line) {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                error_kind = "stderr_write",
                seq = record.seq,
                kind = %record.kind,
                error = %err,
                "stderr forwarder write_all failed",
            );
            return;
        }
        if let Err(err) = handle.write_all(b"\n") {
            tracing::warn!(
                target: "graphreact::observer::audit",
                event = "audit_sink_write_failed",
                error_kind = "stderr_write",
                seq = record.seq,
                kind = %record.kind,
                error = %err,
                "stderr forwarder newline write failed",
            );
        }
        // No explicit flush — stderr is line-buffered when
        // attached to a tty and unbuffered otherwise; the OS does
        // the right thing.
    }

    fn drain<'a>(
        &'a self,
        _deadline: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let _guard = self.guard.lock().unwrap_or_else(|p| p.into_inner());
            let mut handle = std::io::stderr().lock();
            handle.flush().is_ok()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_chain::AuditRecord;

    fn rec(seq: u64) -> AuditRecord {
        AuditRecord {
            seq,
            ts_ms: 0,
            prev_hash: "0".repeat(64),
            kind: "bind".to_string(),
            fields: serde_json::json!({"i": seq}),
            this_hash: "a".repeat(64),
        }
    }

    #[test]
    fn name_is_closed_vocab() {
        let f = StderrJsonLinesForwarder::new();
        assert_eq!(f.name(), STDERR_FORWARDER_NAME);
    }

    #[test]
    fn forward_does_not_panic() {
        let f = StderrJsonLinesForwarder::new();
        // We can't redirect process stderr in a unit test without
        // pulling new deps; we only assert the call doesn't panic.
        // Subprocess-level smoke checking lives in the CLI integration
        // suite.
        f.forward(&rec(0));
    }
}
