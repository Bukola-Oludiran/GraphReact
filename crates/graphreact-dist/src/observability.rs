//! **M16-Backend Phase 10.** Observability substrate for cross-node
//! distributed tracing. Lives in `graphreact-dist` (NOT in
//! `graphreact-observer`) because the wire-tracing hook fires from
//! the dist outbound serialization path; observer does not depend on
//! dist (the layer ordering is observer = consumer of dist's slots,
//! not the other way around). Mirror of [`crate::reload`]'s
//! [`crate::RELOAD_V1_CAPABILITY`] for hot-reload propagation.
//!
//! ## Capability negotiation
//!
//! Peers advertise [`OBSERVER_OTEL_V1_CAPABILITY`] in their
//! [`crate::wire::Hello::capabilities`] /
//! [`crate::wire::ChallengeAck::server_capabilities`] when they're
//! configured with an OTel sink and want their outbound remote
//! messages to carry trace context. Receivers do NOT need the
//! capability advertised to read inbound trace context; the wire
//! format change to v3 made the field mandatorily readable. The
//! capability acts as a hint to the SENDER's hook installation —
//! when both peers have it, cross-node spans render as connected
//! traces collector-side.
//!
//! ## Wire-format dependency
//!
//! Trace context propagation REQUIRES `WIRE_VERSION = 3` (the
//! [`crate::wire::Message::trace_context`] field landed at the same
//! time as the version bump from M15's `WIRE_VERSION = 2`). This is
//! a hard floor — peers running v2 cannot interop with v3 peers
//! (per the existing `MIN_SUPPORTED_VERSION == WIRE_VERSION`
//! invariant; M-Prod owns rolling-version negotiation).

use std::sync::Arc;

use parking_lot::RwLock;

/// **M16-Backend Phase 10.** Capability flag a node advertises in
/// its [`crate::wire::Hello`] / [`crate::wire::ChallengeAck`] to say
/// "I'm running an OTel sink and will populate
/// [`crate::wire::Message::trace_context`] on my outbound remote
/// messages". Peer-pair-level: when both ends advertise it, the
/// cross-cluster trace renders as a connected DAG in the collector.
/// When only the sender advertises it, the receiver still reads the
/// trace context (wire-format-mandatorily) but doesn't propagate it
/// further — useful for one-direction monitoring.
///
/// **String must stay in sync** with `graphreact_observer::OBSERVER_OTEL_V1`.
/// `graphreact-dist` cannot depend on `graphreact-observer` (layering
/// inversion); the canonical assertion that they match lives in the
/// observer crate's test suite at
/// `tests/observer_otel_capability_constant_matches.rs`.
pub const OBSERVER_OTEL_V1_CAPABILITY: &str = "observer-otel-v1";

/// **M-Ingress phase 2.** Capability flag a node advertises in its
/// [`crate::wire::Hello`] / [`crate::wire::ChallengeAck`] to say
/// "I understand the [`crate::wire::Message::correlation_id`] field
/// — propagate it on outbound and consume it on inbound". Peer-pair-
/// level: when both ends advertise it, the
/// `POST /v1/nodes/:id/call` flow can target a remote node and
/// have its reply route back through the call origin's
/// `PendingCalls` registry. When only one side advertises it, the
/// non-advertising side strips correlation_id on outbound (silently
/// — the reply will time out at the call origin) and ignores it on
/// inbound (the call origin's `PendingCalls` lookup never resolves).
///
/// This is the same shape as
/// [`OBSERVER_OTEL_V1_CAPABILITY`]: hard wire-version floor at the
/// `WIRE_VERSION` boundary (v3 ↔ v4 are not interop-compatible per
/// the `MIN_SUPPORTED_VERSION == WIRE_VERSION` invariant), then
/// in-version capability gating for the new feature so two v4
/// peers running different daemon configurations can coexist
/// gracefully.
///
/// **String stability** — pinned by the
/// `ingress_call_v1_capability_string_is_ingress_call_v1` test
/// below. Higher-level crates (e.g. `graphreact-cli`'s daemon
/// capability advertisement) depend on this exact byte sequence.
pub const INGRESS_CALL_V1_CAPABILITY: &str = "ingress-call-v1";

/// **M16-Backend Phase 10.** Trait the observer crate's `OtelSink`
/// implements to provide the dist outbound path with the current
/// thread's active OTel span context. Called once per outbound
/// remote message from [`crate::cluster::Cluster::route_outbound`]
/// (or wherever the outbound serialization fires); returned bytes
/// land in [`crate::wire::Message::trace_context`].
///
/// Lives in dist (not observer) so the dist outbound code can call
/// the trait method without depending on observer. The OtelSink
/// implementation in observer attaches via
/// [`crate::Cluster::install_wire_tracing_hook`] mirroring the
/// existing `Cluster::install_telemetry_sink` /
/// `install_remote_supervision_hook` slot patterns.
///
/// **Hot-path discipline**: implementations MUST NOT block, MUST NOT
/// allocate beyond the 24-byte return value, and MUST be `Send +
/// Sync` for use across the dist outbound task pool.
pub trait WireTracingHook: Send + Sync {
    /// Return the current thread's active OTel span context as a
    /// 24-byte `TraceID(16) || SpanID(8)` blob. Returns `None` if no
    /// span is active on the current thread (which is the common
    /// case for non-OTel deployments + for messages emitted from
    /// non-scheduler threads).
    fn extract_outbound(&self) -> Option<[u8; 24]>;
}

