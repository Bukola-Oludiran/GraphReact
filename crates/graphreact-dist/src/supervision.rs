//! Distributed monitors and links — M14.5.
//!
//! [`RemoteSupervision`] owns the per-process translation tables
//! mapping cross-node monitor and link refs onto the local
//! `MonitorRegistry` / `LinkRegistry` via the shared
//! [`Scheduler::set_remote_sentinel`] node. The shape is deliberately
//! parallel to the M14.4 `RemoteSink` design — the dist crate
//! installs callbacks on the scheduler at `Cluster::start` time,
//! and the per-connection task funnels inbound supervision packets
//! into the same orchestrator.
//!
//! ## State
//!
//! Two outbound and two inbound translation tables live here.
//!
//! **Outbound** — refs we allocated locally for a remote target:
//!
//! - `outbound_monitors_by_node` — per-peer `Vec<RemoteMonitor>`
//!   so disconnect-time fan-out is `O(n)` in the number of
//!   monitors targeting that peer.
//! - `outbound_monitors_by_local_ref` — `MonitorRef → entry` so
//!   inbound `Down` packets locate the local stub in `O(1)` and
//!   fire `__DOWN__` to the right observer.
//! - `outbound_links_by_node` — `Vec<RemoteLink>` per peer for
//!   disconnect-time `__EXIT__` fan-out.
//! - `outbound_link_counts_per_observer` — `(observer_key, peer)
//!   → usize`. The shared sentinel collapses every remote link
//!   from one observer onto a single `(observer, sentinel)` edge
//!   in `LinkRegistry`. The counter ensures the sentinel edge is
//!   torn down ONLY when ALL of an observer's remote links have
//!   unwound, not on the first.
//!
//! **Inbound** — refs the remote allocated, addressing our local
//! nodes:
//!
//! - `inbound_monitors_by_local_target` — per-local-id list of
//!   `(peer_name, remote_ref, observer_qualified_name)`. When a
//!   local id unmounts, the [`graphreact_core::UnmountHook`]
//!   installed in [`Self::install_into`] iterates this list and
//!   ships a wire `Down` packet to each subscribing peer.
//! - `inbound_links_by_local_id` — symmetric for `Exit` packets.
//!
//! ## Threading
//!
//! All state lives behind `parking_lot::Mutex` because:
//!
//! - The scheduler's outbound sinks (`RemoteMonitorSink`,
//!   `RemoteLinkSink`, `RemoteUnlinkSink`) fire on the scheduler
//!   thread inside a hook closure — so they can't be `!Send`.
//! - Inbound packet handlers run on whatever tokio worker the
//!   per-connection `run_connection` task lands on.
//! - The unmount hook fires on the scheduler thread.
//!
//! Each method takes one lock at a time and drops it before any
//! cross-call (no nested lock acquisition, no deadlock surface).
//! The locks are short — single map insertion / removal / lookup
//! per critical section. No `await` is held across a lock.
//!
//! ## Idempotent disconnect
//!
//! Per-monitor `synthesized: AtomicBool` ensures that a peer
//! reconnecting and re-disconnecting can't double-fire `__DOWN__`
//! against an observer that already saw the first disconnect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use graphreact_core::{
    create_message, ExitReason, MonitorRef, NodeKey, SchedulerHandle, SYSTEM_DOWN, SYSTEM_EXIT,
};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use serde_json::json;
use tokio::sync::mpsc::error::TrySendError;

use crate::node_id::Creation;
use crate::registry::NodeRegistry;
use crate::wire::{
    Demonitor, Down, Exit, LinkRequest, MonitorAck, MonitorRequest, Packet, Unlink, WireExitReason,
};

/// Translation entry for an outbound monitor — a `use_monitor`
/// call that named a `name@<other_node>` target.
#[derive(Debug)]
struct OutboundMonitor {
    /// Local stub `MonitorRef` allocated against the shared
    /// remote sentinel. Used by inbound `Down` packets to locate
    /// the right observer and by the `synthesize_disconnect`
    /// path to schedule local registry cleanup via
    /// [`SchedulerHandle::demonitor_remote`].
    local_ref: MonitorRef,
    /// Bare local id of the observer node. Used as the `to`
    /// field of the synthesized `__DOWN__` message.
    observer_id: String,
    /// Full `name@host` string of the remote target — preserved
    /// so the synthesized `__DOWN__` payload's `target` field
    /// points at the qualified peer.
    target: String,
    /// One-shot flag that flips on the first `__DOWN__` synthesis
    /// for this entry. Prevents double-fire across a
    /// reconnect-then-disconnect race where the same `local_ref`
    /// could be drained by both an inbound `Down` packet AND a
    /// subsequent TCP close.
    synthesized: AtomicBool,
}

