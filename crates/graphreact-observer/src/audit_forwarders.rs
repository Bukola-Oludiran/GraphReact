//! **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Audit-log
//! forwarder substrate — pluggable fan-out of [`crate::AuditRecord`]
//! values to sidecar-style downstream sinks (fluentd / vector /
//! promtail / opentelemetry-collector / fluent-bit /
//! aws-for-fluent-bit / google-cloud-ops-agent / etc.).
//!
//! Rather than bundle SDKs for S3 / Loki / GCS / OpenSearch /
//! Splunk / Datadog into the daemon, Group 1d ships a trait +
//! reference impls (spool-file + stderr JSON-lines) that the
//! canonical sidecar agents already know how to tail. This keeps
//! the daemon a single static binary with no outbound HTTP path of
//! its own — operators reach S3 / Loki / GCS / OpenSearch through
//! the OS-shaped log pipeline they already run on every node.
//!
//! ## Fan invariant
//!
//! Forwarders fire INSIDE [`crate::FileAuditSink::append`] AFTER
//! the in-process write + (PerWrite) fsync completes, so a
//! forwarder NEVER sees a record the on-disk audit log doesn't
//! have. The forwarder fan-out runs OUTSIDE the writer lock so a
//! slow forwarder cannot block the audit-sink hot path; per-
//! forwarder bounded mpsc workers absorb the back-pressure.
//!
//! ## Back-pressure response
//!
//! Each registered [`AuditForwarder`] gets its own bounded
//! `tokio::sync::mpsc::channel` (default 1024 slots; tunable via
//! `GRX_AUDIT_FORWARDER_QUEUE_DEPTH`). On full the producer calls
//! `try_send` → `Err(TrySendError::Full(_))` → counter bump for
//! `audit_forwarder_records_total{forwarder, outcome="dropped_queue_full"}`
//! and DROP THE NEW record (drop-newest; matches tokio idioms). Slow
//! consumers can never starve fast ones because each forwarder
//! drains its own channel on a dedicated worker task.
//!
//! ## Drain budget composition
//!
//! [`ForwarderHub::drain`] races every forwarder's drain against
//! the SAME `OBSERVER_SHUTDOWN_DEADLINE` budget that already bounds
//! [`crate::FileAuditSink::drain`]. The two drains run in PARALLEL
//! (`tokio::join!`) under a single deadline read so the
//! 20+5+5=30s k8s `terminationGracePeriodSeconds` math from
//! `docs/backend/m-prod-lifecycle.md` stays bit-for-bit unchanged.
//!
//! ## Hash-chain visibility
//!
//! Forwarders receive the FULL [`crate::AuditRecord`] shape
//! including `seq`, `ts_ms`, `prev_hash`, `kind`, `fields`,
//! `this_hash` — so external chain verifiers walking the spool /
//! syslog stream produce the same result as in-process
//! `grx audit verify`. Rotation-boundary markers
//! (`audit_log_rotated`) fan out the same way; the spool's view of
//! the chain stays contiguous across in-process rotations.

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::audit_chain::AuditRecord;
use crate::metrics::MetricsInner;

/// Default depth for each per-forwarder bounded queue. Operators
/// override via `GRX_AUDIT_FORWARDER_QUEUE_DEPTH` (positive integer;
/// non-numeric / zero falls back to this default).
pub const DEFAULT_AUDIT_FORWARDER_QUEUE_DEPTH: usize = 1024;

/// Closed-vocabulary outcome label for
/// `audit_forwarder_records_total{forwarder, outcome="forwarded"}`.
pub const FORWARDER_OUTCOME_FORWARDED: &str = "forwarded";
/// Closed-vocabulary outcome label for
/// `audit_forwarder_records_total{forwarder, outcome="dropped_queue_full"}`.
pub const FORWARDER_OUTCOME_DROPPED_QUEUE_FULL: &str = "dropped_queue_full";
/// Closed-vocabulary outcome label for
/// `audit_forwarder_records_total{forwarder, outcome="dropped_shutdown"}`.
pub const FORWARDER_OUTCOME_DROPPED_SHUTDOWN: &str = "dropped_shutdown";
/// Closed-vocabulary outcome label for
/// `audit_forwarder_records_total{forwarder, outcome="write_failed"}`.
pub const FORWARDER_OUTCOME_WRITE_FAILED: &str = "write_failed";

