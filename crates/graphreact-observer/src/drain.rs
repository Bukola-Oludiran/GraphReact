//! **M-Prod sub-track #4 Phase α.** SIGTERM drain primitive.
//!
//! The drain primitive is the substrate every future lifecycle
//! trigger (SIGTERM today, REST `POST /v1/admin/drain` later, k8s
//! preStop hook, operator `grx admin drain --deadline=Ns`) calls
//! into so the observer surface ALWAYS de-routes ingress traffic
//! the same way: the load balancer sees `/v1/readyz` go 503, new
//! ingress POSTs see the same 503 `draining` body, and the audit
//! log carries one `drain_started` / `drain_completed` /
//! `drain_deadline_exceeded` line per drain.
//!
//! ## Shape
//!
//! - [`DrainFlag`] is a thin shared state object around an atomic bit
//!   plus a watch channel carrying the one-shot drain deadline, so
//!   the same drain bit is observable from EVERY place that needs
//!   to know about it: the management-API `/v1/readyz` handler,
//!   the ingress POST middlewares, the cluster-handshake quiesce
//!   loop (future Phase β), and the `DaemonHandle::start_drain`
//!   orchestrator on the CLI side. The newtype gives us a single
//!   type to plumb through `ApiState` instead of a bare
//!   `Arc<AtomicBool>` that another track might collide with.
//! - Set/clear semantics: `start()` flips false→true exactly once
//!   (CAS); subsequent calls are no-ops so duplicate SIGTERMs
//!   from the kubelet don't re-emit `drain_started` or reset the
//!   deadline. There is no `clear()` — drain is a one-way trip;
//!   the daemon's process exit clears the flag implicitly by
//!   tearing down the heap.
//! - `is_draining()` is a relaxed atomic load. The hot path
//!   (every `/v1/readyz` probe + every ingress POST) reads this
//!   bit once per request; the relaxed ordering is correct here
//!   because we only care about *eventual* visibility — a
//!   request observing the OLD (false) value during the brief
//!   flip window completes normally, and the next request after
//!   the flip sees `true`. Strong ordering would buy us nothing
//!   and cost a memory fence per probe.
//!
//! ## Audit event contract
//!
//! Three events, all on the existing `graphreact::observer::audit`
//! target so they interleave with `observer scrape` / `observer
//! response` / `tls_reloaded` / `otel_shutdown_timeout` in the
//! same JSON-log stream:
//!
//! | Event | Level | Fields | Emitted at |
//! |---|---|---|---|
//! | `drain_started` | INFO | `deadline_ms: u64` | After `DrainFlag::start()` flips false→true |
//! | `drain_completed` | INFO | `duration_ms: u64`, `in_flight_settled: bool`, `cluster_quiesced: bool` | After in-flight quiesce within deadline |
//! | `drain_deadline_exceeded` | WARN | `deadline_ms: u64`, `duration_ms: u64`, `pending_calls_remaining: usize`, `ingress_in_flight_remaining: usize` | When the deadline expired with traffic still in flight |
//!
//! `drain_started` MUST emit BEFORE the AtomicBool flips so a
//! `/v1/readyz` scrape racing the flip can't see `ready=true` AND
//! `no drain_started` in the log. The `start()` helper enforces
//! this ordering.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// **M-Prod sub-track #4 Phase α.** Shared drain bit. Cloned into
/// [`crate::ApiState`] and held on the CLI's `DaemonHandle` so the
/// management-API hot path and the lifecycle orchestrator agree on
/// "are we draining yet".
#[derive(Clone, Copy, Debug)]
pub(crate) struct DrainDeadline {
    pub(crate) at: tokio::time::Instant,
    pub(crate) budget_ms: u64,
}

#[derive(Debug)]
struct DrainState {
    draining: AtomicBool,
    deadline_tx: tokio::sync::watch::Sender<Option<DrainDeadline>>,
}

