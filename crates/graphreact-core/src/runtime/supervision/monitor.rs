//! Monitor and link primitives.
//!
//! Modeled on OTP's `erlang:monitor/2` (unidirectional) and
//! `erlang:link/1` (bidirectional). When a watched node unmounts, the
//! observers receive a system message:
//!
//! * monitor: `__DOWN__` with `{ref, target, reason}` payload
//! * link:    `__EXIT__` with `{from, reason}` payload (delivered to all
//!            linked peers in both directions)
//!
//! Both registries live on the scheduler. The scheduler's `unmount` path
//! consults them to fan out the appropriate signals before tearing down
//! the node.

use rustc_hash::FxHashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::runtime::scheduler::NodeKey;

/// Why a node terminated. Captured at unmount time and delivered to
/// monitors/linked peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// Clean shutdown (planned removal).
    Normal,
    /// Supervisor-driven shutdown (graceful, but not user-initiated).
    Shutdown,
    /// Abnormal — restart strategies kick in for linked peers.
    Crashed { detail: String },
}

impl ExitReason {
    /// Whether this reason is "abnormal" — controls link propagation and
    /// supervisor restart decisions.
    #[must_use]
    pub fn is_abnormal(&self) -> bool {
        matches!(self, ExitReason::Crashed { .. })
    }

    /// Human-readable string for the system-message payload.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            ExitReason::Normal => "normal",
            ExitReason::Shutdown => "shutdown",
            ExitReason::Crashed { .. } => "crashed",
        }
    }

    /// Optional detail field (only set for `Crashed`).
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            ExitReason::Crashed { detail } => Some(detail),
            _ => None,
        }
    }
}

/// Stable identifier for a monitor. Returned by `Scheduler::monitor`,
/// passed to `demonitor`, included in `__DOWN__` payloads so observers
/// can correlate the event with the original `monitor` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef(pub u64);

static NEXT_MONITOR_REF: AtomicU64 = AtomicU64::new(0);

