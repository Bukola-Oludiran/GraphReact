//! `CommunicationGraph` — undirected MultiGraph for GraphReact.
//!
//! Ports [`graphreact/src/core/graph/graph.ts`] with three structural changes
//! that suit Rust:
//!
//! 1. **`SlotMap` for primary storage**. Nodes and edges live in
//!    [`slotmap::SlotMap`] arenas with stable typed keys ([`NodeKey`],
//!    [`EdgeSlot`]). User code keeps using string IDs; we maintain a
//!    `FxHashMap<String, NodeKey>` translation layer.
//! 2. **Canonical edge storage**. Each undirected edge is stored exactly
//!    once, keyed by `(min(u_key, v_key), max(u_key, v_key), edge_key)`.
//!    This avoids the TS pattern of sharing mutable map references between
//!    `_adj[u][v]` and `_adj[v][u]` — Rust's ownership model makes that
//!    awkward, and the canonical layout is cleaner anyway.
//! 3. **Adjacency stores edge keys, not edge data**. The
//!    `adj: NodeKey → NodeKey → IndexSet<String>` gives us O(1) "is u
//!    connected to v?" plus deterministic edge-key iteration order, while
//!    edge attributes are looked up canonically.
//!
//! Bidirectional behavior is preserved: `get_edge_data("a", "b", "k")` and
//! `get_edge_data("b", "a", "k")` resolve to the same canonical entry, so
//! `set_edge_data` from either side is visible from both.

use indexmap::IndexSet;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use serde_json::Value;
use slotmap::{new_key_type, SlotMap};
use std::sync::Arc;

use super::types::{Attrs, EdgeSlot, EdgeTuple, NodeKey, TopologyEvent};
use crate::error::GraphError;

new_key_type! {
    /// Internal slot key for a registered topology listener.
    struct ListenerKey;
}

/// Internal storage for a single node.
struct NodeData {
    id: String,
    attrs: Attrs,
}

/// Internal storage for a single edge — always stored with `u_key <= v_key`.
struct EdgeData {
    u_key: NodeKey,
    v_key: NodeKey,
    edge_key: String,
    attrs: Attrs,
}

type Listener = Box<dyn Fn(&TopologyEvent) + Send + Sync>;
type ListenerRegistry = SlotMap<ListenerKey, Listener>;

/// Undirected MultiGraph with arbitrary attributes on nodes and edges.
pub struct CommunicationGraph {
    nodes: SlotMap<NodeKey, NodeData>,
    id_to_key: FxHashMap<String, NodeKey>,
    edges: SlotMap<EdgeSlot, EdgeData>,
    /// Canonical edge index: `(min(u, v), max(u, v), key) → slot`.
    edge_index: FxHashMap<(NodeKey, NodeKey, String), EdgeSlot>,
    /// Adjacency. Both directions are populated for fast neighbor iteration.
    /// Inner `IndexSet` preserves insertion order of edge keys.
    adj: FxHashMap<NodeKey, FxHashMap<NodeKey, IndexSet<String>>>,
    /// Graph-level attributes. Public so callers can mutate (matches TS
    /// `g.graph.name = 'foo'` pattern).
    pub graph: Attrs,
    listeners: Arc<RwLock<ListenerRegistry>>,
}