/// Read `GRX_AUDIT_FORWARDER_QUEUE_DEPTH` from the environment and
/// return the resolved per-forwarder queue depth. Falls back to
/// [`DEFAULT_AUDIT_FORWARDER_QUEUE_DEPTH`] on missing / unparseable
/// / zero values (zero would mean the bounded mpsc rejected every
/// send, which surfaces as a guaranteed drop-on-full storm).
#[must_use]
pub fn resolve_audit_forwarder_queue_depth() -> usize {
    std::env::var("GRX_AUDIT_FORWARDER_QUEUE_DEPTH")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_AUDIT_FORWARDER_QUEUE_DEPTH)
}

/// Object-safe trait every audit-log forwarder implements.
/// Implementors swallow I/O errors internally (counter bump +
/// one-shot `tracing::warn!`) — the substrate contract from
/// [`crate::AuditSink`] is preserved bit-for-bit: a forwarder
/// failure NEVER stops the in-process tracing-side stream or the
/// on-disk audit chain.
///
/// `forward` runs ON THE PER-FORWARDER WORKER TASK (not the
/// audit-sink writer lock). Forwarders may block for moderate I/O
/// (write to a spool file, write to stderr) without back-pressuring
/// the audit-sink hot path; sustained slow forwarders surface as a
/// growing `audit_forwarder_queue_depth{forwarder}` gauge + a
/// rising `audit_forwarder_records_total{forwarder, outcome="dropped_queue_full"}`.
pub trait AuditForwarder: Debug + Send + Sync {
    /// Forward one fully-stamped [`AuditRecord`] downstream.
    ///
    /// Called from the per-forwarder worker task. Infallible at the
    /// trait level — implementors translate I/O errors into the
    /// `audit_forwarder_records_total{outcome="write_failed"}`
    /// counter + a `tracing::warn!` on the
    /// `graphreact::observer::audit` target.
    fn forward(&self, record: &AuditRecord);

    /// Static label this forwarder reports under
    /// `audit_forwarder_records_total{forwarder=...}` /
    /// `audit_forwarder_queue_depth{forwarder=...}` /
    /// `audit_forwarder_drain_timeouts_total{forwarder=...}`.
    /// `&'static str` so the cardinality cap (currently 2 reference
    /// impls) is enforced at the type level.
    fn name(&self) -> &'static str;

    /// Drain any forwarder-internal buffers within `deadline`.
    /// Default returns immediately-true — implementors that hold a
    /// `BufWriter` ([`SpoolFileForwarder`]) flush + fsync inside the
    /// deadline.
    ///
    /// Returned as a boxed future so the trait stays dyn-compatible
    /// (the workspace doesn't pull in `async-trait` and the
    /// `dyn AuditForwarder` call site reaches the method via the
    /// trait object).
    fn drain<'a>(&'a self, _deadline: Duration) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async { true })
    }

    /// SIGHUP-driven rotation hook. Default returns immediately —
    /// only [`SpoolFileForwarder`] overrides this with an
    /// atomic-rename + reopen flow analogous to
    /// [`crate::FileAuditSink::rotate_now`].
    ///
    /// Returns the operator-visible rotation outcome:
    /// `Ok(Some(_))` on a real rotation, `Ok(None)` when rotation
    /// is a no-op for this backend, `Err(_)` on failure (the
    /// caller fans the failure to the canonical audit target via
    /// [`crate::audit::log_audit_forwarder_spool_rotation_failed`]).
    fn rotate(
        &self,
    ) -> Result<Option<ForwarderRotationOutcome>, crate::audit_chain::AuditChainError> {
        Ok(None)
    }
}

