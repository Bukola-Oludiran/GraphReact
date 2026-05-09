//! `GraphDirectory` — lightweight structural index over the graph.
//!
//! One instance shared (via `Arc`) across all node contexts. Provides O(1)
//! lookups by ID, role, and depth. Maintained incrementally as the graph
//! mutates at runtime.

use rustc_hash::FxHashMap;

/// A single entry in the directory.
#[derive(Debug, Clone)]
pub struct DirectoryEntry {
    pub id: String,
    pub role: String,
    pub depth: u32,
    pub path: Vec<String>,
}

/// Structural directory mirroring TypeScript's `GraphDirectory`.
#[derive(Debug, Default)]
pub struct GraphDirectory {
    entries: FxHashMap<String, DirectoryEntry>,
    by_role: FxHashMap<String, Vec<String>>,
    by_depth: FxHashMap<u32, Vec<String>>,
}

impl GraphDirectory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lookup an entry by node ID.
    pub fn find_by_id(&self, id: &str) -> Option<&DirectoryEntry> {
        self.entries.get(id)
    }

    /// Node IDs registered under the given role.
    pub fn find_by_role(&self, role: &str) -> Vec<String> {
        self.by_role.get(role).cloned().unwrap_or_default()
    }

    /// Node IDs at the given hierarchy depth.
    pub fn find_by_depth(&self, depth: u32) -> Vec<String> {
        self.by_depth.get(&depth).cloned().unwrap_or_default()
    }

    /// All nodes whose ancestry path contains `ancestor_id`.
    pub fn find_in_subtree(&self, ancestor_id: &str) -> Vec<String> {
        self.entries
            .values()
            .filter(|entry| entry.path.iter().any(|step| step == ancestor_id))
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Whether `ancestor_id` precedes `descendant_id` on the descendant's path.
    pub fn is_ancestor(&self, ancestor_id: &str, descendant_id: &str) -> bool {
        let Some(descendant) = self.entries.get(descendant_id) else {
            return false;
        };
        let ancestor_idx = descendant.path.iter().position(|s| s == ancestor_id);
        let descendant_idx = descendant.path.iter().position(|s| s == descendant_id);
        match (ancestor_idx, descendant_idx) {
            (Some(a), Some(d)) => a < d,
            _ => false,
        }
    }

    /// Insert or replace an entry, updating role and depth indexes.
    pub fn add_entry(&mut self, entry: DirectoryEntry) {
        if let Some(existing) = self.entries.get(&entry.id).cloned() {
            self.remove_indexes_for(&existing);
        }
        self.by_role
            .entry(entry.role.clone())
            .or_default()
            .push(entry.id.clone());
        self.by_depth
            .entry(entry.depth)
            .or_default()
            .push(entry.id.clone());
        self.entries.insert(entry.id.clone(), entry);
    }

    /// Remove an entry by ID. No-op if it doesn't exist.
    pub fn remove_entry(&mut self, id: &str) {
        let Some(entry) = self.entries.remove(id) else {
            return;
        };
        self.remove_indexes_for(&entry);
    }

    fn remove_indexes_for(&mut self, entry: &DirectoryEntry) {
        if let Some(role_list) = self.by_role.get_mut(&entry.role) {
            role_list.retain(|s| s != &entry.id);
            if role_list.is_empty() {
                self.by_role.remove(&entry.role);
            }
        }
        if let Some(depth_list) = self.by_depth.get_mut(&entry.depth) {
            depth_list.retain(|s| s != &entry.id);
            if depth_list.is_empty() {
                self.by_depth.remove(&entry.depth);
            }
        }
    }

    /// Iterate all entries.
    pub fn entries(&self) -> impl Iterator<Item = &DirectoryEntry> + '_ {
        self.entries.values()
    }

    /// Number of registered nodes.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Build a directory from existing instances. Reads `_depth` and
/// `_parentPath` from each node's graph attributes if the compiler supplied
/// them; otherwise defaults depth to 0 and path to `[id]`.
pub fn build_directory(
    nodes: impl IntoIterator<Item = (String, String, crate::graph::Attrs)>,
) -> GraphDirectory {
    let mut directory = GraphDirectory::default();
    for (id, role, attrs) in nodes {
        let depth = attrs
            .get("_depth")
            .and_then(serde_json::Value::as_u64)
            .map_or(0, |v| u32::try_from(v).unwrap_or(0));
        let path = attrs
            .get("_parentPath")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![id.clone()]);
        directory.add_entry(DirectoryEntry {
            id,
            role,
            depth,
            path,
        });
    }
    directory
}