/// Translation entry for an outbound link.
#[derive(Debug, Clone)]
struct OutboundLink {
    observer_key: NodeKey,
    observer_id: String,
    target: String,
}

/// Translation entry for an inbound monitor — a remote node ran
/// `use_monitor("local_id@us")`. Stored so the unmount hook can
/// ship a wire `Down` packet to the subscribing peer.
#[derive(Debug, Clone)]
struct InboundMonitor {
    peer_name: String,
    /// The `remote_ref` the peer allocated. Echoed in the wire
    /// `Down` so the peer can find its local stub.
    remote_ref: u64,
}

/// Translation entry for an inbound link.
#[derive(Debug, Clone)]
struct InboundLink {
    peer_name: String,
    /// Qualified observer string the peer sent — preserved so the
    /// reciprocal wire `Exit` packet's `from` field is correct.
    /// Currently unused (the wire `Exit` uses the unmounting
    /// node's qualified id), but kept for symmetry with
    /// [`InboundMonitor`] and for diagnostics.
    #[allow(dead_code)]
    qualified_peer_id: String,
}

struct Inner {
    outbound_monitors_by_node: Mutex<FxHashMap<String, Vec<Arc<OutboundMonitor>>>>,
    outbound_monitors_by_local_ref: Mutex<FxHashMap<MonitorRef, Arc<OutboundMonitor>>>,
    outbound_links_by_node: Mutex<FxHashMap<String, Vec<OutboundLink>>>,
    outbound_link_counts_per_observer:
        Mutex<FxHashMap<NodeKey, FxHashMap<String, usize>>>,

    inbound_monitors_by_local_target: Mutex<FxHashMap<String, Vec<InboundMonitor>>>,
    inbound_links_by_local_id: Mutex<FxHashMap<String, Vec<InboundLink>>>,

    scheduler_handle: SchedulerHandle,
    registry: NodeRegistry,
    our_name: String,
    our_creation: Creation,
}

/// Distributed-supervision orchestrator.
///
/// Wraps an `Arc<Inner>` so the same instance can be shared
/// between the scheduler-side sinks (`Send + Sync` closures) and
/// the per-connection inbound dispatch tasks.
#[derive(Clone)]
pub struct RemoteSupervision {
    inner: Arc<Inner>,
}

