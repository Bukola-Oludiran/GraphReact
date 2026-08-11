//! Live peer registry.
//!
//! `NodeRegistry` holds one `PeerHandle` per connected NodeId. Each
//! handle bundles:
//!
//! - The peer's `NodeId` + `Creation` (post-handshake identity).
//! - An mpsc sender into the per-connection writer task — outbound
//!   packets are pushed here from the M14.4 `DistRouter`.
//!
//! Future fields land in subsequent sub-phases:
//! - `last_tick: AtomicU64` — M14.7's `PartitionDetector` consults it.
//! - `monitors: Mutex<Vec<RemoteMonitorRef>>` — M14.5's per-node
//!   index for O(1) disconnect fan-out.
//! - `synthesized: AtomicBool` — M14.5's idempotent `__DOWN__`
//!   firing across reconnect-then-disconnect.
//!
//! `DashMap` is used so peer table reads (the hot path for
//! outbound routing) don't bottleneck on a single mutex. Writes are
//! cluster-membership events (handshake completion, disconnect),
//! which are infrequent.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::node_id::{Creation, NodeId};
use crate::wire::Packet;
use crate::DistError;

/// Per-peer state held in [`NodeRegistry`].
pub struct PeerHandle {
    pub node: NodeId,
    pub creation: Creation,
    /// Writer task for this peer. Send a `Packet` here to dispatch
    /// it on the wire. Bounded — backpressure on a slow peer
    /// surfaces as `Sender::send` returning a `SendError`.
    pub send_tx: mpsc::Sender<Packet>,
    /// **M14.7.** Wall-clock millisecond timestamp of the most
    /// recently received packet from this peer (any packet —
    /// `Tick` traffic provides the explicit liveness signal,
    /// data packets provide an implicit one). Wrapped in an
    /// `Arc` so `run_connection` can hold a clone WITHOUT
    /// keeping `send_tx` alive — that lets
    /// [`NodeRegistry::remove`] actually close the channel
    /// (otherwise `send_rx.recv()` would never return `None`
    /// on shutdown). Updated by `dispatch_inbound`; read by
    /// the cluster-wide partition-detector loop.
    pub last_tick: Arc<AtomicU64>,
    /// **M15 Phase 10 / wire v2.** Capabilities advertised by
    /// this peer on its `Hello` (server-side) or echoed via
    /// `ChallengeAck.server_capabilities` (client-side). Used by
    /// capability-gated dispatch (e.g. `Cluster::reload_module`
    /// only ships to peers with `"reload-v1"` listed).
    pub capabilities: Vec<String>,
    /// **Phase β (sub-track #4).** Effective (negotiated) substrate
    /// capability bitmap — bitwise AND of our local bitmap and the
    /// peer's advertised bitmap, computed by
    /// [`crate::handshake::server_handshake`] /
    /// [`crate::handshake::client_handshake`]. v4 peers without a
    /// wire-level bitmap decode to `0` via the codec fallback path,
    /// so v4-with-v5 pairings always see `0` here. Phase β only
    /// EXPOSES the value (via
    /// [`crate::Cluster::peer_effective_capabilities`]); no
    /// dispatch path consumes it yet (Phase γ consumer).
    pub effective_capabilities: u64,
}

impl PeerHandle {
    /// Construct a fresh handle. Initialises `last_tick` to "now"
    /// so the partition detector doesn't false-positive on the
    /// first tick interval after handshake.
    #[must_use]
    pub fn new(
        node: NodeId,
        creation: Creation,
        send_tx: mpsc::Sender<Packet>,
        capabilities: Vec<String>,
        effective_capabilities: u64,
    ) -> Self {
        Self {
            node,
            creation,
            send_tx,
            last_tick: Arc::new(AtomicU64::new(now_ms())),
            capabilities,
            effective_capabilities,
        }
    }

    /// Bump `last_tick` to the current wall-clock. Cheap —
    /// single relaxed atomic store. Called from
    /// `dispatch_inbound` on every inbound packet.
    pub fn touch_last_tick(&self) {
        self.last_tick.store(now_ms(), Ordering::Relaxed);
    }

    /// Read the cached last-tick timestamp.
    #[must_use]
    pub fn last_tick_ms(&self) -> u64 {
        self.last_tick.load(Ordering::Relaxed)
    }
}

#[inline]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl std::fmt::Debug for PeerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerHandle")
            .field("node", &self.node)
            .field("creation", &self.creation)
            .field("send_tx", &"<mpsc::Sender<Packet>>")
            .field("last_tick_ms", &self.last_tick_ms())
            .finish()
    }
}

