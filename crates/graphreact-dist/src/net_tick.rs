//! Net-tick + partition detection — M14.7.
//!
//! [`tick_loop`] is a single cluster-wide tokio task spawned at
//! [`crate::Cluster::start`] that runs every `tick_interval`. Each
//! iteration it:
//!
//! 1. Iterates the live peer registry. For every peer, sends one
//!    `Packet::Tick` with the current sequence number (best-effort —
//!    drops silently on a closed channel; the partition-detection
//!    pass below catches that case).
//! 2. Reads `peer.last_tick` (updated by `dispatch_inbound` on
//!    every inbound packet — `Tick`, `Message`, supervision,
//!    groups gossip, anything). If `now_ms - last_tick`
//!    exceeds `miss_threshold * tick_interval`, the peer is
//!    considered partitioned.
//! 3. Partition handling depends on [`NetSplitPolicy`]:
//!    - [`NetSplitPolicy::ContinueWithStale`] removes the peer
//!      from the registry. The connection task's
//!      `send_rx.recv()` then returns `None`, the task exits,
//!      and the M14.5 / M14.6 disconnect-synthesis paths fire
//!      `__DOWN__` / `__EXIT__` and clean translation tables
//!      exactly as if the TCP connection had closed.
//!    - [`NetSplitPolicy::PauseOnSplit`] (default) logs a
//!      warning and leaves the peer in the registry — observers
//!      see no event. If ticks resume the peer recovers; if
//!      not, the kernel-level TCP keepalive eventually closes
//!      the underlying socket and the regular Eof path takes
//!      over. Application code that needs to react to soft
//!      partition independently of `__DOWN__` can poll
//!      [`PeerHandle::last_tick_ms`] directly.
//!
//! ## Defaults and overrides
//!
//! Production default mirrors OTP-shaped semantics: 5s tick,
//! 3-miss threshold = 15s detection floor. Tests routinely
//! override to 100ms / 2 (200ms detection floor) — both knobs
//! live in [`NetTickConfig`] on [`crate::ClusterConfig`].

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use tokio::task::JoinHandle;

use crate::groups::DistGroups;
use crate::registry::NodeRegistry;
use crate::supervision::RemoteSupervision;
use crate::wire::{Packet, Tick};

/// **M14.10.** Peer-level partition state observable to the
/// supervisor decision hook. Updated by [`tick_pass`] on every
/// pass; queried by the closure installed by `Cluster::start`.
///
/// Today the only consumer is `Cluster::start`'s decision hook;
/// future M16-Backend ops surfaces (Prometheus / management API)
/// will surface the same map directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    /// Peer has sent traffic within the detector threshold.
    Healthy,
    /// Peer has missed the threshold but is still inside the
    /// `partition_grace` window. Logged as warn under
    /// PauseOnSplit; never escalates supervisor decisions yet.
    Suspect { since_ms: u64 },
    /// Peer crossed the `partition_grace` window. Under
    /// PauseOnSplit this triggers ContinueWithStale-equivalent
    /// behaviour (drop registry + synthesize_disconnect) — the
    /// peer is partitioned in earnest.
    Partitioned { since_ms: u64 },
}

/// **M14.10.** Process-shared peer-state map. Populated + mutated
/// by [`tick_pass`]; cloned (cheap `Arc`) into the supervisor
/// decision hook installed by `Cluster::start`.
pub type PeerStates = std::sync::Arc<DashMap<String, PeerStatus>>;

/// What to do when the partition detector marks a peer as
/// partitioned (no inbound traffic for `miss_threshold *
/// tick_interval`). Default is the safer
/// [`NetSplitPolicy::PauseOnSplit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetSplitPolicy {
    /// **Default.** Hold the peer in the registry; log a
    /// warning. Reactive observers don't see a phantom
    /// `__DOWN__` from the partition. If ticks resume, the
    /// peer continues; if not, kernel TCP keepalive eventually
    /// closes the socket and the regular Eof path fires
    /// `synthesize_disconnect` then.
    #[default]
    PauseOnSplit,
    /// Treat partition as disconnect. Remove the peer from the
    /// registry — the connection task exits, M14.5 / M14.6
    /// disconnect-synthesis fires immediately. Useful for
    /// workloads where stale-state-during-partition is worse
    /// than transient false positives.
    ContinueWithStale,
}