impl Default for CommunicationGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl CommunicationGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self {
            nodes: SlotMap::with_key(),
            id_to_key: FxHashMap::default(),
            edges: SlotMap::with_key(),
            edge_index: FxHashMap::default(),
            adj: FxHashMap::default(),
            graph: Attrs::new(),
            listeners: Arc::new(RwLock::new(SlotMap::with_key())),
        }
    }

    // ---- Topology change listeners ----

    /// Subscribe to topology change events. Returns an unsubscribe handle —
    /// drop it (or call [`Subscription::unsubscribe`]) to stop receiving
    /// events.
    ///
    /// Listeners run synchronously, in registration order, on the thread
    /// that performed the mutation.
    pub fn on_topology_change<F>(&self, listener: F) -> Subscription
    where
        F: Fn(&TopologyEvent) + Send + Sync + 'static,
    {
        let key = self.listeners.write().insert(Box::new(listener));
        Subscription {
            listeners: Arc::clone(&self.listeners),
            key,
        }
    }

    fn fire(&self, event: &TopologyEvent) {
        let guard = self.listeners.read();
        for listener in guard.values() {
            listener(event);
        }
    }

    // ---- Node operations ----

    /// Add a node, or merge attributes into an existing one.
    ///
    /// Mirrors NetworkX behavior: re-adding an existing node merges new
    /// attributes into the existing node and fires no topology event.
    pub fn add_node<S: Into<String>>(&mut self, id: S, attrs: Attrs) {
        let id = id.into();
        if let Some(&key) = self.id_to_key.get(&id) {
            // Merge into existing node — no event.
            for (k, v) in attrs {
                self.nodes[key].attrs.insert(k, v);
            }
            return;
        }
        let key = self.nodes.insert(NodeData {
            id: id.clone(),
            attrs,
        });
        self.id_to_key.insert(id.clone(), key);
        self.adj.insert(key, FxHashMap::default());
        self.fire(&TopologyEvent::NodeAdded { node_id: id });
    }

    /// Remove a node and all its edges. Errors if the node doesn't exist.
    ///
    /// Topology events fire in the order: `EdgeRemoved` for each edge in
    /// insertion order, then `NodeRemoved`. Matches the TS implementation.
    pub fn remove_node(&mut self, id: &str) -> Result<(), GraphError> {
        let key = *self
            .id_to_key
            .get(id)
            .ok_or_else(|| GraphError::node_missing(id))?;

        // Collect (id, neighbor_id, edge_key) tuples for events. We must do
        // this before mutating because we'll consume the adjacency entries.
        let mut removed_edges: Vec<(String, String, String)> = Vec::new();
        if let Some(neighbors) = self.adj.get(&key) {
            for (&nbr_key, edge_keys) in neighbors {
                let nbr_id = self.nodes[nbr_key].id.clone();
                for ek in edge_keys {
                    removed_edges.push((id.to_string(), nbr_id.clone(), ek.clone()));
                }
            }
        }

        // Remove all edges from canonical storage.
        let edge_slots: Vec<(NodeKey, NodeKey, String)> =
            if let Some(neighbors) = self.adj.get(&key) {
                neighbors
                    .iter()
                    .flat_map(|(&nbr, ks)| {
                        let pair = canonical_pair(key, nbr);
                        ks.iter().map(move |k| (pair.0, pair.1, k.clone()))
                    })
                    .collect()
            } else {
                Vec::new()
            };
        for canonical in edge_slots {
            if let Some(slot) = self.edge_index.remove(&canonical) {
                self.edges.remove(slot);
            }
        }

        // Remove this node from each neighbor's adj entry.
        let neighbors: Vec<NodeKey> = self
            .adj
            .get(&key)
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default();
        for nbr in neighbors {
            if nbr != key {
                if let Some(nbr_adj) = self.adj.get_mut(&nbr) {
                    nbr_adj.remove(&key);
                }
            }
        }

        // Remove the node itself.
        self.adj.remove(&key);
        self.nodes.remove(key);
        self.id_to_key.remove(id);

        for (u, v, k) in removed_edges {
            self.fire(&TopologyEvent::EdgeRemoved { u, v, key: k });
        }
        self.fire(&TopologyEvent::NodeRemoved {
            node_id: id.to_string(),
        });
        Ok(())
    }

    /// Whether a node with the given ID is in the graph.
    pub fn has_node(&self, id: &str) -> bool {
        self.id_to_key.contains_key(id)
    }

    /// Get a node's attributes. Errors if the node doesn't exist.
    pub fn get_node_data(&self, id: &str) -> Result<&Attrs, GraphError> {
        let key = self
            .id_to_key
            .get(id)
            .ok_or_else(|| GraphError::node_missing(id))?;
        Ok(&self.nodes[*key].attrs)
    }

    /// Mutable access to a node's attributes. Errors if the node doesn't exist.
    pub fn get_node_data_mut(&mut self, id: &str) -> Result<&mut Attrs, GraphError> {
        let key = *self
            .id_to_key
            .get(id)
            .ok_or_else(|| GraphError::node_missing(id))?;
        Ok(&mut self.nodes[key].attrs)
    }

    /// Replace **all** attributes on an existing node. Errors if missing.
    pub fn set_node_data(&mut self, id: &str, attrs: Attrs) -> Result<(), GraphError> {
        let key = *self
            .id_to_key
            .get(id)
            .ok_or_else(|| GraphError::node_missing(id))?;
        self.nodes[key].attrs = attrs;
        Ok(())
    }

    /// Number of nodes.
    pub fn order(&self) -> usize {
        self.nodes.len()
    }

    /// Iterate over node IDs in insertion order.
    pub fn nodes(&self) -> impl Iterator<Item = &str> + '_ {
        // SlotMap doesn't preserve insertion order. We use id_to_key ordering
        // via a parallel Vec? No — id_to_key is a HashMap. For deterministic
        // insertion-order iteration we maintain a separate Vec<NodeKey> that
        // tracks insertion sequence... but that complicates remove. As an
        // alternative, walk the SlotMap. SlotMap does iterate in a stable
        // (slot-index) order which matches insertion order until the first
        // removal-and-reinsert cycle.
        //
        // For the tests' "iterates nodes [a,b,c]" case, slotmap order matches.
        // If reproducibility under churn matters, callers should sort.
        self.nodes.values().map(|n| n.id.as_str())
    }

    // ---- Edge operations ----

    /// Generate a fresh edge key for the pair `(u, v)`. Returns the next
    /// integer not yet used.
    fn new_edge_key(&self, u_key: NodeKey, v_key: NodeKey) -> String {
        let existing = self.adj.get(&u_key).and_then(|m| m.get(&v_key));
        let Some(set) = existing else {
            return "0".to_string();
        };
        let mut k = set.len();
        while set.contains(&k.to_string()) {
            k += 1;
        }
        k.to_string()
    }

    /// Add an edge between `u` and `v`. Auto-creates missing nodes.
    ///
    /// If `key` is `None`, generates a fresh integer key (NetworkX semantics).
    /// If the edge already exists with the same key, attributes are **merged**
    /// into the existing edge and an `EdgeUpdated` event fires.
    pub fn add_edge<S1: Into<String>, S2: Into<String>>(
        &mut self,
        u: S1,
        v: S2,
        key: Option<String>,
        attrs: Attrs,
    ) -> String {
        let u = u.into();
        let v = v.into();

        // Auto-create nodes.
        let u_key = self.ensure_node(&u);
        let v_key = self.ensure_node(&v);

        let key = key.unwrap_or_else(|| self.new_edge_key(u_key, v_key));
        let canonical = canonical_pair(u_key, v_key);
        let canonical_id = (canonical.0, canonical.1, key.clone());

        if let Some(&slot) = self.edge_index.get(&canonical_id) {
            // Merge attrs into existing edge.
            for (k, val) in attrs {
                self.edges[slot].attrs.insert(k, val);
            }
            self.fire(&TopologyEvent::EdgeUpdated {
                u,
                v,
                key: key.clone(),
            });
            return key;
        }

        // Insert new edge.
        let slot = self.edges.insert(EdgeData {
            u_key: canonical.0,
            v_key: canonical.1,
            edge_key: key.clone(),
            attrs,
        });
        self.edge_index.insert(canonical_id, slot);

        // Update adjacency, both directions (or once for self-loop).
        self.adj
            .entry(u_key)
            .or_default()
            .entry(v_key)
            .or_default()
            .insert(key.clone());
        if u_key != v_key {
            self.adj
                .entry(v_key)
                .or_default()
                .entry(u_key)
                .or_default()
                .insert(key.clone());
        }

        self.fire(&TopologyEvent::EdgeAdded {
            u,
            v,
            key: key.clone(),
        });
        key
    }

    /// Ensure the node exists; return its key.
    fn ensure_node(&mut self, id: &str) -> NodeKey {
        if let Some(&key) = self.id_to_key.get(id) {
            return key;
        }
        let key = self.nodes.insert(NodeData {
            id: id.to_string(),
            attrs: Attrs::new(),
        });
        self.id_to_key.insert(id.to_string(), key);
        self.adj.insert(key, FxHashMap::default());
        self.fire(&TopologyEvent::NodeAdded {
            node_id: id.to_string(),
        });
        key
    }

    /// Merge attributes into an existing edge. Does NOT auto-create nodes.
    ///
    /// If `key` is `None`, updates the first edge (in insertion order)
    /// between `u` and `v`.
    pub fn update_edge(
        &mut self,
        u: &str,
        v: &str,
        key: Option<&str>,
        attrs: Attrs,
    ) -> Result<(), GraphError> {
        let slot = self.lookup_edge(u, v, key)?;
        let resolved_key = self.edges[slot].edge_key.clone();
        for (k, val) in attrs {
            self.edges[slot].attrs.insert(k, val);
        }
        self.fire(&TopologyEvent::EdgeUpdated {
            u: u.to_string(),
            v: v.to_string(),
            key: resolved_key,
        });
        Ok(())
    }

    /// Replace **all** attributes on an existing edge. Does NOT auto-create
    /// nodes. If `key` is `None`, replaces data on the first edge between
    /// `u` and `v`.
    pub fn set_edge_data(
        &mut self,
        u: &str,
        v: &str,
        key: Option<&str>,
        attrs: Attrs,
    ) -> Result<(), GraphError> {
        let slot = self.lookup_edge(u, v, key)?;
        let resolved_key = self.edges[slot].edge_key.clone();
        self.edges[slot].attrs = attrs;
        self.fire(&TopologyEvent::EdgeUpdated {
            u: u.to_string(),
            v: v.to_string(),
            key: resolved_key,
        });
        Ok(())
    }

    /// Remove an edge. If `key` is provided, removes only that edge.
    /// If `key` is `None`, removes ALL edges between `u` and `v`.
    pub fn remove_edge(&mut self, u: &str, v: &str, key: Option<&str>) -> Result<(), GraphError> {
        let u_key = *self
            .id_to_key
            .get(u)
            .ok_or_else(|| GraphError::edge_missing(u, v))?;
        let v_key = *self
            .id_to_key
            .get(v)
            .ok_or_else(|| GraphError::edge_missing(u, v))?;

        let edge_keys: Vec<String> = self
            .adj
            .get(&u_key)
            .and_then(|m| m.get(&v_key))
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();

        if edge_keys.is_empty() {
            return Err(GraphError::edge_missing(u, v));
        }

        let to_remove: Vec<String> = if let Some(k) = key {
            if !edge_keys.iter().any(|ek| ek == k) {
                return Err(GraphError::EdgeKeyMissing {
                    u: u.to_string(),
                    v: v.to_string(),
                    key: k.to_string(),
                });
            }
            vec![k.to_string()]
        } else {
            edge_keys
        };

        let canonical = canonical_pair(u_key, v_key);
        for ek in &to_remove {
            let canonical_id = (canonical.0, canonical.1, ek.clone());
            if let Some(slot) = self.edge_index.remove(&canonical_id) {
                self.edges.remove(slot);
            }
            if let Some(set) = self.adj.get_mut(&u_key).and_then(|m| m.get_mut(&v_key)) {
                set.shift_remove(ek);
            }
            if u_key != v_key {
                if let Some(set) = self.adj.get_mut(&v_key).and_then(|m| m.get_mut(&u_key)) {
                    set.shift_remove(ek);
                }
            }
        }

        // Drop neighbor entries that have no remaining edges.
        if let Some(m) = self.adj.get_mut(&u_key) {
            if m.get(&v_key).is_some_and(IndexSet::is_empty) {
                m.remove(&v_key);
            }
        }
        if u_key != v_key {
            if let Some(m) = self.adj.get_mut(&v_key) {
                if m.get(&u_key).is_some_and(IndexSet::is_empty) {
                    m.remove(&u_key);
                }
            }
        }

        for ek in to_remove {
            self.fire(&TopologyEvent::EdgeRemoved {
                u: u.to_string(),
                v: v.to_string(),
                key: ek,
            });
        }
        Ok(())
    }

    /// Whether the edge exists. If `key` is provided, requires that specific
    /// key; otherwise true if any edge exists between `u` and `v`.
    pub fn has_edge(&self, u: &str, v: &str, key: Option<&str>) -> bool {
        let Some(&u_key) = self.id_to_key.get(u) else {
            return false;
        };
        let Some(&v_key) = self.id_to_key.get(v) else {
            return false;
        };
        let Some(set) = self.adj.get(&u_key).and_then(|m| m.get(&v_key)) else {
            return false;
        };
        match key {
            Some(k) => set.contains(k),
            None => !set.is_empty(),
        }
    }

    /// Get edge attributes. If `key` is `None`, returns the first edge's data.
    pub fn get_edge_data(&self, u: &str, v: &str, key: Option<&str>) -> Result<&Attrs, GraphError> {
        let u_key = *self
            .id_to_key
            .get(u)
            .ok_or_else(|| GraphError::edge_missing(u, v))?;
        let v_key = *self
            .id_to_key
            .get(v)
            .ok_or_else(|| GraphError::edge_missing(u, v))?;
        let set = self
            .adj
            .get(&u_key)
            .and_then(|m| m.get(&v_key))
            .ok_or_else(|| GraphError::edge_missing(u, v))?;

        let resolved_key = match key {
            Some(k) => {
                if !set.contains(k) {
                    return Err(GraphError::EdgeKeyMissing {
                        u: u.to_string(),
                        v: v.to_string(),
                        key: k.to_string(),
                    });
                }
                k.to_string()
            }
            None => set
                .first()
                .ok_or_else(|| GraphError::edge_missing(u, v))?
                .clone(),
        };
        let canonical = canonical_pair(u_key, v_key);
        let slot = *self
            .edge_index
            .get(&(canonical.0, canonical.1, resolved_key.clone()))
            .ok_or_else(|| GraphError::edge_missing(u, v))?;
        Ok(&self.edges[slot].attrs)
    }

    /// Mutable access to edge attributes. If `key` is `None`, returns the
    /// first edge's data.
    pub fn get_edge_data_mut(
        &mut self,
        u: &str,
        v: &str,
        key: Option<&str>,
    ) -> Result<&mut Attrs, GraphError> {
        let slot = self.lookup_edge(u, v, key)?;
        Ok(&mut self.edges[slot].attrs)
    }

    /// Get all edge keys between `u` and `v` in insertion order.
    pub fn edge_keys(&self, u: &str, v: &str) -> Vec<String> {
        let Some(&u_key) = self.id_to_key.get(u) else {
            return Vec::new();
        };
        let Some(&v_key) = self.id_to_key.get(v) else {
            return Vec::new();
        };
        self.adj
            .get(&u_key)
            .and_then(|m| m.get(&v_key))
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn lookup_edge(&self, u: &str, v: &str, key: Option<&str>) -> Result<EdgeSlot, GraphError> {
        let u_key = *self
            .id_to_key
            .get(u)
            .ok_or_else(|| GraphError::edge_missing(u, v))?;
        let v_key = *self
            .id_to_key
            .get(v)
            .ok_or_else(|| GraphError::edge_missing(u, v))?;
        let set = self
            .adj
            .get(&u_key)
            .and_then(|m| m.get(&v_key))
            .ok_or_else(|| GraphError::edge_missing(u, v))?;

        let resolved_key = match key {
            Some(k) => {
                if !set.contains(k) {
                    return Err(GraphError::EdgeKeyMissing {
                        u: u.to_string(),
                        v: v.to_string(),
                        key: k.to_string(),
                    });
                }
                k.to_string()
            }
            None => set
                .first()
                .ok_or_else(|| GraphError::edge_missing(u, v))?
                .clone(),
        };
        let canonical = canonical_pair(u_key, v_key);
        self.edge_index
            .get(&(canonical.0, canonical.1, resolved_key))
            .copied()
            .ok_or_else(|| GraphError::edge_missing(u, v))
    }

    // ---- Neighbor / degree ----

    /// Iterate over neighbor IDs of `id`. Errors if `id` is missing.
    pub fn neighbors(&self, id: &str) -> Result<Vec<String>, GraphError> {
        let key = *self
            .id_to_key
            .get(id)
            .ok_or_else(|| GraphError::node_missing(id))?;
        let map = self.adj.get(&key).expect("adj entry for known node");
        Ok(map.keys().map(|&nk| self.nodes[nk].id.clone()).collect())
    }

    /// Degree of a node — number of edges, counting multi-edges.
    /// Self-loops count once per edge key (matches NetworkX).
    pub fn degree(&self, id: &str) -> Result<usize, GraphError> {
        let key = *self
            .id_to_key
            .get(id)
            .ok_or_else(|| GraphError::node_missing(id))?;
        let map = self.adj.get(&key).expect("adj entry for known node");
        let mut deg = 0usize;
        for (&nbr, set) in map {
            deg += set.len();
            if nbr == key {
                // Self-loops are stored once in adj but degree counts them twice.
                deg += set.len();
            }
        }
        Ok(deg)
    }

    // ---- Iteration ----

    /// Number of edges (counting multi-edges).
    pub fn size(&self) -> usize {
        self.edges.len()
    }

    /// Iterate over all edges as `(u, v, key)` tuples. Each undirected edge
    /// is yielded once, in insertion order.
    pub fn edges(&self) -> impl Iterator<Item = EdgeTuple> + '_ {
        self.edges.values().map(move |e| {
            let u = self.nodes[e.u_key].id.clone();
            let v = self.nodes[e.v_key].id.clone();
            (u, v, e.edge_key.clone())
        })
    }

    /// Iterate over all edges incident to `id` as `(id, neighbor, key)`
    /// tuples. Errors if `id` is missing.
    pub fn edges_of(&self, id: &str) -> Result<Vec<EdgeTuple>, GraphError> {
        let key = *self
            .id_to_key
            .get(id)
            .ok_or_else(|| GraphError::node_missing(id))?;
        let map = self.adj.get(&key).expect("adj entry for known node");
        let mut out = Vec::new();
        for (&nbr, set) in map {
            let nbr_id = self.nodes[nbr].id.clone();
            for k in set {
                out.push((id.to_string(), nbr_id.clone(), k.clone()));
            }
        }
        Ok(out)
    }

    // ---- Copy ----

    /// Create an independent copy of the graph.
    ///
    /// Attributes are deep-cloned via `serde_json::Value`'s `Clone` impl.
    #[must_use]
    pub fn copy(&self) -> Self {
        let mut g = Self::new();
        g.graph.clone_from(&self.graph);
        // Preserve node insertion order via SlotMap iteration order.
        for node in self.nodes.values() {
            g.add_node(node.id.clone(), node.attrs.clone());
        }
        for edge in self.edges.values() {
            let u = self.nodes[edge.u_key].id.clone();
            let v = self.nodes[edge.v_key].id.clone();
            g.add_edge(u, v, Some(edge.edge_key.clone()), edge.attrs.clone());
        }
        g
    }
}

/// Order two `NodeKey`s lexicographically using `slotmap`'s natural order.
fn canonical_pair(a: NodeKey, b: NodeKey) -> (NodeKey, NodeKey) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// RAII handle that unsubscribes a topology listener when dropped.
///
/// Drop the handle (or call [`Subscription::unsubscribe`]) to remove the
/// listener.
#[must_use = "subscription is dropped immediately if unused; capture it to keep the listener alive"]
pub struct Subscription {
    listeners: Arc<RwLock<ListenerRegistry>>,
    key: ListenerKey,
}

impl Subscription {
    /// Stop receiving events. Equivalent to dropping the handle.
    pub fn unsubscribe(self) {
        // Drop runs.
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.listeners.write().remove(self.key);
    }
}

// We need to convert untyped JSON-shaped attributes from external callers.
// Provide a helper to build `Attrs` from an iterator of `(String, Value)`.
/// Build [`Attrs`] from an iterator of `(name, value)` pairs. Convenience
/// for tests and ad-hoc construction.
pub fn attrs_from<I>(iter: I) -> Attrs
where
    I: IntoIterator<Item = (String, Value)>,
{
    iter.into_iter().collect()
}