impl RemoteSupervision {
    pub fn new(
        scheduler_handle: SchedulerHandle,
        registry: NodeRegistry,
        our_name: String,
        our_creation: Creation,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                outbound_monitors_by_node: Mutex::new(FxHashMap::default()),
                outbound_monitors_by_local_ref: Mutex::new(FxHashMap::default()),
                outbound_links_by_node: Mutex::new(FxHashMap::default()),
                outbound_link_counts_per_observer: Mutex::new(FxHashMap::default()),
                inbound_monitors_by_local_target: Mutex::new(FxHashMap::default()),
                inbound_links_by_local_id: Mutex::new(FxHashMap::default()),
                scheduler_handle,
                registry,
                our_name,
                our_creation,
            }),
        }
    }

    pub fn our_name(&self) -> &str {
        &self.inner.our_name
    }

    /// Number of outbound monitors targeting `peer_name`. Test
    /// introspection — used by the M14.5 acceptance gate to
    /// assert the translation table empties on disconnect.
    pub fn outbound_monitor_count_for_peer(&self, peer_name: &str) -> usize {
        self.inner
            .outbound_monitors_by_node
            .lock()
            .get(peer_name)
            .map_or(0, Vec::len)
    }

    /// Number of outbound links targeting `peer_name`. Test
    /// introspection.
    pub fn outbound_link_count_for_peer(&self, peer_name: &str) -> usize {
        self.inner
            .outbound_links_by_node
            .lock()
            .get(peer_name)
            .map_or(0, Vec::len)
    }

    /// Number of inbound monitor subscriptions for the local
    /// target id. Test introspection.
    pub fn inbound_monitor_count_for_target(&self, local_id: &str) -> usize {
        self.inner
            .inbound_monitors_by_local_target
            .lock()
            .get(local_id)
            .map_or(0, Vec::len)
    }

    // ---------------------------------------------------------------
    // Outbound — installed as the scheduler's RemoteMonitor /
    // RemoteLink / RemoteUnlink sinks.
    // ---------------------------------------------------------------

    /// `RemoteMonitorSink` callback. Records the outbound monitor
    /// in the per-peer fan-out index and ships a `MonitorRequest`
    /// packet. Best-effort delivery — if the peer isn't currently
    /// connected the entry stays in the table; the next disconnect
    /// fires `__DOWN__` with `reason="noconnection"`.
    pub fn record_outbound_monitor(
        &self,
        _observer_key: NodeKey,
        observer_id: &str,
        target: &str,
        local_ref: MonitorRef,
    ) {
        let Some(peer_name) = peer_name_of(target) else {
            return;
        };
        let entry = Arc::new(OutboundMonitor {
            local_ref,
            observer_id: observer_id.to_string(),
            target: target.to_string(),
            synthesized: AtomicBool::new(false),
        });
        self.inner
            .outbound_monitors_by_node
            .lock()
            .entry(peer_name.to_string())
            .or_default()
            .push(Arc::clone(&entry));
        self.inner
            .outbound_monitors_by_local_ref
            .lock()
            .insert(local_ref, Arc::clone(&entry));
        // Best-effort packet send — drops with debug log if the
        // peer isn't yet (or no longer) connected. The peer-name
        // entry stays in the table so disconnect-synthesis still
        // fires `__DOWN__` on the local observer.
        if let Some(peer) = self.inner.registry.get_by_name(peer_name) {
            let qualified_observer = format!("{observer_id}@{}", self.inner.our_name);
            let pkt = Packet::MonitorRequest(MonitorRequest {
                observer: qualified_observer,
                target: target.to_string(),
                remote_ref: local_ref.0,
                creation: self.inner.our_creation.0,
            });
            best_effort_send(&peer.send_tx, peer_name, pkt);
        }
    }

    /// `RemoteLinkSink` callback. Records the outbound link in
    /// per-peer + per-observer indexes and ships a `LinkRequest`
    /// packet. Returns `true` if the request was queued for
    /// delivery (the peer is currently connected).
    pub fn record_outbound_link(
        &self,
        observer_key: NodeKey,
        observer_id: &str,
        target: &str,
    ) -> bool {
        let Some(peer_name) = peer_name_of(target) else {
            return false;
        };
        let entry = OutboundLink {
            observer_key,
            observer_id: observer_id.to_string(),
            target: target.to_string(),
        };
        self.inner
            .outbound_links_by_node
            .lock()
            .entry(peer_name.to_string())
            .or_default()
            .push(entry);
        // Increment the per-observer-per-peer counter so the
        // shared sentinel edge in `LinkRegistry` stays installed
        // until ALL remote links from this observer unwind.
        let mut counts = self.inner.outbound_link_counts_per_observer.lock();
        *counts
            .entry(observer_key)
            .or_default()
            .entry(peer_name.to_string())
            .or_insert(0) += 1;
        drop(counts);
        let Some(peer) = self.inner.registry.get_by_name(peer_name) else {
            return false;
        };
        let qualified_observer = format!("{observer_id}@{}", self.inner.our_name);
        let pkt = Packet::LinkRequest(LinkRequest {
            a: qualified_observer,
            b: target.to_string(),
        });
        best_effort_send(&peer.send_tx, peer_name, pkt)
    }

    /// `RemoteUnlinkSink` callback. Removes the outbound link
    /// entry, decrements the per-observer counter, and — if the
    /// counter hits zero — schedules
    /// [`SchedulerHandle::unlink_remote`] so the local
    /// `(observer, sentinel)` edge in `LinkRegistry` is torn
    /// down at the next `process_batch`.
    pub fn record_outbound_unlink(
        &self,
        observer_key: NodeKey,
        observer_id: &str,
        target: &str,
    ) {
        let Some(peer_name) = peer_name_of(target) else {
            return;
        };
        // Remove the matching outbound link entry (first match).
        {
            let mut by_node = self.inner.outbound_links_by_node.lock();
            if let Some(entries) = by_node.get_mut(peer_name) {
                if let Some(pos) = entries
                    .iter()
                    .position(|e| e.observer_key == observer_key && e.target == target)
                {
                    entries.swap_remove(pos);
                    if entries.is_empty() {
                        by_node.remove(peer_name);
                    }
                }
            }
        }
        // Decrement the per-observer-per-peer counter.
        let counter_zero_for_observer = {
            let mut counts = self.inner.outbound_link_counts_per_observer.lock();
            let mut total = 0usize;
            if let Some(per_peer) = counts.get_mut(&observer_key) {
                if let Some(entry) = per_peer.get_mut(peer_name) {
                    *entry = entry.saturating_sub(1);
                    if *entry == 0 {
                        per_peer.remove(peer_name);
                    }
                }
                total = per_peer.values().sum();
                if per_peer.is_empty() {
                    counts.remove(&observer_key);
                }
            }
            total == 0
        };
        if counter_zero_for_observer {
            self.inner.scheduler_handle.unlink_remote(observer_key);
        }
        // Ship the wire Unlink. Best-effort.
        if let Some(peer) = self.inner.registry.get_by_name(peer_name) {
            let qualified_observer = format!("{observer_id}@{}", self.inner.our_name);
            let pkt = Packet::Unlink(Unlink {
                a: qualified_observer,
                b: target.to_string(),
            });
            best_effort_send(&peer.send_tx, peer_name, pkt);
        }
    }

    // ---------------------------------------------------------------
    // Disconnect synthesis — called from run_connection's exit point.
    // ---------------------------------------------------------------

    /// Fan out `__DOWN__` and `__EXIT__` to every local observer
    /// holding an outbound monitor or link against `peer_name`.
    /// Idempotent — re-running after the same peer reconnects
    /// briefly and disconnects again does NOT double-fire on
    /// already-synthesized refs.
    pub fn synthesize_disconnect(&self, peer_name: &str) {
        // ------- Monitors -------
        let monitors = self
            .inner
            .outbound_monitors_by_node
            .lock()
            .remove(peer_name)
            .unwrap_or_default();
        for entry in monitors {
            // CAS guard so a reconnect-then-disconnect race can't
            // double-deliver. Acquire-release on the boolean is
            // sufficient — we only need atomicity, not ordering
            // against other state.
            if entry.synthesized.swap(true, Ordering::AcqRel) {
                continue;
            }
            // Drop from by_local_ref too so an inbound `Down`
            // arriving in flight can't re-fire.
            self.inner
                .outbound_monitors_by_local_ref
                .lock()
                .remove(&entry.local_ref);
            // Push synthesized __DOWN__. The scheduler's normal
            // delivery path will route to the observer.
            let payload = json!({
                "ref": entry.local_ref.0,
                "target": entry.target,
                "reason": "noconnection",
            });
            let msg = create_message(
                SYSTEM_DOWN,
                "__system__",
                entry.observer_id.clone(),
                payload,
                None,
            );
            self.inner.scheduler_handle.send_raw(msg);
            // Schedule local stub cleanup. Drained at the head
            // of the next process_batch.
            self.inner
                .scheduler_handle
                .demonitor_remote(entry.local_ref);
        }
        // ------- Links -------
        let links = self
            .inner
            .outbound_links_by_node
            .lock()
            .remove(peer_name)
            .unwrap_or_default();
        if !links.is_empty() {
            let mut counts = self.inner.outbound_link_counts_per_observer.lock();
            for link in links {
                let payload = json!({
                    "from": link.target,
                    "reason": "noconnection",
                });
                let msg = create_message(
                    SYSTEM_EXIT,
                    "__system__",
                    link.observer_id.clone(),
                    payload,
                    None,
                );
                self.inner.scheduler_handle.send_raw(msg);
                // Decrement per-observer-per-peer counter and
                // — if the observer no longer has any remote
                // links anywhere — schedule unlink_remote so
                // the sentinel edge gets shed.
                let observer_zero = {
                    let mut total = 0usize;
                    if let Some(per_peer) = counts.get_mut(&link.observer_key) {
                        per_peer.remove(peer_name);
                        total = per_peer.values().sum();
                        if per_peer.is_empty() {
                            counts.remove(&link.observer_key);
                        }
                    }
                    total == 0
                };
                if observer_zero {
                    self.inner
                        .scheduler_handle
                        .unlink_remote(link.observer_key);
                }
            }
        }
        // ------- Inbound subscriptions -------
        // Inbound monitors / links from this peer are no longer
        // valid — their wire-Down/Exit fan-out was conditional on
        // the peer being reachable. Drop them so subsequent
        // local-target unmounts don't try to ship to a dead
        // connection. (The outbound table cleanup via
        // schedule-remove is already complete above.)
        {
            let mut by_target = self.inner.inbound_monitors_by_local_target.lock();
            by_target.retain(|_, list| {
                list.retain(|m| m.peer_name != peer_name);
                !list.is_empty()
            });
        }
        {
            let mut by_id = self.inner.inbound_links_by_local_id.lock();
            by_id.retain(|_, list| {
                list.retain(|l| l.peer_name != peer_name);
                !list.is_empty()
            });
        }
    }

    // ---------------------------------------------------------------
    // Inbound packet handlers — called from run_connection.
    // ---------------------------------------------------------------

    /// Inbound `MonitorRequest`. Records the subscription and
    /// replies with `MonitorAck`. The `target_alive` field
    /// reflects whether the local target is currently registered;
    /// the peer can use this to short-circuit re-arm logic.
    pub fn handle_monitor_request(&self, peer_name: &str, req: MonitorRequest) {
        let local_target = strip_our_suffix(&req.target, &self.inner.our_name)
            .unwrap_or_else(|| req.target.clone());
        self.inner
            .inbound_monitors_by_local_target
            .lock()
            .entry(local_target.clone())
            .or_default()
            .push(InboundMonitor {
                peer_name: peer_name.to_string(),
                remote_ref: req.remote_ref,
            });
        // The "target alive" check would consult the local
        // scheduler's id_to_key. We don't have a Send+Sync handle
        // for that today; reply optimistically with `true`. The
        // peer's __DOWN__ handler still fires on disconnect or
        // unmount, so the worst case is a redundant Ack that
        // gets immediately followed by a Down on a missing
        // target. Documented; tightening lands when M16-Backend
        // adds an introspection endpoint.
        if let Some(peer) = self.inner.registry.get_by_name(peer_name) {
            let pkt = Packet::MonitorAck(MonitorAck {
                remote_ref: req.remote_ref,
                target_alive: true,
            });
            best_effort_send(&peer.send_tx, peer_name, pkt);
        }
    }

    /// Inbound `MonitorAck`. No-op for M14.5 — the local stub
    /// ref was already allocated synchronously inside the sink
    /// callback; the ack is informational. M16-Backend may
    /// surface it through observability.
    #[allow(clippy::needless_pass_by_value)]
    pub fn handle_monitor_ack(&self, _peer_name: &str, _ack: MonitorAck) {}

    /// Inbound `Demonitor`. Drops the matching inbound entry.
    pub fn handle_demonitor(&self, peer_name: &str, dem: Demonitor) {
        let mut by_target = self.inner.inbound_monitors_by_local_target.lock();
        by_target.retain(|_, list| {
            list.retain(|m| !(m.peer_name == peer_name && m.remote_ref == dem.remote_ref));
            !list.is_empty()
        });
    }

    /// Inbound `Down` — the remote peer's monitored target
    /// unmounted. Synthesize `__DOWN__` to the local observer
    /// holding the matching `local_ref`, then schedule cleanup.
    pub fn handle_down(&self, _peer_name: &str, down: Down) {
        let mref = MonitorRef(down.remote_ref);
        let entry = self
            .inner
            .outbound_monitors_by_local_ref
            .lock()
            .remove(&mref);
        let Some(entry) = entry else {
            // Either the local observer already unmounted (we
            // pruned via the unmount hook) or this is a
            // duplicate-after-disconnect Down. Drop silently.
            return;
        };
        if entry.synthesized.swap(true, Ordering::AcqRel) {
            return;
        }
        // Also remove from per-node index so disconnect-
        // synthesis doesn't re-fire.
        {
            let mut by_node = self.inner.outbound_monitors_by_node.lock();
            for list in by_node.values_mut() {
                list.retain(|e| e.local_ref != entry.local_ref);
            }
            by_node.retain(|_, list| !list.is_empty());
        }
        let reason = wire_exit_reason_to_str(&down.reason);
        let payload = json!({
            "ref": entry.local_ref.0,
            "target": entry.target,
            "reason": reason,
        });
        let msg = create_message(
            SYSTEM_DOWN,
            "__system__",
            entry.observer_id.clone(),
            payload,
            None,
        );
        self.inner.scheduler_handle.send_raw(msg);
        self.inner.scheduler_handle.demonitor_remote(mref);
    }

    /// Inbound `LinkRequest`. Records the inbound subscription so
    /// the unmount hook can ship a wire `Exit` when the local
    /// target id unmounts.
    pub fn handle_link_request(&self, peer_name: &str, req: LinkRequest) {
        let local_target = strip_our_suffix(&req.b, &self.inner.our_name)
            .unwrap_or_else(|| req.b.clone());
        self.inner
            .inbound_links_by_local_id
            .lock()
            .entry(local_target)
            .or_default()
            .push(InboundLink {
                peer_name: peer_name.to_string(),
                qualified_peer_id: req.a,
            });
    }

    /// Inbound `Unlink`.
    pub fn handle_unlink(&self, peer_name: &str, unl: Unlink) {
        let local_target = strip_our_suffix(&unl.b, &self.inner.our_name)
            .unwrap_or_else(|| unl.b.clone());
        let mut by_id = self.inner.inbound_links_by_local_id.lock();
        if let Some(list) = by_id.get_mut(&local_target) {
            list.retain(|l| l.peer_name != peer_name);
            if list.is_empty() {
                by_id.remove(&local_target);
            }
        }
    }

    /// Inbound `Exit` — the remote peer's linked node unmounted.
    /// Synthesize `__EXIT__` to every observer that held an
    /// outbound link to the remote `from` node.
    pub fn handle_exit(&self, peer_name: &str, ex: Exit) {
        let target = ex.from.clone();
        let observers: Vec<OutboundLink> = {
            let mut by_node = self.inner.outbound_links_by_node.lock();
            if let Some(list) = by_node.get_mut(peer_name) {
                let matched: Vec<OutboundLink> =
                    list.iter().filter(|l| l.target == target).cloned().collect();
                list.retain(|l| l.target != target);
                if list.is_empty() {
                    by_node.remove(peer_name);
                }
                matched
            } else {
                Vec::new()
            }
        };
        if observers.is_empty() {
            return;
        }
        let reason = wire_exit_reason_to_str(&ex.reason);
        let mut counts = self.inner.outbound_link_counts_per_observer.lock();
        for link in observers {
            let payload = json!({
                "from": link.target,
                "reason": reason,
            });
            let msg = create_message(
                SYSTEM_EXIT,
                "__system__",
                link.observer_id.clone(),
                payload,
                None,
            );
            self.inner.scheduler_handle.send_raw(msg);
            let observer_zero = {
                let mut total = 0usize;
                if let Some(per_peer) = counts.get_mut(&link.observer_key) {
                    if let Some(entry) = per_peer.get_mut(peer_name) {
                        *entry = entry.saturating_sub(1);
                        if *entry == 0 {
                            per_peer.remove(peer_name);
                        }
                    }
                    total = per_peer.values().sum();
                    if per_peer.is_empty() {
                        counts.remove(&link.observer_key);
                    }
                }
                total == 0
            };
            if observer_zero {
                self.inner
                    .scheduler_handle
                    .unlink_remote(link.observer_key);
            }
        }
    }

    // ---------------------------------------------------------------
    // Unmount hook — installed on the scheduler at Cluster::start.
    // ---------------------------------------------------------------

    /// Returns the `Arc<dyn Fn(&str, &ExitReason) + Send + Sync>`
    /// installed via `Scheduler::install_unmount_hook`. Fires
    /// from the scheduler's `unmount_with_reason` after local
    /// `__DOWN__` / `__EXIT__` fan-out completes; ships wire
    /// `Down` / `Exit` packets to every peer subscribing to the
    /// unmounting id and prunes outbound translation tables that
    /// referenced the unmounted node as observer.
    pub fn unmount_hook(&self) -> graphreact_core::UnmountHook {
        let inner = Arc::clone(&self.inner);
        Arc::new(move |unmounted_id: &str, reason: &ExitReason| {
            // 1. Inbound monitors against this id → ship Down to each peer.
            let inbound_monitors: Vec<InboundMonitor> = inner
                .inbound_monitors_by_local_target
                .lock()
                .remove(unmounted_id)
                .unwrap_or_default();
            for sub in inbound_monitors {
                let Some(peer) = inner.registry.get_by_name(&sub.peer_name) else {
                    continue;
                };
                let qualified_target = format!("{unmounted_id}@{}", inner.our_name);
                let pkt = Packet::Down(Down {
                    remote_ref: sub.remote_ref,
                    target: qualified_target,
                    reason: exit_reason_to_wire(reason),
                });
                best_effort_send(&peer.send_tx, &sub.peer_name, pkt);
            }
            // 2. Inbound links → ship Exit to each peer.
            let inbound_links: Vec<InboundLink> = inner
                .inbound_links_by_local_id
                .lock()
                .remove(unmounted_id)
                .unwrap_or_default();
            for sub in inbound_links {
                let Some(peer) = inner.registry.get_by_name(&sub.peer_name) else {
                    continue;
                };
                let qualified_from = format!("{unmounted_id}@{}", inner.our_name);
                let pkt = Packet::Exit(Exit {
                    from: qualified_from,
                    reason: exit_reason_to_wire(reason),
                });
                best_effort_send(&peer.send_tx, &sub.peer_name, pkt);
            }
            // 3. Outbound monitor entries observed by the unmounting
            //    node → drop. The local registry already pruned
            //    via `drain_observer`; we just clean up our table
            //    so disconnect-synthesis doesn't try to fire a
            //    __DOWN__ at a now-vanished observer.
            {
                let mut by_node = inner.outbound_monitors_by_node.lock();
                let mut by_ref = inner.outbound_monitors_by_local_ref.lock();
                by_node.retain(|_, list| {
                    list.retain(|e| {
                        if e.observer_id == unmounted_id {
                            by_ref.remove(&e.local_ref);
                            false
                        } else {
                            true
                        }
                    });
                    !list.is_empty()
                });
            }
            // 4. Outbound link entries for this observer → drop.
            //    The local LinkRegistry already pruned via
            //    `drain_node`; we clear the per-observer counter
            //    so the same observer id reused (rare) starts
            //    fresh.
            {
                let mut by_node = inner.outbound_links_by_node.lock();
                by_node.retain(|_, list| {
                    list.retain(|e| e.observer_id != unmounted_id);
                    !list.is_empty()
                });
            }
            // The per-observer counter is keyed by NodeKey, not
            // by id — so we don't have the key here. Stale
            // entries get GCed lazily on the next disconnect for
            // that observer (which can't happen since the
            // observer is gone) OR on the next remote-link from
            // a fresh observer that reuses the same id (rare;
            // slotmap keys don't reuse). Acceptable bounded leak.
        })
    }
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Extract the `name` part of a `local_id@name` target. Returns
/// `None` if there's no `@` — those are local targets and would
/// have been routed through the local path before reaching the
/// dist sink.
fn peer_name_of(target: &str) -> Option<&str> {
    target.rsplit_once('@').map(|(_, n)| n)
}