/// Tick + partition-detection knobs.
#[derive(Debug, Clone, Copy)]
pub struct NetTickConfig {
    /// How often [`tick_loop`] runs. Outbound `Tick` packets
    /// are sent on each iteration; inbound liveness is checked
    /// on the same cadence.
    pub tick_interval: Duration,
    /// Number of consecutive `tick_interval`s without inbound
    /// traffic before a peer is considered partitioned. With
    /// the default `tick_interval = 5s`, `miss_threshold = 3`
    /// gives a 15s detection floor.
    pub miss_threshold: u32,
    /// **M14.10.** Window after a peer enters [`PeerStatus::Suspect`]
    /// during which PauseOnSplit holds the peer (warn-only) and
    /// supervisors of children depending on it park their restart
    /// decisions. After this window, PauseOnSplit escalates the
    /// peer to [`PeerStatus::Partitioned`] (drop registry +
    /// synthesize_disconnect) and parked supervisor decisions
    /// re-evaluate against the now-empty peer_states map.
    pub partition_grace: Duration,
}

impl Default for NetTickConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(5),
            miss_threshold: 3,
            partition_grace: Duration::from_secs(30),
        }
    }
}

impl NetTickConfig {
    /// Detection threshold in milliseconds —
    /// `miss_threshold * tick_interval`.
    #[must_use]
    pub fn detection_threshold_ms(&self) -> u64 {
        let ms = u64::try_from(self.tick_interval.as_millis()).unwrap_or(u64::MAX);
        ms.saturating_mul(u64::from(self.miss_threshold))
    }
}

/// Spawn the cluster-wide tick + partition detection loop. The
/// returned [`JoinHandle`] can be aborted at cluster shutdown.
pub fn spawn_tick_loop(
    registry: NodeRegistry,
    supervision: RemoteSupervision,
    dist_groups: DistGroups,
    peer_states: PeerStates,
    config: NetTickConfig,
    policy: NetSplitPolicy,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(config.tick_interval);
        // Skip the first immediate tick so we don't spam Ticks
        // before the first peer has even handshaken.
        interval.tick().await;
        let mut seq: u64 = 0;
        loop {
            interval.tick().await;
            seq = seq.wrapping_add(1);
            tick_pass(
                &registry,
                &supervision,
                &dist_groups,
                &peer_states,
                &config,
                policy,
                seq,
            );
        }
    })
}

