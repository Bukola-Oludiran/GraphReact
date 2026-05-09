//! pg-style process groups.
//!
//! Implementations land with M5 stage 2.

use crate::runtime::scheduler::NodeKey;
use rustc_hash::FxHashMap;

/// Group name. Newtype around `String` to keep the API obvious.
pub type GroupName = String;

#[derive(Debug, Default)]
pub struct GroupRegistry {
    members: FxHashMap<GroupName, Vec<NodeKey>>,
}

impl GroupRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `node` to `group`. Idempotent — multiple joins are collapsed.
    pub fn join(&mut self, group: impl Into<GroupName>, node: NodeKey) {
        let entry = self.members.entry(group.into()).or_default();
        if !entry.contains(&node) {
            entry.push(node);
        }
    }

    /// Remove `node` from `group`. No-op if absent.
    pub fn leave(&mut self, group: &str, node: NodeKey) {
        if let Some(entry) = self.members.get_mut(group) {
            entry.retain(|k| *k != node);
            if entry.is_empty() {
                self.members.remove(group);
            }
        }
    }

    /// Drop `node` from every group it has joined. Called from the
    /// unmount path so groups self-clean.
    pub fn leave_all(&mut self, node: NodeKey) {
        let mut empty: Vec<String> = Vec::new();
        for (name, members) in &mut self.members {
            members.retain(|k| *k != node);
            if members.is_empty() {
                empty.push(name.clone());
            }
        }
        for name in empty {
            self.members.remove(&name);
        }
    }

    /// Current members of `group`. Order matches join order.
    #[must_use]
    pub fn members(&self, group: &str) -> Vec<NodeKey> {
        self.members.get(group).cloned().unwrap_or_default()
    }

    /// Number of registered groups.
    #[must_use]
    pub fn group_count(&self) -> usize {
        self.members.len()
    }

    /// Names of all non-empty groups.
    #[must_use]
    pub fn group_names(&self) -> Vec<String> {
        self.members.keys().cloned().collect()
    }

    /// Names of every group `node` currently belongs to. Used by
    /// the unmount path so `RemoteGroupSink` can fan a `Leave`
    /// per-group BEFORE [`Self::leave_all`] clears the local
    /// registry. M14.6.
    #[must_use]
    pub fn groups_for_node(&self, node: NodeKey) -> Vec<String> {
        self.members
            .iter()
            .filter_map(|(name, members)| {
                if members.contains(&node) {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect()
    }
}