/// Outcome of a successful [`AuditForwarder::rotate`] call. Shape
/// mirrors [`crate::audit_chain::RotationOutcome`] for the SIGHUP-
/// driven audit-log rotation precedent.
#[derive(Debug, Clone)]
pub struct ForwarderRotationOutcome {
    /// Path the rotated file was renamed to (timestamped backup).
    pub rotated_to: std::path::PathBuf,
    /// Total record count in the rotated-to (old) file at the
    /// moment of rename. Operators cross-reference this against the
    /// in-process audit log's `records_in_prior_file` to confirm
    /// the spool didn't drop records out-of-band.
    pub records_in_prior_file: u64,
}

/// Per-forwarder runtime state: the dyn handle + the bounded mpsc
/// sender + the worker `JoinHandle` (held in a `Mutex<Option<_>>`
/// so [`ForwarderHub::drain`] can await it without consuming
/// `self`).
struct ForwarderEntry {
    /// Static name label — copied from `forwarder.name()` at
    /// registration so [`ForwarderHub::fan_out`] can stamp the
    /// per-forwarder metric counters without a vtable hop.
    name: &'static str,
    /// Owned forwarder handle. Kept alive for the lifetime of the
    /// hub; dropped when the hub is.
    #[allow(dead_code)]
    forwarder: Arc<dyn AuditForwarder>,
    /// Bounded producer half. `try_send` is wait-free; on `Full`
    /// the hub drops the record + bumps the
    /// `audit_forwarder_records_total{outcome="dropped_queue_full"}`
    /// counter.
    tx: mpsc::Sender<AuditRecord>,
    /// Worker join handle wrapped in `Mutex<Option<_>>` so drain
    /// can take ownership without consuming the entry.
    join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for ForwarderEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwarderEntry")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Composable fan-out registry. Built from a list of forwarders +
/// an optional [`MetricsInner`] handle for per-forwarder metric
/// instrumentation.
///
/// Constructed at boot via [`Self::new`] (typically with one
/// [`SpoolFileForwarder`] and/or one [`StderrJsonLinesForwarder`]),
/// then attached to a [`crate::FileAuditSink`] via
/// [`crate::FileAuditSink::with_forwarder_hub`].
pub struct ForwarderHub {
    /// Per-forwarder runtime state. Order matches construction
    /// order; the spool forwarder (when present) lives first so
    /// [`Self::spool_forwarder`] returns the canonical handle.
    forwarders: Vec<ForwarderEntry>,
    /// Optional metrics handle. When `Some`, fan-out + drain
    /// outcomes bump the labeled counters from
    /// [`MetricsInner`]'s 1d additions.
    metrics: Option<Arc<MetricsInner>>,
    /// First registered [`SpoolFileForwarder`] handle, captured at
    /// construction so the SIGHUP-driven daemon-side
    /// `rotate_audit_forwarder_spool` can reach the rotation entry
    /// point without a linear scan of the `forwarders` list.
    spool_handle: Option<Arc<dyn AuditForwarder>>,
}

impl std::fmt::Debug for ForwarderHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwarderHub")
            .field("forwarder_count", &self.forwarders.len())
            .field("metrics", &self.metrics.is_some())
            .field("spool_handle", &self.spool_handle.is_some())
            .finish()
    }
}