/// One pass of the tick loop — visible in tests so the pass can
/// be driven manually without waiting for `tokio::time::interval`.
pub fn tick_pass(
    registry: &NodeRegistry,
    supervision: &RemoteSupervision,
    dist_groups: &DistGroups,
    peer_states: &PeerStates,
    config: &NetTickConfig,
    policy: NetSplitPolicy,
    seq: u64,
) {
    let now = now_ms();
    let threshold_ms = config.detection_threshold_ms();
    let grace_ms = u64::try_from(config.partition_grace.as_millis()).unwrap_or(u64::MAX);
    let sent_at = now;
    for peer_id in registry.list() {
        let peer_name = peer_id.name().to_string();
        let Some(peer) = registry.get_by_name(&peer_name) else {
            continue;
        };
        // Send tick. Best-effort — backpressure or closed
        // channel just means we'll catch the staleness next
        // pass.
        let _ = peer.send_tx.try_send(Packet::Tick(Tick {
            seq,
            sent_at_ms: sent_at,
        }));

        let last = peer.last_tick_ms();
        let elapsed = now.saturating_sub(last);
        if elapsed > threshold_ms {
            match policy {
                NetSplitPolicy::ContinueWithStale => {
                    tracing::warn!(
                        "dist net_tick: peer `{peer_name}` partitioned ({elapsed}ms since last \
                         packet, threshold {threshold_ms}ms); ContinueWithStale → disconnecting"
                    );
                    peer_states
                        .insert(peer_name.clone(), PeerStatus::Partitioned { since_ms: now });
                    // Drop the registry entry; the run_connection
                    // task's send_rx.recv() returns None, the
                    // task exits, and synthesize_disconnect
                    // fires for both monitor / link state and
                    // group memberships.
                    drop(peer);
                    registry.remove_by_name(&peer_name);
                    // Also synthesize directly here so the
                    // synthesis fires even if the connection
                    // task is in the middle of a packet read
                    // — idempotent at the AtomicBool guard
                    // and at the per-peer index.
                    supervision.synthesize_disconnect(&peer_name);
                    dist_groups.synthesize_disconnect(&peer_name);
                }
                NetSplitPolicy::PauseOnSplit => {
                    // M14.10: track Suspect → Partitioned
                    // transition. While Suspect, log warn but
                    // hold the peer in the registry (today's
                    // PauseOnSplit semantics). Once grace
                    // elapses, escalate to ContinueWithStale-
                    // equivalent behaviour.
                    let suspect_since = match peer_states.get(&peer_name).map(|e| *e.value()) {
                        Some(
                            PeerStatus::Suspect { since_ms } | PeerStatus::Partitioned { since_ms },
                        ) => since_ms,
                        _ => {
                            peer_states
                                .insert(peer_name.clone(), PeerStatus::Suspect { since_ms: now });
                            now
                        }
                    };
                    let suspect_age = now.saturating_sub(suspect_since);
                    if suspect_age >= grace_ms {
                        tracing::warn!(
                            "dist net_tick: peer `{peer_name}` Suspect for {suspect_age}ms \
                             (grace {grace_ms}ms); PauseOnSplit escalating → disconnecting"
                        );
                        peer_states.insert(
                            peer_name.clone(),
                            PeerStatus::Partitioned {
                                since_ms: suspect_since,
                            },
                        );
                        drop(peer);
                        registry.remove_by_name(&peer_name);
                        supervision.synthesize_disconnect(&peer_name);
                        dist_groups.synthesize_disconnect(&peer_name);
                    } else {
                        tracing::warn!(
                            "dist net_tick: peer `{peer_name}` partitioned ({elapsed}ms since \
                             last packet, threshold {threshold_ms}ms); PauseOnSplit → \
                             Suspect (age {suspect_age}ms / grace {grace_ms}ms)"
                        );
                    }
                }
            }
        } else {
            // Healthy. If the peer was previously Suspect, the
            // partition healed — clear the entry so deferred
            // supervisor decisions can proceed on next batch.
            if let Some(prev) = peer_states.get(&peer_name).map(|e| *e.value()) {
                if !matches!(prev, PeerStatus::Healthy) {
                    tracing::info!("dist net_tick: peer `{peer_name}` healed; clearing Suspect");
                }
            }
            peer_states.insert(peer_name, PeerStatus::Healthy);
        }
    }
}