/// Live peer table. Cheap to clone (`Arc<DashMap>`-shaped).
///
/// **Keyed by node *name*** (the `name` part of `name@host`), not
/// the full `NodeId`. Cluster names are cluster-unique by
/// convention (matches OTP semantics: two nodes can't share a name
/// in the same cluster), so the name is the canonical routing
/// identity. The full `NodeId` lives in `PeerHandle` for
/// diagnostics and reconnection.
#[derive(Debug, Clone, Default)]
pub struct NodeRegistry {
    peers: Arc<DashMap<String, Arc<PeerHandle>>>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a fresh handshake-completed peer. Returns
    /// [`DistError::DuplicatePeer`] if the node *name* is already
    /// registered — the caller (M14.4 `DistRouter`) is expected to
    /// resolve via [`crate::handshake::should_keep_outbound`] before
    /// re-attempting.
    pub fn insert(&self, handle: Arc<PeerHandle>) -> Result<(), DistError> {
        let name = handle.node.name().to_string();
        if self.peers.contains_key(&name) {
            return Err(DistError::DuplicatePeer {
                node: handle.node.to_string(),
            });
        }
        self.peers.insert(name, handle);
        Ok(())
    }

    /// Read a peer handle by full `NodeId`. `O(1)` average via the
    /// internal name-keyed `DashMap`.
    pub fn get(&self, node: &NodeId) -> Option<Arc<PeerHandle>> {
        self.get_by_name(node.name())
    }

    /// Read a peer handle by node *name* — the routing-fast-path
    /// shape used by the dist router's outbound sink. Most callsites
    /// only have the bare name (parsed out of `msg.to`'s `@<name>`
    /// suffix), so this is the common entry point.
    pub fn get_by_name(&self, name: &str) -> Option<Arc<PeerHandle>> {
        self.peers.get(name).map(|r| Arc::clone(r.value()))
    }

    /// Remove a peer by full `NodeId`. Called on graceful disconnect
    /// or on tick-timeout (M14.7). Returns the handle so M14.5's
    /// disconnect-synthesis path can drain the per-peer monitor
    /// index before the handle is dropped.
    pub fn remove(&self, node: &NodeId) -> Option<Arc<PeerHandle>> {
        self.remove_by_name(node.name())
    }

    /// Remove a peer by node *name*. Symmetric to
    /// [`Self::get_by_name`].
    pub fn remove_by_name(&self, name: &str) -> Option<Arc<PeerHandle>> {
        self.peers.remove(name).map(|(_, h)| h)
    }

    /// Snapshot the current peer set. Caller-side use cases:
    /// `grx cluster status`, `pg` gossip target list.
    pub fn list(&self) -> Vec<NodeId> {
        self.peers.iter().map(|r| r.value().node.clone()).collect()
    }

    /// **M15 Phase 10.** Snapshot the current peer set as
    /// `Arc<PeerHandle>` clones — gives capability-gated
    /// dispatch (e.g. `Cluster::reload_module`) a stable view
    /// that doesn't require holding any DashMap reference
    /// across the per-peer iteration.
    pub fn snapshot_handles(&self) -> Vec<Arc<PeerHandle>> {
        self.peers.iter().map(|r| Arc::clone(r.value())).collect()
    }

    /// Number of currently-connected peers. O(1).
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> NodeId {
        s.parse().unwrap()
    }

    fn make_handle(node: &str, creation: u32) -> Arc<PeerHandle> {
        let (tx, _rx) = mpsc::channel(8);
        Arc::new(PeerHandle::new(
            id(node),
            Creation(creation),
            tx,
            Vec::new(),
            0,
        ))
    }

    #[test]
    fn insert_get_remove_round_trip() {
        let r = NodeRegistry::new();
        assert!(r.is_empty());
        let h = make_handle("alice@host", 1);
        r.insert(h.clone()).unwrap();
        assert_eq!(r.len(), 1);
        let got = r.get(&id("alice@host")).expect("get");
        assert_eq!(got.node.to_string(), "alice@host");
        assert_eq!(got.creation.0, 1);
        let removed = r.remove(&id("alice@host")).expect("remove");
        assert_eq!(removed.node.to_string(), "alice@host");
        assert!(r.is_empty());
        assert!(r.get(&id("alice@host")).is_none());
    }

    #[test]
    fn duplicate_insert_rejected() {
        let r = NodeRegistry::new();
        r.insert(make_handle("alice@host", 1)).unwrap();
        let err = r.insert(make_handle("alice@host", 2)).unwrap_err();
        match err {
            DistError::DuplicatePeer { node } => assert_eq!(node, "alice@host"),
            other => panic!("expected DuplicatePeer, got {other:?}"),
        }
        // Original entry is preserved; the duplicate insert didn't
        // overwrite.
        let got = r.get(&id("alice@host")).expect("get");
        assert_eq!(got.creation.0, 1);
    }

    #[test]
    fn list_returns_all_peers() {
        let r = NodeRegistry::new();
        r.insert(make_handle("alice@host", 1)).unwrap();
        r.insert(make_handle("bob@host", 2)).unwrap();
        r.insert(make_handle("carol@host", 3)).unwrap();
        let mut names: Vec<String> = r.list().into_iter().map(|n| n.to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["alice@host", "bob@host", "carol@host"]);
    }

    #[test]
    fn registry_clone_shares_state() {
        // The registry must be cheap-clone-able and shared because
        // multiple connection tasks update it concurrently.
        let r = NodeRegistry::new();
        let r2 = r.clone();
        r.insert(make_handle("alice@host", 1)).unwrap();
        assert!(r2.get(&id("alice@host")).is_some());
    }
}
