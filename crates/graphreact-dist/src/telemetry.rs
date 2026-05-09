//! **M16-Backend Phase 1.** Per-crate telemetry sink slot for
//! `graphreact-dist`. Mirrors the substrate-shape pattern of
//! [`crate::reload::RemoteReloadSink`]: a clone-cheap orchestrator
//! that holds a shared callback table, lets the higher layer install a
//! sink once at `Cluster::start` time, and is cloned into every
//! per-connection task + into `route_outbound`'s closure so neither
//! path takes a lock against the parent `Cluster` instance.
//!
//! ## Why a per-crate slot (option (a) per the M16-Backend Phase-1
//! architectural decision)
//!
//! `on_remote_message` fires from the dist crate's outbound +
//! inbound paths. Both run on tokio worker threads — `route_outbound`
//! is invoked from inside `Scheduler::make_emit` (which executes on
//! the scheduler's `LocalSet` thread but the outbound mpsc handoff
//! happens to a tokio task), and `dispatch_inbound` runs on the
//! per-connection task. Rather than route those events back through
//! the scheduler (which would add a cross-thread channel hop on every
//! remote-message, costing M14's 295k+ msg/sec floor), the trait fires
//! directly through this slot.
//!
//! The user installs the same `Arc<dyn TelemetrySink>` (typically a
//! `MultiplexSink` from `graphreact-observer`) in three places —
//! `Scheduler::install_telemetry_sink`,
//! [`crate::Cluster::install_telemetry_sink`], and
//! `ReloadOrchestrator::install_telemetry_sink` — for ergonomically
//! equivalent "one consumer" registration without the cross-thread
//! tax.

use std::sync::Arc;

use arc_swap::ArcSwapOption;

use graphreact_core::{TelemetrySink, TelemetrySinkRef};

/// Per-crate sink slot. Cloned into every per-connection task at
/// `promote_connection_*` time + into `route_outbound`'s closure so
/// inbound + outbound paths fire the installed sink without
/// re-acquiring the parent `Cluster`. Replace via [`Self::install`]
/// (last writer wins).
///
/// Storage: `arc_swap::ArcSwapOption<TelemetrySinkRef>` — lock-free
/// reads (~3-5ns) without any `Arc::clone` per callsite. The
/// `TelemetrySinkRef` wrapper is required because `arc_swap` needs
/// `T: Sized` + the trait-object `Arc<dyn TelemetrySink>` is unsized.
#[derive(Clone, Default)]
pub struct TelemetrySinkSlot {
    inner: Arc<ArcSwapOption<TelemetrySinkRef>>,
}

impl TelemetrySinkSlot {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the installed sink. Re-installation is supported (last
    /// writer wins). The higher layer (typically `graphreact-cli`)
    /// calls this once after `Cluster::start` returns; in production
    /// nobody re-installs at steady state.
    pub fn install(&self, sink: Arc<dyn TelemetrySink>) {
        self.inner.store(Some(Arc::new(TelemetrySinkRef::new(sink))));
    }

    /// Drop the installed sink. After this returns, `with_sink`
    /// short-circuits to a no-op. Test surface; production teardown
    /// can also use this during graceful shutdown.
    pub fn clear(&self) {
        self.inner.store(None);
    }

    /// Run `f` against the currently-installed sink, if any. The
    /// closure receives a borrow of the `TelemetrySinkRef` (which
    /// derefs to `&dyn TelemetrySink`) so it can call any trait
    /// method without bumping the Arc strong count. Returns the
    /// closure's result, or `None` if no sink is installed.
    ///
    /// Hot-path discipline (re-stated from `graphreact_core::telemetry`):
    /// the sink method MUST NOT block + MUST NOT call back into the
    /// dist crate's mpsc machinery + MUST NOT borrow the `Cluster` /
    /// scheduler.
    #[inline]
    pub fn with_sink<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&TelemetrySinkRef) -> R,
    {
        let guard = self.inner.load();
        guard.as_ref().map(|arc| f(arc.as_ref()))
    }

    /// Test helper — `true` when a sink is installed.
    #[cfg(test)]
    #[must_use]
    pub fn has_sink(&self) -> bool {
        self.inner.load().is_some()
    }
}