/// Wall-clock millisecond timestamp.
#[inline]
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `Arc::strong_count`-style stub kept for symmetry with the
/// other dist modules' helpers — nothing in net_tick needs it
/// today, but keeping the namespace open avoids churn when
/// M14.7 follow-on adds RTT histograms keyed by peer.
#[allow(dead_code)]
fn _unused() -> Arc<()> {
    Arc::new(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groups::DistGroups;
    use crate::node_id::{Creation, NodeId};
    use crate::registry::PeerHandle;
    use crate::supervision::RemoteSupervision;
    use graphreact_core::Scheduler;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    fn fresh() -> (RemoteSupervision, DistGroups, NodeRegistry) {
        let scheduler = Scheduler::default();
        let handle = scheduler.handle();
        let registry = NodeRegistry::new();
        let supervision = RemoteSupervision::new(
            handle.clone(),
            registry.clone(),
            "alice".to_string(),
            Creation(1),
        );
        let dist_groups = DistGroups::new(handle, registry.clone(), "alice".to_string());
        (supervision, dist_groups, registry)
    }

    fn insert_peer(reg: &NodeRegistry, name: &str, last_tick_ms: u64) -> mpsc::Receiver<Packet> {
        let (tx, rx) = mpsc::channel(64);
        let node: NodeId = format!("{name}@127.0.0.1").parse().unwrap();
        let h = Arc::new(PeerHandle::new(node, Creation(1), tx, Vec::new(), 0));
        h.last_tick.store(last_tick_ms, Ordering::Relaxed);
        reg.insert(h).unwrap();
        rx
    }

    #[tokio::test]
    async fn fresh_peer_initializes_last_tick_to_now() {
        let (_sup, _dg, reg) = fresh();
        let _rx = insert_peer(&reg, "bob", now_ms());
        let h = reg.get_by_name("bob").unwrap();
        let lag = now_ms().saturating_sub(h.last_tick_ms());
        assert!(
            lag < 50,
            "fresh peer last_tick should be ~now (lag {lag}ms)"
        );
    }

    fn fresh_peer_states() -> PeerStates {
        std::sync::Arc::new(DashMap::new())
    }

    #[tokio::test]
    async fn continue_with_stale_drops_partitioned_peer() {
        let (sup, dg, reg) = fresh();
        let states = fresh_peer_states();
        let _rx_bob = insert_peer(&reg, "bob", 0); // ancient last_tick
        let _rx_carol = insert_peer(&reg, "carol", now_ms()); // healthy
        let cfg = NetTickConfig {
            tick_interval: Duration::from_millis(50),
            miss_threshold: 2,
            partition_grace: Duration::from_millis(0),
        };
        tick_pass(
            &reg,
            &sup,
            &dg,
            &states,
            &cfg,
            NetSplitPolicy::ContinueWithStale,
            1,
        );
        assert!(
            reg.get_by_name("bob").is_none(),
            "bob should be removed under ContinueWithStale + stale last_tick"
        );
        assert!(
            reg.get_by_name("carol").is_some(),
            "carol's fresh last_tick should keep her registered"
        );
    }

    #[tokio::test]
    async fn pause_on_split_within_grace_holds_peer_as_suspect() {
        let (sup, dg, reg) = fresh();
        let states = fresh_peer_states();
        let _rx_bob = insert_peer(&reg, "bob", 0);
        let cfg = NetTickConfig {
            tick_interval: Duration::from_millis(50),
            miss_threshold: 2,
            partition_grace: Duration::from_secs(60),
        };
        tick_pass(
            &reg,
            &sup,
            &dg,
            &states,
            &cfg,
            NetSplitPolicy::PauseOnSplit,
            1,
        );
        assert!(
            reg.get_by_name("bob").is_some(),
            "bob should be held under PauseOnSplit within grace window"
        );
        match states.get("bob").map(|e| *e.value()) {
            Some(PeerStatus::Suspect { .. }) => {}
            other => panic!("expected Suspect, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pause_on_split_escalates_after_grace() {
        let (sup, dg, reg) = fresh();
        let states = fresh_peer_states();
        let _rx_bob = insert_peer(&reg, "bob", 0);
        let cfg = NetTickConfig {
            tick_interval: Duration::from_millis(50),
            miss_threshold: 2,
            partition_grace: Duration::from_millis(0),
        };
        // First pass: marks bob as Suspect with since_ms = now.
        tick_pass(
            &reg,
            &sup,
            &dg,
            &states,
            &cfg,
            NetSplitPolicy::PauseOnSplit,
            1,
        );
        // Second pass: grace is zero, so suspect_age >= grace_ms
        // immediately → escalation.
        tick_pass(
            &reg,
            &sup,
            &dg,
            &states,
            &cfg,
            NetSplitPolicy::PauseOnSplit,
            2,
        );
        assert!(
            reg.get_by_name("bob").is_none(),
            "bob should be removed by PauseOnSplit escalation after grace"
        );
        match states.get("bob").map(|e| *e.value()) {
            Some(PeerStatus::Partitioned { .. }) => {}
            other => panic!("expected Partitioned post-escalation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tick_pass_emits_tick_packet_to_each_peer() {
        let (sup, dg, reg) = fresh();
        let states = fresh_peer_states();
        let mut rx_bob = insert_peer(&reg, "bob", now_ms());
        let cfg = NetTickConfig {
            tick_interval: Duration::from_millis(50),
            miss_threshold: 100,
            partition_grace: Duration::from_secs(60),
        };
        tick_pass(
            &reg,
            &sup,
            &dg,
            &states,
            &cfg,
            NetSplitPolicy::PauseOnSplit,
            42,
        );
        let pkt = rx_bob.recv().await.expect("Tick packet");
        match pkt {
            Packet::Tick(t) => {
                assert_eq!(t.seq, 42);
                assert!(t.sent_at_ms > 0);
            }
            other => panic!("expected Tick, got {other:?}"),
        }
        // Healthy peer's status is Healthy.
        assert_eq!(
            states.get("bob").map(|e| *e.value()),
            Some(PeerStatus::Healthy)
        );
    }

    #[tokio::test]
    async fn detection_threshold_is_miss_times_interval() {
        let cfg = NetTickConfig {
            tick_interval: Duration::from_millis(500),
            miss_threshold: 4,
            partition_grace: Duration::from_secs(30),
        };
        assert_eq!(cfg.detection_threshold_ms(), 2000);
    }
}