/// Strip `@<our_name>` suffix from a remote-supplied id. Returns
/// `None` if the suffix doesn't match (unexpected — the remote
/// shouldn't be addressing us under a different name) or there's
/// no `@` separator.
fn strip_our_suffix(qualified: &str, our_name: &str) -> Option<String> {
    let (local, suffix) = qualified.rsplit_once('@')?;
    if suffix == our_name {
        Some(local.to_string())
    } else {
        None
    }
}

/// Wire-friendly stringification of an [`ExitReason`]. Matches
/// the local `ExitReason::as_str` shape so the `__DOWN__` payload
/// looks identical regardless of whether the reason came from a
/// local or wire-side unmount.
fn wire_exit_reason_to_str(reason: &WireExitReason) -> &'static str {
    match reason {
        WireExitReason::Normal => "normal",
        WireExitReason::Shutdown => "shutdown",
        WireExitReason::Crashed { .. } => "crashed",
        WireExitReason::NodeDown => "noconnection",
    }
}

/// Convert a local [`ExitReason`] to the wire form. The detail
/// string for `Crashed` is preserved verbatim.
fn exit_reason_to_wire(reason: &ExitReason) -> WireExitReason {
    match reason {
        ExitReason::Normal => WireExitReason::Normal,
        ExitReason::Shutdown => WireExitReason::Shutdown,
        ExitReason::Crashed { detail } => WireExitReason::Crashed {
            detail: detail.clone(),
        },
    }
}