/// **M16-Backend Phase 10.** Synchronization slot for a
/// [`WireTracingHook`] installed on a [`crate::Cluster`]. Mirrors
/// [`crate::TelemetrySinkSlot`]'s `RwLock<Option<Arc<dyn ...>>>`
/// pattern. The dist outbound path acquires a read guard, queries
/// `extract_outbound()`, releases the guard before serialization.
#[derive(Default, Clone)]
pub struct WireTracingHookSlot {
    inner: Arc<RwLock<Option<Arc<dyn WireTracingHook>>>>,
}

impl WireTracingHookSlot {
    /// Install or replace the hook. Safe to call at any time; the
    /// outbound path picks up the new hook on its next acquire.
    pub fn install(&self, hook: Arc<dyn WireTracingHook>) {
        *self.inner.write() = Some(hook);
    }

    /// Query the current hook for its outbound trace context. Returns
    /// `None` if no hook is installed OR the hook returns `None` for
    /// the current thread.
    #[must_use]
    pub fn extract_outbound(&self) -> Option<[u8; 24]> {
        self.inner.read().as_ref().and_then(|h| h.extract_outbound())
    }

    /// Whether a hook is currently installed. Useful for debug
    /// instrumentation; not load-bearing for outbound population.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.inner.read().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `OBSERVER_OTEL_V1_CAPABILITY` is the string handshake peers
    /// match against — pin against typo regression.
    #[test]
    fn observer_otel_v1_capability_string_is_observer_otel_v1() {
        assert_eq!(OBSERVER_OTEL_V1_CAPABILITY, "observer-otel-v1");
    }

    /// **M-Ingress phase 2.** Same shape — pin the literal so a
    /// rename on one side surfaces immediately as a test failure.
    #[test]
    fn ingress_call_v1_capability_string_is_ingress_call_v1() {
        assert_eq!(INGRESS_CALL_V1_CAPABILITY, "ingress-call-v1");
    }

    /// A freshly-constructed slot is empty and returns None for both
    /// `is_installed` and `extract_outbound`.
    #[test]
    fn empty_slot_returns_none() {
        let slot = WireTracingHookSlot::default();
        assert!(!slot.is_installed());
        assert!(slot.extract_outbound().is_none());
    }

    /// Install + query round-trip with a fixture hook returning a
    /// fixed byte pattern.
    #[test]
    fn installed_hook_extract_round_trips() {
        struct FixtureHook;
        impl WireTracingHook for FixtureHook {
            fn extract_outbound(&self) -> Option<[u8; 24]> {
                let mut buf = [0u8; 24];
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = u8::try_from(i).unwrap_or(0).wrapping_add(1);
                }
                Some(buf)
            }
        }
        let slot = WireTracingHookSlot::default();
        slot.install(Arc::new(FixtureHook));
        assert!(slot.is_installed());
        let got = slot.extract_outbound().expect("installed hook returns Some");
        assert_eq!(got[0], 1);
        assert_eq!(got[23], 24);
    }

    /// Re-installing a hook replaces the previous installation.
    #[test]
    fn reinstall_replaces_prior_hook() {
        struct ZeroHook;
        impl WireTracingHook for ZeroHook {
            fn extract_outbound(&self) -> Option<[u8; 24]> {
                Some([0; 24])
            }
        }
        struct OneHook;
        impl WireTracingHook for OneHook {
            fn extract_outbound(&self) -> Option<[u8; 24]> {
                Some([1; 24])
            }
        }
        let slot = WireTracingHookSlot::default();
        slot.install(Arc::new(ZeroHook));
        assert_eq!(slot.extract_outbound().unwrap()[0], 0);
        slot.install(Arc::new(OneHook));
        assert_eq!(slot.extract_outbound().unwrap()[0], 1);
    }

    /// Hook returning None propagates None through the slot.
    #[test]
    fn none_returning_hook_propagates_none() {
        struct NoneHook;
        impl WireTracingHook for NoneHook {
            fn extract_outbound(&self) -> Option<[u8; 24]> {
                None
            }
        }
        let slot = WireTracingHookSlot::default();
        slot.install(Arc::new(NoneHook));
        assert!(slot.is_installed());
        assert!(slot.extract_outbound().is_none());
    }
}
