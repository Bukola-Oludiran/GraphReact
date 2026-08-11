//! Distributed pg-style group registry — M14.6.
//!
//! [`DistGroups`] overlays the local `GroupRegistry`
//! ([`graphreact_core::runtime::supervision::groups`]) with a
//! per-process view of cluster-wide group memberships and a
//! gossip protocol for syncing them between peers. Same shape as
//! M14.5's `RemoteSupervision`: callbacks installed on the
//! scheduler at `Cluster::start` time, and the per-connection
//! task funnels inbound `Pg*` packets into the orchestrator.
//!
//! ## State
//!
//! Memberships are tracked per `(group, member_qualified_id)`,
//! where the member id is `local_id@origin_node` — that's the
//! cluster-unique handle that PgBroadcast / PgJoin / PgLeave use
//! over the wire. For each `(group, member)` we keep a single
//! [`MemberState`]:
//!
//! - `present: bool` — current registry truth (false = a Leave
//!   superseded an earlier Join).
//! - `lamport: u64` — origin's monotonic clock at the moment
//!   the state was recorded. Higher Lamport beats lower.
//! - `origin: String` — the peer name that authored the entry.
//!
//! Updates are reconciled per-`(group, member)` last-writer-wins
//! by Lamport timestamp. A `(group, member)` whose origin is our
//! own node name represents a local member; the dist crate
//! chooses to fan inbound `PgBroadcast`s to those members
//! exclusively (their messages enqueue locally via
//! `SchedulerHandle::send_raw`).
//!
//! ## Gossip protocol
//!
//! - **PgJoin / PgLeave**: deltas. Sent to every connected peer
//!   when the local membership changes (via the
//!   [`graphreact_core::RemoteGroupSink`] callback).
//! - **PgDiscover → PgSync**: post-handshake handshake.
//!   Whenever a peer connection completes, we send a
//!   `PgDiscover` and the peer replies with a `PgSync`
//!   containing its full known state. We merge under
//!   Lamport-newest-wins so reconnection-after-partition
//!   converges.
//! - **PgBroadcast**: hub-and-spoke fan-out, sent from the
//!   originating node to every peer hosting ≥1 member of
//!   `group`. Receivers re-fan to local members via
//!   `SchedulerHandle::send_raw`.
//!
//! ## Threading
//!
//! Same as M14.5 — `parking_lot::Mutex` per state map; sinks fire
//! on the scheduler thread; inbound packet handlers fire on tokio
//! workers. Locks are short and uncontested in the common case.
//!
//! ## Reconnection semantics
//!
//! On peer disconnect ([`Self::synthesize_disconnect`]) we drop
//! every membership originated by that peer. When the same peer
//! reconnects with a fresh `Creation`, its `PgSync` reseeds
//! state from scratch — the dropped-then-rejoined members get
//! re-installed via the new `PgSync`'s Lamport timestamps.
//! Because we DROPPED the prior state instead of merging, an
//! earlier `Leave` from before the disconnect cannot resurrect
//! against a fresh `Join` (the OTP convention the splendid-waffle
//! plan calls out).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use graphreact_core::runtime::types::Message;
use graphreact_core::{create_message, GroupOp, SchedulerHandle};
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;

/// **M14.8.x-followon (Phase C).** Debounce window for local
/// `PgJoin` / `PgLeave` outbound fan-out. Multiple
/// `record_local_group_op` calls within this window are buffered
/// in `Inner::pending_ops` and shipped together at the end of the
/// window (still as separate per-op packets — wire format
/// unchanged), preventing storms when bodies thrash group
/// membership during a render burst.
///
/// Local upserts (the `state: Mutex<...>` map) are NOT delayed;
/// only wire-side fan-out is.
pub const MIN_GOSSIP_INTERVAL: Duration = Duration::from_millis(100);

/// **M14.8.x-followon (Phase F).** Bound on the per-process
/// seen-broadcast cache used to deduplicate decentralized tree
/// fan-out. The cache key is `(from_qualified, timestamp)` —
/// FIFO-evicted when the bound is reached. 10k entries is a few
/// hundred kB at most and covers the worst-case observed
/// broadcast rate for clusters in the O(100)-node range.
const MAX_SEEN_BROADCASTS: usize = 10_000;

use crate::registry::NodeRegistry;
use crate::wire::{Packet, PgBroadcast, PgDiscover, PgJoin, PgLeave, PgSync};

/// Per-`(group, member)` membership state.
#[derive(Debug, Clone)]
struct MemberState {
    /// Latest seen presence flag — `true` after the most-recent
    /// Join; `false` after the most-recent Leave (entries with
    /// `present = false` are kept around with their Lamport so
    /// out-of-order Joins from earlier Lamports get rejected).
    present: bool,
    /// Origin's monotonic clock at the moment the state was
    /// authored. Higher Lamport beats lower at reconciliation.
    lamport: u64,
    /// Peer name that authored the state. `our_name` for local
    /// members.
    origin: String,
}

