//! **M15 Phase 10.** Cluster-wide hot reload propagation.
//!
//! Mirrors the substrate-shape pattern of `RemoteSupervision`
//! plus `DistGroups`: a clone-cheap orchestrator that holds a
//! shared callback table, lets the higher layer install a sink
//! once at `Cluster::start` time, and is cloned into every
//! per-connection task so the inbound dispatch path can fire
//! the sink without taking out a lock against the parent
//! `Cluster` instance.
//!
//! ## Layering
//!
//! `graphreact-dist` is intentionally parser-agnostic — it
//! shuttles `Vec<u8>` payloads (JSON-encoded
//! `graphreact_grx_parser::ir::GrxIr`; mirrors `Message.payload`'s
//! outer-postcard / inner-JSON split) without decoding them.
//! The decode + classify + apply pipeline runs at the
//! `graphreact-cli` layer, which installs a `RemoteReloadSink`
//! callback that owns those concerns. This keeps the dist
//! crate orthogonal to parser / runtime decisions and matches
//! the dependency direction of the rest of the workspace
//! (`graphreact-cli` → `graphreact-dist`, never the other way).

use std::sync::Arc;

use parking_lot::RwLock;

/// Callback fired on inbound `Packet::ReloadModule`. Arguments:
///
/// - `origin_peer` — authenticated name of the peer that shipped
///   the reload (rewritten authoritatively from the connection's
///   handshake-verified `NodeId`, NOT from the wire's `origin`
///   field — same forgery-rejection shape as `Message`).
/// - `file_path` — the origin's canonical path. Treated as opaque
///   by the receiver in v1 (do NOT re-read from local disk).
/// - `ir_payload` — JSON-encoded `GrxIr` bytes (`serde_json::to_vec`).
/// - `ir_hash` — SHA-256 over `ir_payload`. Lets the callback
///   surface a tracing warning on mismatch (mismatch is non-
///   fatal in v1; logged, then we continue applying).
/// - `version` — the origin's monotonic per-origin reload counter.
///
/// Return value is reserved for a future reverse-channel where
/// peer outcomes flow back to the origin; in v1 the origin
/// doesn't read this and we log the outcome unconditionally.
pub type RemoteReloadCallback =
    Arc<dyn Fn(&str, &str, &[u8], &[u8; 32], u64) -> bool + Send + Sync>;

/// Cluster-side reload sink. Cloned into every per-connection
/// task at `promote_connection_*` time, so inbound dispatch
/// fires the installed callback without re-acquiring the
/// parent `Cluster`.
#[derive(Clone, Default)]
pub struct RemoteReloadSink {
    inner: Arc<RwLock<Option<RemoteReloadCallback>>>,
}

impl RemoteReloadSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the installed callback. The higher layer
    /// (`graphreact-cli`) calls this once after `Cluster::start`
    /// returns. Re-installation is supported (last writer wins);
    /// in practice nobody should re-install during steady state.
    pub fn install(&self, callback: RemoteReloadCallback) {
        *self.inner.write() = Some(callback);
    }

    /// Fire the installed callback. Returns `false` if no
    /// callback is installed (silently drops the reload + logs
    /// a warning so the operator knows their cluster lost a
    /// fan-out).
    pub fn dispatch(
        &self,
        origin_peer: &str,
        file_path: &str,
        ir_payload: &[u8],
        ir_hash: &[u8; 32],
        version: u64,
    ) -> bool {
        let guard = self.inner.read();
        if let Some(cb) = guard.as_ref() {
            cb(origin_peer, file_path, ir_payload, ir_hash, version)
        } else {
            tracing::warn!(
                "dist: ReloadModule from `{origin_peer}` (file `{file_path}`, \
                 version {version}, {} bytes) dropped — no reload sink \
                 installed (call `Cluster::install_remote_reload_sink` \
                 from your watch driver)",
                ir_payload.len()
            );
            false
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn has_callback(&self) -> bool {
        self.inner.read().is_some()
    }
}

/// Per-peer outcome from a `Cluster::reload_module` fan-out.
///
/// `Shipped` is the success path; the other variants are the
/// reasons we may NOT have shipped to a given peer. The plan-
/// committed v1 behaviour is "skip cleanly with a log", never
/// "abort the whole reload" — graceful mixed-version operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerReloadOutcome {
    /// Successfully enqueued onto the peer's outbound channel.
    Shipped,
    /// Peer hasn't advertised the `"reload-v1"` capability.
    /// Plan-locked behaviour: skip + warn, do NOT attempt to
    /// ship (would trip `UnknownPacketType` on the peer side).
    SkippedNoCapability,
    /// M14.10 partition-state filter — peer is `Suspect` or
    /// `Partitioned` per the tick loop. Reloads only target
    /// `Healthy` peers; suspect peers may receive the reload
    /// once they recover (the application-level retry is the
    /// caller's job in v1).
    SkippedNonHealthy,
    /// Peer's outbound mpsc channel was full (backpressure) or
    /// closed (race with disconnect). Counted separately from
    /// `SkippedNoCapability` because this is a runtime failure,
    /// not a capability-negotiation outcome.
    SendFailed,
}

/// Aggregate outcome of a `Cluster::reload_module` fan-out.
/// Returned synchronously — actual peer apply is asynchronous
/// and happens on the receiving side. M-Prod's reverse channel
/// for peer apply confirmations is a future deliverable.
#[derive(Debug, Clone, Default)]
pub struct ClusterReloadOutcome {
    pub shipped: Vec<String>,
    pub skipped_no_capability: Vec<String>,
    pub skipped_non_healthy: Vec<String>,
    pub send_failed: Vec<String>,
}

impl ClusterReloadOutcome {
    /// Convenience: total peer count we considered (shipped +
    /// every skipped variant).
    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.shipped.len()
            + self.skipped_no_capability.len()
            + self.skipped_non_healthy.len()
            + self.send_failed.len()
    }

    /// Convenience: did every peer we considered get a packet
    /// shipped? (`true` for both "shipped to all" + "no peers
    /// were registered".)
    #[must_use]
    pub fn all_shipped(&self) -> bool {
        self.skipped_no_capability.is_empty()
            && self.skipped_non_healthy.is_empty()
            && self.send_failed.is_empty()
    }
}

/// **M15 Phase 10.** The capability string a node advertises to
/// say "I accept `Packet::ReloadModule`". Single source of
/// truth — both the origin's filter logic + the application
/// layer's `ClusterConfig.capabilities` use this constant.
pub const RELOAD_V1_CAPABILITY: &str = "reload-v1";