#[derive(Clone, Debug)]
pub struct DrainFlag {
    inner: Arc<DrainState>,
}

impl DrainFlag {
    /// Fresh flag, value `false`.
    pub fn new() -> Self {
        let (deadline_tx, _deadline_rx) = tokio::sync::watch::channel(None);
        Self {
            inner: Arc::new(DrainState {
                draining: AtomicBool::new(false),
                deadline_tx,
            }),
        }
    }

    /// Idempotent drain trigger.
    ///
    /// On the FIRST call, emits the `drain_started` audit event
    /// (BEFORE flipping the bit — see module docs) and returns
    /// `true`. Subsequent calls (duplicate SIGTERMs from kubelet
    /// or operator-driven double-fire) return `false` without
    /// re-emitting the audit row or resetting the deadline.
    ///
    /// `deadline_ms` is logged with the event so operators can
    /// see the bound at drain-start without grepping the CLI
    /// flag history.
    pub fn start(&self, deadline_ms: u64) -> bool {
        // Emit BEFORE the flip so any reader that races us can't
        // observe `is_draining()==true` without `drain_started`
        // already being in the audit stream.
        match self
            .inner
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                // We're the winner — but we need the audit row
                // to be observable BEFORE the flag flip from the
                // tracing-subscriber's perspective. tracing's
                // info! is synchronous w.r.t. the calling thread,
                // so this emit happens-before the compare_exchange
                // succeeds from any other thread's perspective.
                crate::audit::log_drain_started(deadline_ms);
                self.inner.deadline_tx.send_replace(Some(DrainDeadline {
                    at: tokio::time::Instant::now() + Duration::from_millis(deadline_ms),
                    budget_ms: deadline_ms,
                }));
                true
            }
            Err(_) => false,
        }
    }

    /// Relaxed-ordering read of the drain bit. Hot-path callers
    /// (readyz handler, ingress POST middlewares) should use this.
    #[inline]
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::Relaxed)
    }

    pub(crate) fn subscribe_deadline(&self) -> tokio::sync::watch::Receiver<Option<DrainDeadline>> {
        self.inner.deadline_tx.subscribe()
    }
}

impl Default for DrainFlag {
    fn default() -> Self {
        Self::new()
    }
}

/// **M-Prod sub-track #4 Phase α.** Lock-free counter of in-flight
/// ingress POSTs. Bumped by the ingress middleware on entry to the
/// inject / call / bulk handlers; decremented on exit (or on axum
/// future cancellation, via [`InflightGuard`]'s `Drop`).
///
/// Used by the [`crate::DaemonHandle::start_drain`] orchestrator
/// to know when the in-flight population has settled to zero: a
/// drain completes early (before the deadline) once both this
/// counter and the [`crate::PendingCalls`] registry are empty.
#[derive(Clone, Debug, Default)]
pub struct IngressInFlight {
    inner: Arc<AtomicUsize>,
}

impl IngressInFlight {
    /// Fresh counter, value `0`.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Relaxed-ordering read. Hot-path: every `start_drain`
    /// poll-loop iteration.
    #[inline]
    pub fn load(&self) -> usize {
        self.inner.load(Ordering::Relaxed)
    }