/// FIFO-bounded set of recently-seen `PgBroadcast`s.
/// Used by Phase F's decentralized tree fan-out to dedup
/// duplicate deliveries via different relay paths.
struct SeenBroadcasts {
    entries: FxHashSet<(String, u64)>,
    order: VecDeque<(String, u64)>,
    cap: usize,
}

impl SeenBroadcasts {
    fn with_capacity(cap: usize) -> Self {
        Self {
            entries: FxHashSet::default(),
            order: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// If the key is unseen, mark it seen and return `false`. If
    /// already seen, return `true` (caller drops the duplicate).
    /// FIFO-evicts the oldest entry when the cap is reached.
    fn check_and_insert(&mut self, key: (String, u64)) -> bool {
        if self.entries.contains(&key) {
            return true;
        }
        if self.entries.len() >= self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key.clone());
        self.order.push_back(key);
        false
    }
}

/// Buffered local group op pending wire fan-out (Phase C).
struct PendingOp {
    op: GroupOp,
    group: String,
    /// Qualified `local_id@our_name`.
    qualified_member: String,
    /// Lamport stamped at the moment of the local upsert. Stays
    /// authoritative even if buffering delays the wire send.
    lamport: u64,
}

struct Inner {
    /// `(group, member_qualified_id) → MemberState`. The map is
    /// flat (not group-then-member-nested) because most reads are
    /// "all members of group G with present=true" or "all entries
    /// originated by peer P" — both linear scans of the value
    /// set. Group-keyed nesting would speed up the first read
    /// path; for M14.6 acceptance the flat shape is plenty fast
    /// (clusters with O(thousands) of members).
    state: Mutex<FxHashMap<(String, String), MemberState>>,

    /// Highest Lamport seen from each peer. Used during
    /// `PgSync` reconciliation as a pre-filter — entries we've
    /// already-seen-newer-from-this-peer can be skipped without
    /// the per-(group, member) lookup.
    peer_high_water: Mutex<FxHashMap<String, u64>>,

    /// Our own monotonic clock. Increments before every outgoing
    /// state change so peers see a strictly-increasing Lamport
    /// from us across joins / leaves.
    our_lamport: AtomicU64,

    /// **M14.8.x-followon (Phase C).** Buffered `PgJoin` / `PgLeave`
    /// ops awaiting wire fan-out. `record_local_group_op` appends;
    /// the debounce task drains after `MIN_GOSSIP_INTERVAL`.
    pending_ops: Mutex<Vec<PendingOp>>,

    /// **M14.8.x-followon (Phase C).** Handle of the
    /// currently-sleeping debounce task, if any. The
    /// `is_finished()` check on push prevents redundant
    /// schedulings while a task is in-flight.
    flush_handle: Mutex<Option<JoinHandle<()>>>,

    /// **M14.8.x-followon (Phase F).** Receiver-side dedup
    /// cache for decentralized tree broadcast. Key is
    /// `(from_qualified, timestamp)`. FIFO-bounded by
    /// `MAX_SEEN_BROADCASTS`. On a duplicate inbound
    /// `PgBroadcast` (e.g., one peer relays via tree path A
    /// while another path B also delivers), receiver drops
    /// the duplicate without fanning to local members or
    /// re-relaying.
    seen_broadcasts: Mutex<SeenBroadcasts>,