impl MonitorRef {
    fn new() -> Self {
        Self(NEXT_MONITOR_REF.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

/// Single monitor entry — observer watches target.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MonitorEntry {
    pub observer: NodeKey,
    pub target: NodeKey,
}

/// Registry of active monitors. Keyed by [`MonitorRef`] so `demonitor` can
/// find the entry without walking the table.
#[derive(Debug, Default)]
pub struct MonitorRegistry {
    entries: FxHashMap<MonitorRef, MonitorEntry>,
    /// Reverse index: target → set of refs watching it. Lets us fire
    /// DOWN messages efficiently when a target unmounts.
    by_target: FxHashMap<NodeKey, Vec<MonitorRef>>,
}

impl MonitorRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a monitor. Returns the new [`MonitorRef`].
    pub fn add(&mut self, observer: NodeKey, target: NodeKey) -> MonitorRef {
        let id = MonitorRef::new();
        self.entries.insert(id, MonitorEntry { observer, target });
        self.by_target.entry(target).or_default().push(id);
        id
    }

    /// Remove a monitor by ref. No-op if it doesn't exist.
    pub fn remove(&mut self, id: MonitorRef) {
        if let Some(entry) = self.entries.remove(&id) {
            if let Some(list) = self.by_target.get_mut(&entry.target) {
                list.retain(|r| *r != id);
                if list.is_empty() {
                    self.by_target.remove(&entry.target);
                }
            }
        }
    }

    /// Drain all monitors watching `target` and return them. Intended for
    /// the unmount path: caller iterates the returned entries to deliver
    /// `__DOWN__` messages.
    pub fn drain_target(&mut self, target: NodeKey) -> Vec<(MonitorRef, NodeKey)> {
        let Some(refs) = self.by_target.remove(&target) else {
            return Vec::new();
        };
        refs.into_iter()
            .filter_map(|id| self.entries.remove(&id).map(|entry| (id, entry.observer)))
            .collect()
    }

    /// Drop every monitor registered by `observer` (e.g., on observer
    /// unmount). Prevents stale entries from firing into a dead node.
    pub fn drain_observer(&mut self, observer: NodeKey) {
        let to_remove: Vec<MonitorRef> = self
            .entries
            .iter()
            .filter_map(|(id, entry)| (entry.observer == observer).then_some(*id))
            .collect();
        for id in to_remove {
            self.remove(id);
        }
    }

    /// Count of active monitors. For introspection / tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Bidirectional link registry. Stored as a symmetric adjacency: if
/// `a` links to `b`, both directions appear in the underlying set.
#[derive(Debug, Default)]
pub struct LinkRegistry {
    adj: FxHashMap<NodeKey, HashSet<NodeKey>>,
}

impl LinkRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a bidirectional link. Idempotent.
    pub fn add(&mut self, a: NodeKey, b: NodeKey) {
        if a == b {
            return; // Self-link is a no-op; matches OTP's `link(self())`.
        }
        self.adj.entry(a).or_default().insert(b);
        self.adj.entry(b).or_default().insert(a);
    }

    /// Remove a bidirectional link. No-op if not present.
    pub fn remove(&mut self, a: NodeKey, b: NodeKey) {
        if let Some(set) = self.adj.get_mut(&a) {
            set.remove(&b);
            if set.is_empty() {
                self.adj.remove(&a);
            }
        }
        if let Some(set) = self.adj.get_mut(&b) {
            set.remove(&a);
            if set.is_empty() {
                self.adj.remove(&b);
            }
        }
    }

    /// Whether `a` is linked to `b` (in either direction).
    #[must_use]
    pub fn is_linked(&self, a: NodeKey, b: NodeKey) -> bool {
        self.adj.get(&a).is_some_and(|set| set.contains(&b))
    }

    /// Drain every link involving `node`. Returns the peers that need to
    /// receive a `__EXIT__` message. Removes both sides cleanly.
    pub fn drain_node(&mut self, node: NodeKey) -> Vec<NodeKey> {
        let Some(peers) = self.adj.remove(&node) else {
            return Vec::new();
        };
        let peer_list: Vec<NodeKey> = peers.into_iter().collect();
        for peer in &peer_list {
            if let Some(set) = self.adj.get_mut(peer) {
                set.remove(&node);
                if set.is_empty() {
                    self.adj.remove(peer);
                }
            }
        }
        peer_list
    }

    /// Number of linked nodes. For tests / introspection.
    #[must_use]
    pub fn linked_count(&self, node: NodeKey) -> usize {
        self.adj.get(&node).map_or(0, HashSet::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::SlotMap;

    fn three_keys() -> (NodeKey, NodeKey, NodeKey) {
        let mut sm: SlotMap<NodeKey, ()> = SlotMap::with_key();
        (sm.insert(()), sm.insert(()), sm.insert(()))
    }

    #[test]
    fn monitor_registry_drain_target_returns_observers() {
        let (a, b, c) = three_keys();
        let mut reg = MonitorRegistry::new();
        let r1 = reg.add(a, c);
        let r2 = reg.add(b, c);
        assert_eq!(reg.len(), 2);

        let drained = reg.drain_target(c);
        assert_eq!(drained.len(), 2);
        assert!(drained.iter().any(|(r, obs)| *r == r1 && *obs == a));
        assert!(drained.iter().any(|(r, obs)| *r == r2 && *obs == b));
        assert!(reg.is_empty());
    }

    #[test]
    fn monitor_registry_remove_by_ref() {
        let (a, _b, c) = three_keys();
        let mut reg = MonitorRegistry::new();
        let r = reg.add(a, c);
        assert_eq!(reg.len(), 1);
        reg.remove(r);
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn monitor_registry_drain_observer_cleans_up() {
        let (a, _b, c) = three_keys();
        let mut reg = MonitorRegistry::new();
        let _ = reg.add(a, c);
        reg.drain_observer(a);
        assert!(reg.is_empty());
    }

    #[test]
    fn link_registry_is_bidirectional() {
        let (a, b, _c) = three_keys();
        let mut reg = LinkRegistry::new();
        reg.add(a, b);
        assert!(reg.is_linked(a, b));
        assert!(reg.is_linked(b, a));
        reg.remove(a, b);
        assert!(!reg.is_linked(a, b));
        assert!(!reg.is_linked(b, a));
    }

    #[test]
    fn link_registry_drain_returns_all_peers() {
        let (a, b, c) = three_keys();
        let mut reg = LinkRegistry::new();
        reg.add(a, b);
        reg.add(a, c);
        let mut peers = reg.drain_node(a);
        peers.sort_by_key(|k| format!("{k:?}"));
        assert_eq!(peers.len(), 2);
        assert!(!reg.is_linked(a, b));
        assert!(!reg.is_linked(a, c));
    }

    #[test]
    fn self_link_is_a_noop() {
        let (a, _b, _c) = three_keys();
        let mut reg = LinkRegistry::new();
        reg.add(a, a);
        assert_eq!(reg.linked_count(a), 0);
    }

    #[test]
    fn exit_reason_classifies_abnormality() {
        assert!(!ExitReason::Normal.is_abnormal());
        assert!(!ExitReason::Shutdown.is_abnormal());
        assert!(ExitReason::Crashed {
            detail: "oom".into()
        }
        .is_abnormal());
    }
}