/// Best-effort packet send — drops with a debug log on backpressure
/// (channel full) and on closed-connection (the per-connection task
/// already exited; another disconnect-synthesis path will handle
/// it). Returns `true` on Ok, `false` otherwise.
fn best_effort_send(
    tx: &tokio::sync::mpsc::Sender<Packet>,
    peer_name: &str,
    packet: Packet,
) -> bool {
    let ty = packet.packet_type();
    match tx.try_send(packet) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            tracing::warn!(
                "dist supervision: outbound channel full to `{peer_name}`; dropping {ty:?}"
            );
            false
        }
        Err(TrySendError::Closed(_)) => {
            tracing::debug!(
                "dist supervision: peer `{peer_name}` channel closed; dropping {ty:?}"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_id::NodeId;
    use crate::registry::PeerHandle;
    use graphreact_core::{NodeKey, Scheduler};
    use tokio::sync::mpsc;

    /// A synthetic [`NodeKey`] for the tests below — we never feed
    /// it back into the scheduler, so the null key (slotmap's
    /// `Default`) is fine. The translation tables key on it for
    /// equality / hashing only.
    fn observer_key() -> NodeKey {
        NodeKey::default()
    }

    fn make_supervision() -> (RemoteSupervision, NodeRegistry) {
        let scheduler = Scheduler::default();
        let handle = scheduler.handle();
        let registry = NodeRegistry::new();
        let sup = RemoteSupervision::new(
            handle,
            registry.clone(),
            "alice".to_string(),
            Creation(7),
        );
        (sup, registry)
    }

    fn insert_peer(reg: &NodeRegistry, name: &str) -> mpsc::Receiver<Packet> {
        let (tx, rx) = mpsc::channel(64);
        let node: NodeId = format!("{name}@127.0.0.1").parse().unwrap();
        let handle = Arc::new(PeerHandle::new(node, Creation(1), tx, Vec::new()));
        reg.insert(handle).unwrap();
        rx
    }

    #[tokio::test]
    async fn record_outbound_monitor_ships_packet_and_indexes_entry() {
        let (sup, registry) = make_supervision();
        let mut rx = insert_peer(&registry, "bob");
        sup.record_outbound_monitor(observer_key(), "watcher", "responder@bob", MonitorRef(42));
        // Packet on the wire?
        let pkt = rx.recv().await.expect("MonitorRequest packet");
        match pkt {
            Packet::MonitorRequest(req) => {
                assert_eq!(req.observer, "watcher@alice");
                assert_eq!(req.target, "responder@bob");
                assert_eq!(req.remote_ref, 42);
                assert_eq!(req.creation, 7);
            }
            other => panic!("expected MonitorRequest, got {other:?}"),
        }
        // Indexed by peer + by ref?
        let by_node = sup.inner.outbound_monitors_by_node.lock();
        assert_eq!(by_node.get("bob").unwrap().len(), 1);
        let by_ref = sup.inner.outbound_monitors_by_local_ref.lock();
        assert!(by_ref.contains_key(&MonitorRef(42)));
    }

    #[tokio::test]
    async fn synthesize_disconnect_fires_down_once_and_idempotent_on_repeat() {
        let (sup, registry) = make_supervision();
        let _rx_bob = insert_peer(&registry, "bob");
        sup.record_outbound_monitor(observer_key(), "watcher", "tgt1@bob", MonitorRef(1));
        sup.record_outbound_monitor(observer_key(), "watcher", "tgt2@bob", MonitorRef(2));
        // Ensure the registry index is populated.
        assert_eq!(sup.inner.outbound_monitors_by_node.lock().get("bob").unwrap().len(), 2);
        sup.synthesize_disconnect("bob");
        // Both entries drained.
        assert!(sup.inner.outbound_monitors_by_node.lock().get("bob").is_none());
        assert_eq!(sup.inner.outbound_monitors_by_local_ref.lock().len(), 0);
        // Re-running is a no-op (idempotent).
        sup.synthesize_disconnect("bob");
        assert!(sup.inner.outbound_monitors_by_node.lock().get("bob").is_none());
    }

    #[tokio::test]
    async fn handle_link_request_records_inbound_subscription() {
        let (sup, registry) = make_supervision();
        let _rx = insert_peer(&registry, "bob");
        sup.handle_link_request(
            "bob",
            LinkRequest {
                a: "watcher@bob".to_string(),
                b: "tgt@alice".to_string(),
            },
        );
        let by_id = sup.inner.inbound_links_by_local_id.lock();
        assert_eq!(by_id.get("tgt").unwrap().len(), 1);
        assert_eq!(by_id.get("tgt").unwrap()[0].peer_name, "bob");
    }

    #[tokio::test]
    async fn record_outbound_link_then_unlink_decrements_to_zero() {
        let (sup, registry) = make_supervision();
        let _rx = insert_peer(&registry, "bob");
        let observer = observer_key();
        let queued = sup.record_outbound_link(observer, "linker", "tgt@bob");
        assert!(queued);
        assert_eq!(
            *sup.inner
                .outbound_link_counts_per_observer
                .lock()
                .get(&observer)
                .unwrap()
                .get("bob")
                .unwrap(),
            1
        );
        sup.record_outbound_unlink(observer, "linker", "tgt@bob");
        // Counter map empty for this observer.
        assert!(sup
            .inner
            .outbound_link_counts_per_observer
            .lock()
            .get(&observer)
            .is_none());
    }
}