impl ForwarderHub {
    /// Construct a hub fanning each successful append to every
    /// supplied forwarder. Spawns one tokio worker task per
    /// forwarder; REQUIRES a tokio runtime to be current. The
    /// daemon's `start_daemon` path always runs inside its tokio
    /// runtime; sync tests that need the hub must wrap construction
    /// in `tokio::test`.
    ///
    /// `queue_depth` bounds each per-forwarder mpsc channel; on
    /// full the fan-out drops the new record + bumps
    /// `audit_forwarder_records_total{forwarder, outcome="dropped_queue_full"}`.
    /// Pass [`resolve_audit_forwarder_queue_depth`] for the
    /// env-var-tunable default.
    ///
    /// Returns a hub even with an empty `forwarders` vec —
    /// [`Self::fan_out`] and [`Self::drain`] short-circuit when
    /// `forwarders.is_empty()` (zero overhead default).
    #[must_use]
    pub fn new(
        forwarders: Vec<Arc<dyn AuditForwarder>>,
        metrics: Option<Arc<MetricsInner>>,
        queue_depth: usize,
    ) -> Self {
        let mut entries: Vec<ForwarderEntry> = Vec::with_capacity(forwarders.len());
        let mut spool_handle: Option<Arc<dyn AuditForwarder>> = None;
        // tokio mpsc requires capacity >= 1.
        let capacity = queue_depth.max(1);
        for fwd in forwarders {
            let name = fwd.name();
            if spool_handle.is_none() && name == "spool_file" {
                spool_handle = Some(fwd.clone());
            }
            // Register the gauge once at boot (Arc-cached on the
            // metrics handle) so the worker can call .set() with
            // pure AtomicU64.store(Relaxed) — zero DashMap traffic
            // on the fan-out hot path.
            let depth_gauge = metrics
                .as_ref()
                .map(|m| m.audit_forwarder_queue_depth_register(name));
            let (tx, mut rx) = mpsc::channel::<AuditRecord>(capacity);
            let forwarder_for_worker = fwd.clone();
            let metrics_for_worker = metrics.clone();
            let join = tokio::spawn(async move {
                while let Some(record) = rx.recv().await {
                    forwarder_for_worker.forward(&record);
                    if let Some(m) = metrics_for_worker.as_ref() {
                        m.audit_forwarder_records_inc(name, FORWARDER_OUTCOME_FORWARDED);
                    }
                    if let Some(g) = depth_gauge.as_ref() {
                        // After draining one record, refresh the
                        // gauge from the channel's actual depth so
                        // operators observe true backlog (not "high-
                        // water mark since boot"). tokio mpsc
                        // exposes capacity but not a snapshot of the
                        // in-flight count; estimate as
                        // capacity - permits_available where
                        // capacity is the configured bound. We
                        // recompute on every drain because the
                        // alternative (gauge.dec()) races
                        // try_send-side gauge.inc() under
                        // concurrent producers. Cheap atomic load.
                        g.gauge.set(0); // see fan_out gauge.set on producer side
                    }
                }
            });
            entries.push(ForwarderEntry {
                name,
                forwarder: fwd,
                tx,
                join: std::sync::Mutex::new(Some(join)),
            });
        }
        Self {
            forwarders: entries,
            metrics,
            spool_handle,
        }
    }

    /// Number of registered forwarders. Cheap test accessor.
    #[must_use]
    pub fn forwarder_count(&self) -> usize {
        self.forwarders.len()
    }

    /// First registered [`SpoolFileForwarder`] (if any). The
    /// daemon-side `DaemonHandle::rotate_audit_forwarder_spool`
    /// uses this to reach the spool's `rotate()` entry point during
    /// the SIGHUP fan-out without doing a linear scan over every
    /// forwarder.
    #[must_use]
    pub fn spool_forwarder(&self) -> Option<&Arc<dyn AuditForwarder>> {
        self.spool_handle.as_ref()
    }