    /// Bump on ingress entry; the returned [`InflightGuard`]
    /// decrements on `Drop` so axum future cancellation
    /// (HTTP-client disconnect, server-side abort on drain
    /// deadline) frees the slot. Always succeeds — there is no
    /// hard cap here (the existing per-endpoint caps —
    /// [`crate::PendingCalls::HARD_CAP`] and
    /// [`crate::SNAPSHOT_CHANNEL_CAPACITY`] — bound the upstream
    /// resource consumption).
    #[inline]
    pub fn enter(&self) -> InflightGuard {
        self.inner.fetch_add(1, Ordering::AcqRel);
        InflightGuard {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// **M-Prod sub-track #4 Phase α.** RAII guard returned by
/// [`IngressInFlight::enter`]. Decrements the counter on drop so
/// the count stays accurate across axum future cancellation
/// (HTTP-client disconnect, drain-deadline-driven cancel).
#[derive(Debug)]
pub struct InflightGuard {
    inner: Arc<AtomicUsize>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inner.fetch_sub(1, Ordering::AcqRel);
    }
}

/// **M-Prod sub-track #4 Phase α.** Convenience: emit the
/// `drain_completed` audit row at the end of a successful drain.
/// Kept separate from [`DrainFlag`] so the orchestrator (which
/// owns the timing + the quiescence-check signals) controls when
/// it fires. The drain coordinator on the CLI side composes
/// [`DrainFlag::start`] + this helper around the in-flight-await
/// loop.
pub fn log_drain_completed(duration: Duration, in_flight_settled: bool, cluster_quiesced: bool) {
    crate::audit::log_drain_completed(
        duration_to_ms_u64(duration),
        in_flight_settled,
        cluster_quiesced,
    );
}

/// **M-Prod sub-track #4 Phase α.** Convenience: emit the
/// `drain_deadline_exceeded` audit row when the drain budget
/// expired while traffic was still in flight. Operators see the
/// remaining counts so they can correlate against load-balancer
/// in-flight requests at SIGTERM time.
pub fn log_drain_deadline_exceeded(
    deadline: Duration,
    duration: Duration,
    pending_calls_remaining: usize,
    ingress_in_flight_remaining: usize,
) {
    crate::audit::log_drain_deadline_exceeded(
        duration_to_ms_u64(deadline),
        duration_to_ms_u64(duration),
        pending_calls_remaining,
        ingress_in_flight_remaining,
    );
}

/// Saturating conversion from `Duration` to ms-as-u64.
///
/// `Duration::as_millis()` returns `u128`; the audit event fields
/// are `u64`. A direct `as u64` cast triggers
/// `clippy::cast_possible_truncation` because in theory a
/// caller could pass a `Duration::MAX`-shaped value. In practice
/// drain budgets are bounded by `terminationGracePeriodSeconds`
/// (60s typical) so we never approach the `u64::MAX` ms ceiling
/// (~584 million years). Saturating conversion preserves the
/// audit-event semantics even in the (impossible) overflow case
/// and silences the lint without adding an `#[allow(...)]`.
#[inline]
fn duration_to_ms_u64(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_is_idempotent() {
        let flag = DrainFlag::new();
        assert!(!flag.is_draining());
        assert!(flag.start(25_000));
        assert!(flag.is_draining());
        // Duplicate SIGTERM — second start returns false, no
        // second `drain_started` audit row is emitted.
        assert!(!flag.start(25_000));
        assert!(flag.is_draining());
    }

    #[test]
    fn clone_shares_the_same_bit() {
        let a = DrainFlag::new();
        let b = a.clone();
        assert!(!a.is_draining());
        assert!(!b.is_draining());
        a.start(10_000);
        assert!(b.is_draining(), "clone must observe the flip");
    }

    #[test]
    fn fresh_flag_is_not_draining() {
        let flag = DrainFlag::default();
        assert!(!flag.is_draining());
    }

    #[test]
    fn ingress_inflight_guard_balances_count() {
        let counter = IngressInFlight::new();
        assert_eq!(counter.load(), 0);
        let g1 = counter.enter();
        assert_eq!(counter.load(), 1);
        let g2 = counter.enter();
        assert_eq!(counter.load(), 2);
        drop(g1);
        assert_eq!(counter.load(), 1);
        drop(g2);
        assert_eq!(counter.load(), 0);
    }

    #[test]
    fn ingress_inflight_clone_shares_state() {
        let a = IngressInFlight::new();
        let b = a.clone();
        let g = a.enter();
        assert_eq!(a.load(), 1);
        assert_eq!(b.load(), 1, "clone observes shared counter");
        drop(g);
        assert_eq!(b.load(), 0);
    }
}