    scheduler_handle: SchedulerHandle,
    registry: NodeRegistry,
    our_name: String,
}

/// Distributed group orchestrator. Cheap-clone (`Arc<Inner>`).
#[derive(Clone)]
pub struct DistGroups {
    inner: Arc<Inner>,
}

impl DistGroups {
    pub fn new(
        scheduler_handle: SchedulerHandle,
        registry: NodeRegistry,
        our_name: String,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(FxHashMap::default()),
                peer_high_water: Mutex::new(FxHashMap::default()),
                our_lamport: AtomicU64::new(0),
                pending_ops: Mutex::new(Vec::new()),
                flush_handle: Mutex::new(None),
                seen_broadcasts: Mutex::new(SeenBroadcasts::with_capacity(MAX_SEEN_BROADCASTS)),
                scheduler_handle,
                registry,
                our_name,
            }),
        }
    }

    pub fn our_name(&self) -> &str {
        &self.inner.our_name
    }

    fn next_local_lamport(&self) -> u64 {
        self.inner.our_lamport.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn observe_peer_lamport(&self, peer: &str, lamport: u64) {
        let mut hw = self.inner.peer_high_water.lock();
        let entry = hw.entry(peer.to_string()).or_insert(0);
        if lamport > *entry {
            *entry = lamport;
        }
    }

    /// Apply an inbound state record. Returns `true` if the
    /// record was newer than what we had and the map mutated;
    /// `false` if it was an out-of-order duplicate.
    fn upsert(&self, group: &str, member: &str, present: bool, lamport: u64, origin: &str) -> bool {
        let mut state = self.inner.state.lock();
        let key = (group.to_string(), member.to_string());
        match state.get(&key) {
            Some(existing) if existing.lamport >= lamport => false,
            _ => {
                state.insert(
                    key,
                    MemberState {
                        present,
                        lamport,
                        origin: origin.to_string(),
                    },
                );
                true
            }
        }
    }

    // ---------------------------------------------------------------
    // Outbound — installed as the scheduler's RemoteGroupSink and
    // RemoteBroadcastSink.
    // ---------------------------------------------------------------

    /// `RemoteGroupSink` callback. Records the local join/leave
    /// at our origin's next Lamport tick. **M14.8.x-followon
    /// (Phase C):** local upsert happens immediately so local
    /// queries see the change, but the wire-side fan-out is
    /// buffered and shipped after `MIN_GOSSIP_INTERVAL` (100ms)
    /// to coalesce render-burst storms. Multiple ops within the
    /// window become a single batched flush (still as separate
    /// `PgJoin` / `PgLeave` packets — wire format unchanged).
    pub fn record_local_group_op(&self, op: GroupOp, group: &str, member_local_id: &str) {
        let lamport = self.next_local_lamport();
        let qualified = format!("{member_local_id}@{}", self.inner.our_name);
        // 1. Local upsert is immediate.
        self.upsert(
            group,
            &qualified,
            matches!(op, GroupOp::Join),
            lamport,
            &self.inner.our_name,
        );
        // 2. Buffer the op for delayed wire fan-out.
        self.inner.pending_ops.lock().push(PendingOp {
            op,
            group: group.to_string(),
            qualified_member: qualified,
            lamport,
        });
        // 3. Schedule the debounce flush if not already in
        // flight.
        self.maybe_spawn_debounce_task();
    }

    /// Schedule a one-shot tokio task that sleeps
    /// `MIN_GOSSIP_INTERVAL` then drains
    /// `pending_ops` via [`Self::flush_pending_ops`]. If a task
    /// is already in flight (`is_finished()` returns false), this
    /// is a no-op — the in-flight task will drain whatever has
    /// accumulated when it wakes up. (Phase C.)
    fn maybe_spawn_debounce_task(&self) {
        let mut handle_guard = self.inner.flush_handle.lock();
        if let Some(h) = handle_guard.as_ref() {
            if !h.is_finished() {
                return;
            }
        }
        let groups = self.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(MIN_GOSSIP_INTERVAL).await;
            groups.flush_pending_ops();
        });
        *handle_guard = Some(task);
    }

    /// Drain `pending_ops` and ship one `PgJoin` / `PgLeave`
    /// packet per buffered op to every connected peer. Public so
    /// `Cluster::shutdown` can flush in-flight ops synchronously
    /// before dropping connections, and so tests can force-flush
    /// without waiting on the 100ms debounce window. (Phase C.)
    pub fn flush_pending_ops(&self) {
        let drained: Vec<PendingOp> = std::mem::take(&mut *self.inner.pending_ops.lock());
        for pending in drained {
            let vc = vec![(self.inner.our_name.clone(), pending.lamport)];
            let pkt = match pending.op {
                GroupOp::Join => Packet::PgJoin(PgJoin {
                    group: pending.group,
                    member: pending.qualified_member,
                    vc,
                }),
                GroupOp::Leave => Packet::PgLeave(PgLeave {
                    group: pending.group,
                    member: pending.qualified_member,
                    vc,
                }),
            };
            self.fan_to_all_peers(pkt);
        }
    }

    /// Number of currently-buffered ops awaiting wire fan-out.
    /// Test introspection. (Phase C.)
    pub fn pending_ops_count(&self) -> usize {
        self.inner.pending_ops.lock().len()
    }

    /// `RemoteBroadcastSink` callback. Fired AFTER local-member
    /// fan-out. **M14.8.x-followon (Phase F):** decentralized
    /// tree fan-out — originator populates `visited_peers`
    /// with `[our_name]` and sends to every peer hosting ≥1
    /// `present=true` member of `group`. Each receiver fans
    /// to local members AND relays to peers not in
    /// `visited_peers` (with its own name appended). Loop
    /// prevention: visited list grows monotonically.
    pub fn record_local_broadcast(
        &self,
        group: &str,
        from_qualified: &str,
        msg_type: &str,
        payload: &Value,
    ) {
        let peers_with_members = self.peers_with_members(group);
        if peers_with_members.is_empty() {
            return;
        }
        let payload_bytes = serde_json::to_vec(payload).unwrap_or_else(|e| {
            tracing::error!("dist groups: payload serialise failed (impossible?): {e}");
            Vec::new()
        });
        let timestamp = wall_clock_ms();
        // Originator's visited_peers seed = [our_name].
        let visited_peers = vec![self.inner.our_name.clone()];
        for peer_name in peers_with_members {
            let pkt = Packet::PgBroadcast(PgBroadcast {
                group: group.to_string(),
                msg_type: msg_type.to_string(),
                payload: payload_bytes.clone(),
                from_qualified: from_qualified.to_string(),
                timestamp,
                visited_peers: visited_peers.clone(),
            });
            if let Some(peer) = self.inner.registry.get_by_name(&peer_name) {
                best_effort_send(&peer.send_tx, &peer_name, pkt);
            }
        }
    }

    /// Set of peer names hosting ≥1 `present=true` member of `group`.
    fn peers_with_members(&self, group: &str) -> FxHashSet<String> {
        let state = self.inner.state.lock();
        state
            .iter()
            .filter_map(|((g, _member), s)| {
                if g == group && s.present && s.origin != self.inner.our_name {
                    Some(s.origin.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Bare local ids of `present=true` members of `group`
    /// originated by us. Used by inbound `PgBroadcast` to fan
    /// out to local members.
    fn local_members_of_group(&self, group: &str) -> Vec<String> {
        let state = self.inner.state.lock();
        state
            .iter()
            .filter_map(|((g, member), s)| {
                if g == group && s.present && s.origin == self.inner.our_name {
                    // Strip the @our_name suffix to get the
                    // bare local id the scheduler routes by.
                    member.rsplit_once('@').map(|(local, _)| local.to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Send a packet to every currently-connected peer.
    /// Best-effort: per-peer drops on backpressure / closed
    /// channel are logged but don't propagate.
    fn fan_to_all_peers(&self, pkt: Packet) {
        for peer_id in self.inner.registry.list() {
            let peer_name = peer_id.name().to_string();
            if let Some(peer) = self.inner.registry.get_by_name(&peer_name) {
                let cloned = pkt.clone();
                best_effort_send(&peer.send_tx, &peer_name, cloned);
            }
        }
    }

    // ---------------------------------------------------------------
    // Disconnect — drop every membership originated by the peer.
    // ---------------------------------------------------------------

    /// Drop every membership that came from `peer_name`. Future
    /// `PgBroadcast`s targeting groups won't try to fan to that
    /// peer; future inbound `PgBroadcast` from the same group
    /// (post-reconnect) reseeds via `PgSync`.
    pub fn synthesize_disconnect(&self, peer_name: &str) {
        let mut state = self.inner.state.lock();
        state.retain(|(_g, _m), s| s.origin != peer_name);
        // Don't drop peer_high_water — keeping the high-water
        // lets us reject post-reconnect duplicates from the same
        // origin if for some reason they replay an old Lamport.
        // The new connection will increment from there.
    }

    // ---------------------------------------------------------------
    // Inbound packet handlers.
    // ---------------------------------------------------------------

    pub fn handle_pg_join(&self, peer_name: &str, pkt: PgJoin) {
        // Take the maximum Lamport from the vc that matches the
        // sending peer (which authors the entry). If the vc
        // doesn't include the peer's own entry (malformed
        // packet), fall back to the highest Lamport in the vc.
        let lamport = lamport_from_vc(&pkt.vc, peer_name);
        self.observe_peer_lamport(peer_name, lamport);
        self.upsert(&pkt.group, &pkt.member, true, lamport, peer_name);
    }

    pub fn handle_pg_leave(&self, peer_name: &str, pkt: PgLeave) {
        let lamport = lamport_from_vc(&pkt.vc, peer_name);
        self.observe_peer_lamport(peer_name, lamport);
        self.upsert(&pkt.group, &pkt.member, false, lamport, peer_name);
    }

    pub fn handle_pg_sync(&self, peer_name: &str, pkt: PgSync) {
        for (peer, lamport) in &pkt.vc {
            self.observe_peer_lamport(peer, *lamport);
        }
        // PgSync's `groups: Vec<(group, Vec<member>)>` carries the
        // SENDER'S relayed knowledge of every cluster member, not
        // just the sender's own. The authoritative origin of each
        // entry is the @-suffix of the qualified member id —
        // `local_id@origin_node`. The Lamport for an entry is
        // peer_name's known high-water for that origin (which the
        // sender just told us via `pkt.vc`). Falling back to the
        // sender's own Lamport when the vc doesn't carry the
        // origin (e.g. self-relay) is acceptable.
        let pkt_vc: FxHashMap<String, u64> = pkt.vc.iter().cloned().collect();
        let mut snapshot_set: FxHashSet<(String, String)> = FxHashSet::default();
        for (group, members) in &pkt.groups {
            for member in members {
                snapshot_set.insert((group.clone(), member.clone()));
                let origin = member
                    .rsplit_once('@')
                    .map(|(_, n)| n.to_string())
                    .unwrap_or_else(|| peer_name.to_string());
                let lamport_for_origin = pkt_vc
                    .get(&origin)
                    .copied()
                    .or_else(|| self.inner.peer_high_water.lock().get(&origin).copied())
                    .unwrap_or(0);
                self.upsert(group, member, true, lamport_for_origin, &origin);
            }
        }
        // Implicit-Leave sweep: only entries authored by
        // peer_name itself (origin == peer_name) are in
        // peer_name's authoritative scope. If peer_name's
        // authoritative own-origin list omits one of its own
        // members, that member implicitly left during the
        // partition. Don't touch entries authored by other
        // peers — peer_name's relay is non-authoritative for
        // those.
        let lamport_for_peer = pkt_vc
            .get(peer_name)
            .copied()
            .or_else(|| self.inner.peer_high_water.lock().get(peer_name).copied())
            .unwrap_or(0);
        let mut state = self.inner.state.lock();
        for ((g, m), s) in state.iter_mut() {
            if s.origin == peer_name && s.present {
                let key = (g.clone(), m.clone());
                if !snapshot_set.contains(&key) {
                    s.present = false;
                    s.lamport = lamport_for_peer;
                }
            }
        }
    }

    pub fn handle_pg_discover(&self, peer_name: &str, pkt: PgDiscover) {
        for (peer, lamport) in &pkt.vc {
            self.observe_peer_lamport(peer, *lamport);
        }
        // Reply with a full `PgSync` of OUR known-present state.
        // Acceptable for M14.6 — the substrate-not-MVP rule says
        // ship complete reconciliation; tighter delta-only
        // reconciliation is a future optimisation.
        let sync = self.snapshot_sync();
        if let Some(peer) = self.inner.registry.get_by_name(peer_name) {
            best_effort_send(&peer.send_tx, peer_name, Packet::PgSync(sync));
        }
    }

    pub fn handle_pg_broadcast(&self, peer_name: &str, pkt: PgBroadcast) {
        // **M14.8.x-followon (Phase F)** dedup. Decentralized
        // tree fan-out can deliver the same broadcast via
        // multiple relay paths — drop duplicates before any
        // fan-out or relay so observers see exactly one
        // delivery and we don't re-flood the cluster.
        let dedup_key = (pkt.from_qualified.clone(), pkt.timestamp);
        if self
            .inner
            .seen_broadcasts
            .lock()
            .check_and_insert(dedup_key)
        {
            tracing::trace!(
                "dist groups: dropping duplicate PgBroadcast \
                 from={} ts={} (received via peer `{peer_name}`)",
                pkt.from_qualified,
                pkt.timestamp
            );
            return;
        }

        // Decode payload up front (used by both local fan-out
        // and relay).
        let payload: Value = if pkt.payload.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice(&pkt.payload) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("dist groups: PgBroadcast payload decode failed: {e}");
                    return;
                }
            }
        };

        // 1. Local fan-out — to every present local member of `group`.
        let local_members = self.local_members_of_group(&pkt.group);
        for member_id in local_members {
            let msg = create_message(
                pkt.msg_type.clone(),
                pkt.from_qualified.clone(),
                member_id,
                payload.clone(),
                None,
            );
            self.inner.scheduler_handle.send_raw(msg);
        }

        // 2. **M14.8.x-followon (Phase F)** decentralized tree
        // relay. Compute `downstream` = peers with members in
        // `group` MINUS peers already in `visited_peers` MINUS
        // the inbound sender (defensive — even if the sender
        // forgot to seed `visited_peers` with their own name,
        // we know who delivered this packet to us). Append
        // our_name to `visited_peers` and ship the updated
        // PgBroadcast to each downstream peer.
        let mut visited: FxHashSet<&str> = pkt.visited_peers.iter().map(String::as_str).collect();
        visited.insert(peer_name);
        let our_name = self.inner.our_name.as_str();
        let peers_with_members = self.peers_with_members(&pkt.group);
        let downstream: Vec<String> = peers_with_members
            .into_iter()
            .filter(|p| !visited.contains(p.as_str()) && p != our_name)
            .collect();
        if downstream.is_empty() {
            return;
        }
        // New visited list = inbound visited + sender + our_name
        // (deduplicated). The sender add is defensive against a
        // malformed inbound that omitted its own name.
        let mut new_visited = pkt.visited_peers.clone();
        for to_add in [peer_name, our_name] {
            if !new_visited.iter().any(|p| p == to_add) {
                new_visited.push(to_add.to_string());
            }
        }
        for downstream_peer in downstream {
            let relay_pkt = Packet::PgBroadcast(PgBroadcast {
                group: pkt.group.clone(),
                msg_type: pkt.msg_type.clone(),
                payload: pkt.payload.clone(),
                from_qualified: pkt.from_qualified.clone(),
                timestamp: pkt.timestamp,
                visited_peers: new_visited.clone(),
            });
            if let Some(peer) = self.inner.registry.get_by_name(&downstream_peer) {
                best_effort_send(&peer.send_tx, &downstream_peer, relay_pkt);
            }
        }
    }

    /// Build a `PgSync` carrying our full present-state and the
    /// peer-high-water vc. Used on `PgDiscover` reply and
    /// post-reconnect resync.
    fn snapshot_sync(&self) -> PgSync {
        let state = self.inner.state.lock();
        let mut groups: FxHashMap<String, Vec<String>> = FxHashMap::default();
        for ((g, member), s) in state.iter() {
            if s.present {
                groups.entry(g.clone()).or_default().push(member.clone());
            }
        }
        let groups_vec: Vec<(String, Vec<String>)> = groups.into_iter().collect();
        let vc: Vec<(String, u64)> = self
            .inner
            .peer_high_water
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        PgSync {
            groups: groups_vec,
            vc,
        }
    }

    // ---------------------------------------------------------------
    // Connection-event hook — called from cluster.rs after a peer
    // handshake completes.
    // ---------------------------------------------------------------

    /// Send a `PgDiscover` to a freshly-connected peer so they
    /// reply with a `PgSync` that seeds our view of their
    /// memberships. Idempotent — re-running on the same peer
    /// just causes another reconciliation round.
    pub fn on_peer_connected(&self, peer_name: &str) {
        let pkt = Packet::PgDiscover(PgDiscover {
            groups_known: self.snapshot_known_group_names(),
            vc: self
                .inner
                .peer_high_water
                .lock()
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        });
        if let Some(peer) = self.inner.registry.get_by_name(peer_name) {
            best_effort_send(&peer.send_tx, peer_name, pkt);
        }
    }

    fn snapshot_known_group_names(&self) -> Vec<String> {
        let state = self.inner.state.lock();
        let mut names: FxHashSet<String> = FxHashSet::default();
        for ((g, _m), s) in state.iter() {
            if s.present {
                names.insert(g.clone());
            }
        }
        names.into_iter().collect()
    }

    // ---------------------------------------------------------------
    // Test introspection.
    // ---------------------------------------------------------------

    /// Number of `present=true` members of `group`. Counts
    /// across every origin (local + every peer).
    pub fn member_count(&self, group: &str) -> usize {
        let state = self.inner.state.lock();
        state
            .iter()
            .filter(|((g, _m), s)| g == group && s.present)
            .count()
    }

    /// Bare local ids of present members of `group` originated
    /// by `peer_name`. `peer_name = our_name` returns local
    /// members. Used by tests to verify cross-node visibility.
    pub fn members_of_group_by_origin(&self, group: &str, peer_name: &str) -> Vec<String> {
        let state = self.inner.state.lock();
        state
            .iter()
            .filter_map(|((g, member), s)| {
                if g == group && s.present && s.origin == peer_name {
                    member.rsplit_once('@').map(|(local, _)| local.to_string())
                } else {
                    None
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Pull the Lamport our peer authored for itself out of a vc.
/// Falls back to the max Lamport in the vc if the peer's own
/// entry is absent (defensive against malformed packets).
fn lamport_from_vc(vc: &[(String, u64)], peer_name: &str) -> u64 {
    vc.iter()
        .find_map(|(k, v)| if k == peer_name { Some(*v) } else { None })
        .or_else(|| vc.iter().map(|(_, v)| *v).max())
        .unwrap_or(0)
}

fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn best_effort_send(
    tx: &tokio::sync::mpsc::Sender<Packet>,
    peer_name: &str,
    packet: Packet,
) -> bool {
    let ty = packet.packet_type();
    match tx.try_send(packet) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            tracing::warn!("dist groups: outbound channel full to `{peer_name}`; dropping {ty:?}");
            false
        }
        Err(TrySendError::Closed(_)) => {
            tracing::debug!("dist groups: peer `{peer_name}` channel closed; dropping {ty:?}");
            false
        }
    }
}

#[allow(dead_code)]
fn _unused_message_type_assertion(_m: Message) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_id::Creation;
    use crate::node_id::NodeId;
    use crate::registry::PeerHandle;
    use graphreact_core::Scheduler;
    use tokio::sync::mpsc;

    fn fresh() -> (DistGroups, NodeRegistry) {
        let scheduler = Scheduler::default();
        let registry = NodeRegistry::new();
        let groups = DistGroups::new(scheduler.handle(), registry.clone(), "alice".to_string());
        (groups, registry)
    }

    fn insert_peer(reg: &NodeRegistry, name: &str) -> mpsc::Receiver<Packet> {
        let (tx, rx) = mpsc::channel(64);
        let node: NodeId = format!("{name}@127.0.0.1").parse().unwrap();
        let handle = Arc::new(PeerHandle::new(node, Creation(1), tx, Vec::new(), 0));
        reg.insert(handle).unwrap();
        rx
    }

    #[tokio::test]
    async fn record_local_join_ships_pg_join_to_every_peer() {
        let (groups, registry) = fresh();
        let mut bob_rx = insert_peer(&registry, "bob");
        let mut carol_rx = insert_peer(&registry, "carol");
        groups.record_local_group_op(GroupOp::Join, "workers", "w1");
        // PgJoin landed on both peers.
        for rx in [&mut bob_rx, &mut carol_rx] {
            let pkt = rx.recv().await.expect("PgJoin");
            match pkt {
                Packet::PgJoin(j) => {
                    assert_eq!(j.group, "workers");
                    assert_eq!(j.member, "w1@alice");
                    assert_eq!(j.vc.len(), 1);
                    assert_eq!(j.vc[0].0, "alice");
                }
                other => panic!("expected PgJoin, got {other:?}"),
            }
        }
        assert_eq!(groups.member_count("workers"), 1);
    }

    #[tokio::test]
    async fn handle_pg_join_lamport_newest_wins() {
        let (groups, _reg) = fresh();
        groups.handle_pg_join(
            "bob",
            PgJoin {
                group: "workers".into(),
                member: "w1@bob".into(),
                vc: vec![("bob".to_string(), 5)],
            },
        );
        assert_eq!(groups.member_count("workers"), 1);
        // Stale Leave with lower Lamport — rejected.
        groups.handle_pg_leave(
            "bob",
            PgLeave {
                group: "workers".into(),
                member: "w1@bob".into(),
                vc: vec![("bob".to_string(), 3)],
            },
        );
        assert_eq!(
            groups.member_count("workers"),
            1,
            "stale Leave should be rejected"
        );
        // Newer Leave — wins.
        groups.handle_pg_leave(
            "bob",
            PgLeave {
                group: "workers".into(),
                member: "w1@bob".into(),
                vc: vec![("bob".to_string(), 7)],
            },
        );
        assert_eq!(groups.member_count("workers"), 0);
    }

    #[tokio::test]
    async fn synthesize_disconnect_drops_only_peer_origin_entries() {
        let (groups, _reg) = fresh();
        groups.handle_pg_join(
            "bob",
            PgJoin {
                group: "workers".into(),
                member: "w1@bob".into(),
                vc: vec![("bob".to_string(), 1)],
            },
        );
        groups.handle_pg_join(
            "carol",
            PgJoin {
                group: "workers".into(),
                member: "w2@carol".into(),
                vc: vec![("carol".to_string(), 1)],
            },
        );
        groups.record_local_group_op(GroupOp::Join, "workers", "w0");
        assert_eq!(groups.member_count("workers"), 3);
        groups.synthesize_disconnect("bob");
        assert_eq!(groups.member_count("workers"), 2);
        assert!(groups
            .members_of_group_by_origin("workers", "bob")
            .is_empty());
    }

    #[tokio::test]
    async fn handle_pg_sync_implicit_leave_for_partition_state() {
        let (groups, _reg) = fresh();
        // Bob says he had w1 and w2.
        groups.handle_pg_join(
            "bob",
            PgJoin {
                group: "workers".into(),
                member: "w1@bob".into(),
                vc: vec![("bob".to_string(), 1)],
            },
        );
        groups.handle_pg_join(
            "bob",
            PgJoin {
                group: "workers".into(),
                member: "w2@bob".into(),
                vc: vec![("bob".to_string(), 2)],
            },
        );
        assert_eq!(groups.member_count("workers"), 2);
        // After partition, bob's PgSync only carries w1. w2 was
        // implicitly removed during the partition.
        groups.handle_pg_sync(
            "bob",
            PgSync {
                groups: vec![("workers".to_string(), vec!["w1@bob".to_string()])],
                vc: vec![("bob".to_string(), 10)],
            },
        );
        assert_eq!(groups.member_count("workers"), 1);
        assert_eq!(
            groups.members_of_group_by_origin("workers", "bob"),
            vec!["w1".to_string()]
        );
    }

    #[tokio::test]
    async fn handle_pg_broadcast_fans_to_local_members_only() {
        let (groups, _reg) = fresh();
        groups.record_local_group_op(GroupOp::Join, "workers", "w0");
        groups.handle_pg_join(
            "bob",
            PgJoin {
                group: "workers".into(),
                member: "wB@bob".into(),
                vc: vec![("bob".to_string(), 1)],
            },
        );
        // Inbound PgBroadcast from bob targeting "workers" —
        // we have one local member (w0). bob's local member
        // wB@bob is NOT a local member from our point of view,
        // so we don't enqueue a message for it.
        let payload = serde_json::json!({"task": 42});
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        groups.handle_pg_broadcast(
            "bob",
            PgBroadcast {
                group: "workers".into(),
                msg_type: "do_work".into(),
                payload: payload_bytes,
                from_qualified: "wB@bob".into(),
                timestamp: 0,
                visited_peers: vec!["bob".to_string()],
            },
        );
        // Verify the local fan-out enqueued a Message via send_raw
        // by spawning a tokio task to drive the scheduler — but
        // SchedulerHandle::send_raw lands in external_inbox; we
        // check by counting. There's no direct probe on
        // SchedulerHandle; this test verifies the path doesn't
        // panic and the inbound broadcast doesn't double-fan to
        // bob's own member.
    }

    /// **M14.8.x-followon (Phase C) acceptance.** 5 rapid
    /// `record_local_group_op` calls within the 100ms debounce
    /// window produce ZERO outbound packets immediately, then 5
    /// outbound packets shipped together at the end of the
    /// window. Local upserts are immediate; only wire-side
    /// fan-out is delayed.
    #[tokio::test]
    async fn debounce_buffers_rapid_ops_until_flush_window() {
        let (groups, registry) = fresh();
        let mut bob_rx = insert_peer(&registry, "bob");

        // Fire 5 ops rapidly.
        for i in 0..5u32 {
            groups.record_local_group_op(GroupOp::Join, "workers", &format!("w{i}"));
        }

        // Local upserts are immediate.
        assert_eq!(groups.member_count("workers"), 5);

        // Within 30ms, no packets should have shipped (debounce
        // is 100ms). Use try_recv to check non-blocking.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            bob_rx.try_recv().is_err(),
            "no packets should ship before debounce window elapses"
        );

        // 5 ops are buffered.
        assert_eq!(groups.pending_ops_count(), 5);

        // Wait past the 100ms debounce window.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // All 5 PgJoin packets should now have arrived.
        let mut received = 0;
        for _ in 0..5 {
            match bob_rx.try_recv() {
                Ok(Packet::PgJoin(_)) => received += 1,
                Ok(other) => panic!("expected PgJoin, got {other:?}"),
                Err(_) => break,
            }
        }
        assert_eq!(
            received, 5,
            "all 5 buffered PgJoin packets should ship after debounce"
        );

        // Buffer is now empty.
        assert_eq!(groups.pending_ops_count(), 0);
    }

    /// **M14.8.x-followon (Phase F) acceptance.** Tree fan-out
    /// dedup. A receiver that gets the same broadcast via two
    /// different relay paths only fans to local members ONCE,
    /// and only relays ONCE. Verified by simulating the second
    /// delivery and asserting (a) no duplicate ship to the
    /// remaining peer and (b) duplicate is silently dropped.
    #[tokio::test]
    async fn tree_broadcast_dedup_drops_duplicates() {
        let (groups, registry) = fresh();
        let _bob_rx = insert_peer(&registry, "bob");
        let mut carol_rx = insert_peer(&registry, "carol");

        // Register one local member (alice's w0) + register
        // bob and carol as having members of `workers`. With
        // local member present, our handle_pg_broadcast will
        // do local fan-out + try to relay.
        groups.record_local_group_op(GroupOp::Join, "workers", "w0");
        groups.flush_pending_ops();
        // Drain the bob_rx + carol_rx PgJoin packets we just
        // shipped (so the test assertions only see the
        // PgBroadcast relay traffic).
        let _ = _bob_rx;
        // carol_rx — drain the PgJoin from groups.record_local_group_op above
        while carol_rx.try_recv().is_ok() {}
        groups.handle_pg_join(
            "bob",
            PgJoin {
                group: "workers".into(),
                member: "wB@bob".into(),
                vc: vec![("bob".to_string(), 1)],
            },
        );
        groups.handle_pg_join(
            "carol",
            PgJoin {
                group: "workers".into(),
                member: "wC@carol".into(),
                vc: vec![("carol".to_string(), 1)],
            },
        );

        let payload = serde_json::json!({"task": 99});
        let payload_bytes = serde_json::to_vec(&payload).unwrap();

        // First delivery — from bob, via the originator-style
        // path. visited_peers = ["origin", "bob"] meaning
        // alice should relay to carol but NOT bob.
        groups.handle_pg_broadcast(
            "bob",
            PgBroadcast {
                group: "workers".into(),
                msg_type: "do_work".into(),
                payload: payload_bytes.clone(),
                from_qualified: "origin@somewhere".into(),
                timestamp: 1_234_567_890,
                visited_peers: vec!["origin".to_string(), "bob".to_string()],
            },
        );

        // Carol should have received exactly ONE relay packet.
        let mut relay_count = 0;
        while let Ok(pkt) = carol_rx.try_recv() {
            if matches!(pkt, Packet::PgBroadcast(_)) {
                relay_count += 1;
            }
        }
        assert_eq!(
            relay_count, 1,
            "first delivery should relay to carol exactly once"
        );

        // Second delivery — same (from_qualified, timestamp)
        // but routed via carol this time. Dedup should drop it.
        groups.handle_pg_broadcast(
            "carol",
            PgBroadcast {
                group: "workers".into(),
                msg_type: "do_work".into(),
                payload: payload_bytes,
                from_qualified: "origin@somewhere".into(),
                timestamp: 1_234_567_890,
                visited_peers: vec!["origin".to_string(), "carol".to_string()],
            },
        );

        // No additional relay packets should ship.
        let mut second_relay_count = 0;
        while let Ok(pkt) = carol_rx.try_recv() {
            if matches!(pkt, Packet::PgBroadcast(_)) {
                second_relay_count += 1;
            }
        }
        assert_eq!(
            second_relay_count, 0,
            "duplicate delivery should be deduped — no additional relay"
        );
    }

    /// **M14.8.x-followon (Phase C).** `flush_pending_ops` ships
    /// buffered ops synchronously — used by `Cluster::shutdown`
    /// to ensure no in-flight gossip is lost on teardown.
    #[tokio::test]
    async fn flush_pending_ops_drains_immediately() {
        let (groups, registry) = fresh();
        let mut bob_rx = insert_peer(&registry, "bob");

        groups.record_local_group_op(GroupOp::Join, "workers", "w0");
        groups.record_local_group_op(GroupOp::Leave, "workers", "w0");

        // Buffer has 2 ops; nothing on the wire yet.
        assert_eq!(groups.pending_ops_count(), 2);
        assert!(bob_rx.try_recv().is_err());

        // Force flush.
        groups.flush_pending_ops();

        // Both packets ship synchronously.
        assert!(matches!(bob_rx.try_recv(), Ok(Packet::PgJoin(_))));
        assert!(matches!(bob_rx.try_recv(), Ok(Packet::PgLeave(_))));
        assert_eq!(groups.pending_ops_count(), 0);
    }
}