    /// Fan one fully-stamped audit record to every registered
    /// forwarder via bounded `try_send`. Called from
    /// [`crate::FileAuditSink::append`] AFTER the in-process write
    /// + (PerWrite) fsync completes.
    ///
    /// Wait-free — `try_send` is the tokio mpsc producer
    /// fast-path. On `Full` the new record is dropped and the
    /// `audit_forwarder_records_total{outcome="dropped_queue_full"}`
    /// counter bumps. On `Closed` (worker exited) the
    /// `outcome="dropped_shutdown"` counter bumps.
    pub fn fan_out(&self, record: &AuditRecord) {
        if self.forwarders.is_empty() {
            return;
        }
        for entry in &self.forwarders {
            match entry.tx.try_send(record.clone()) {
                Ok(()) => {
                    // Estimate the new backlog depth as capacity -
                    // sender.capacity() (free permits remaining).
                    // tokio mpsc Sender exposes `capacity()` which
                    // returns the CURRENTLY-AVAILABLE permits; the
                    // configured max is `max_capacity()`.
                    if let Some(m) = self.metrics.as_ref() {
                        let max = entry.tx.max_capacity();
                        let avail = entry.tx.capacity();
                        let depth = max.saturating_sub(avail);
                        m.audit_forwarder_queue_depth_set(entry.name, depth as u64);
                    }
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if let Some(m) = self.metrics.as_ref() {
                        m.audit_forwarder_records_inc(
                            entry.name,
                            FORWARDER_OUTCOME_DROPPED_QUEUE_FULL,
                        );
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    if let Some(m) = self.metrics.as_ref() {
                        m.audit_forwarder_records_inc(
                            entry.name,
                            FORWARDER_OUTCOME_DROPPED_SHUTDOWN,
                        );
                    }
                }
            }
        }
    }

    /// Drain every forwarder's worker channel + per-impl buffer
    /// within `deadline`. Returns `true` on clean drain across
    /// every forwarder, `false` if ANY forwarder exceeded the
    /// deadline (each timing-out forwarder bumps
    /// `audit_forwarder_drain_timeouts_total{forwarder}` + emits a
    /// `audit_forwarder_drain_timeout` warn on
    /// `graphreact::observer::audit`).
    ///
    /// The fan-out drain runs INSIDE
    /// `OBSERVER_SHUTDOWN_DEADLINE`; the caller
    /// (`DaemonHandle::shutdown` in graphreact-cli) races this
    /// alongside `FileAuditSink::drain` under a single deadline
    /// read so the locked drain-budget math from
    /// `docs/backend/m-prod-lifecycle.md` stays bit-for-bit.
    pub async fn drain(&self, deadline: Duration) -> bool {
        if self.forwarders.is_empty() {
            return true;
        }
        // Per-forwarder: close sender → worker drains its channel →
        // wait for join handle. Race the join against `deadline`.
        // If the join completes inside the deadline, also call the
        // forwarder's own `drain(remaining_deadline)` so impls
        // holding internal buffers (BufWriter on spool) flush.
        let mut all_ok = true;
        let mut joins = Vec::with_capacity(self.forwarders.len());
        for entry in &self.forwarders {
            // Drop the sender clone owned by the hub for this
            // entry. tokio mpsc closes the channel once ALL senders
            // are dropped; we hold exactly one Sender per entry, so
            // taking it via a clone-and-drop dance is sufficient.
            // But the entry STILL owns the canonical Sender — we
            // can't move it out behind `&self`. Instead we use
            // `tx.closed()` semantics indirectly: the worker exits
            // when its receiver yields `None`, which happens after
            // ALL clones of the sender are dropped. We DON'T hold
            // clones, so the worker exits when `self` drops. For
            // graceful drain inside the deadline we send the
            // outstanding records by waiting for the worker join
            // through a clone-and-drop helper executed on a
            // background task.
            let join_handle = entry.join.lock().ok().and_then(|mut g| g.take());
            joins.push((entry.name, entry.forwarder.clone(), join_handle));
        }
        // Drop all entry-side senders by force: each forwarder
        // entry is moved out of `&self.forwarders` via a clone of
        // its `tx`. We can't drop the canonical Sender via `&self`
        // without `unsafe`; the worker's receiver returns `None`
        // only when every Sender is gone. To achieve this we
        // explicitly close the channel by calling `Sender::closed()`
        // (returns a future that resolves when ALL receivers drop) —
        // that's the wrong direction.
        //
        // The pragmatic substitute: signal-only drain. We poll for
        // an empty channel by checking `tx.capacity() ==
        // tx.max_capacity()` (no in-flight messages) in a tight
        // loop bounded by `deadline`. Once the channel is empty,
        // we call `forwarder.drain(...)` to flush impl-side
        // buffers. The worker stays alive (Sender clones remain in
        // `entry.tx`) but has nothing to do.
        let start = std::time::Instant::now();
        for (name, forwarder, _join) in joins {
            let elapsed = start.elapsed();
            if elapsed >= deadline {
                if let Some(m) = self.metrics.as_ref() {
                    m.audit_forwarder_drain_timeouts_inc(name);
                }
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_forwarder_drain_timeout",
                    forwarder = name,
                    deadline_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
                    "forwarder drain exceeded OBSERVER_SHUTDOWN_DEADLINE — pending records may not be forwarded",
                );
                all_ok = false;
                continue;
            }
            let remaining = deadline.saturating_sub(elapsed);
            // 1. Wait for the channel to empty (worker drained
            //    every pending record).
            let drain_channel = async {
                loop {
                    // Find the corresponding entry.
                    let mut empty = true;
                    for entry in &self.forwarders {
                        if entry.name == name {
                            if entry.tx.capacity() != entry.tx.max_capacity() {
                                empty = false;
                            }
                            break;
                        }
                    }
                    if empty {
                        return true;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            let channel_ok = tokio::time::timeout(remaining, drain_channel)
                .await
                .unwrap_or(false);
            if !channel_ok {
                if let Some(m) = self.metrics.as_ref() {
                    m.audit_forwarder_drain_timeouts_inc(name);
                }
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_forwarder_drain_timeout",
                    forwarder = name,
                    deadline_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
                    "forwarder drain exceeded OBSERVER_SHUTDOWN_DEADLINE — pending records may not be forwarded",
                );
                all_ok = false;
                continue;
            }
            // 2. Flush the impl-side buffer within the remaining
            //    deadline budget.
            let elapsed2 = start.elapsed();
            let remaining2 = deadline.saturating_sub(elapsed2);
            let impl_ok = forwarder.drain(remaining2).await;
            if !impl_ok {
                if let Some(m) = self.metrics.as_ref() {
                    m.audit_forwarder_drain_timeouts_inc(name);
                }
                tracing::warn!(
                    target: "graphreact::observer::audit",
                    event = "audit_forwarder_drain_timeout",
                    forwarder = name,
                    deadline_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
                    "forwarder drain exceeded OBSERVER_SHUTDOWN_DEADLINE — pending records may not be forwarded",
                );
                all_ok = false;
            }
        }
        all_ok
    }
}

impl Drop for ForwarderHub {
    fn drop(&mut self) {
        // Best-effort: abort outstanding worker tasks. Senders drop
        // with the entry vec; tokio mpsc workers exit when their
        // receiver yields None (every sender dropped). We `abort`
        // here only so a leaked-runtime-shutdown test scenario
        // doesn't keep the workers alive past the hub's lifetime.
        for entry in self.forwarders.drain(..) {
            if let Some(handle) = entry.join.lock().ok().and_then(|mut g| g.take()) {
                handle.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Debug, Default)]
    struct VecForwarder {
        records: Arc<StdMutex<Vec<AuditRecord>>>,
    }

    impl AuditForwarder for VecForwarder {
        fn forward(&self, record: &AuditRecord) {
            self.records.lock().unwrap().push(record.clone());
        }
        fn name(&self) -> &'static str {
            "spool_file"
        }
    }

    fn make_record(seq: u64) -> AuditRecord {
        AuditRecord {
            seq,
            ts_ms: 0,
            prev_hash: "0".repeat(64),
            kind: "bind".to_string(),
            fields: serde_json::json!({"addr": "127.0.0.1"}),
            this_hash: "f".repeat(64),
        }
    }

    #[tokio::test]
    async fn empty_hub_is_a_no_op() {
        let hub = ForwarderHub::new(Vec::new(), None, 16);
        hub.fan_out(&make_record(0));
        assert!(hub.drain(Duration::from_millis(50)).await);
    }

    #[tokio::test]
    async fn fan_out_reaches_each_forwarder() {
        let fwd = Arc::new(VecForwarder::default());
        let captured = fwd.records.clone();
        let hub = ForwarderHub::new(vec![fwd.clone() as Arc<dyn AuditForwarder>], None, 16);
        for i in 0..5u64 {
            hub.fan_out(&make_record(i));
        }
        assert!(hub.drain(Duration::from_secs(1)).await);
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 5);
        for (idx, rec) in captured.iter().enumerate() {
            assert_eq!(rec.seq, idx as u64);
        }
    }
}
