//! Leiden community detection — Traag, Waltman, van Eck (2019), "From
//! Louvain to Leiden: guaranteeing well-connected communities."
//!
//! Three phases per iteration:
//!
//! 1. **Local moving (`move_nodes_fast`)** — visit each node in
//!    randomised queue order; move to whichever neighbour community
//!    gives the largest positive modularity gain. Re-enqueue
//!    neighbours whose context just changed. Continue until queue
//!    empty.
//!
//! 2. **Refinement (`refine_partition`)** — within each phase-1
//!    community, redistribute nodes among sub-communities, only
//!    allowing merges that keep the resulting sub-community
//!    "well-connected" relative to the rest of the original
//!    community. This is the Leiden improvement over Louvain — it
//!    rules out the disconnected-community pathology.
//!
//! 3. **Aggregation (`aggregate`)** — build a new graph where each
//!    refined sub-community is a single super-node; super-edges carry
//!    the summed weight of all crossings between the merged sub-
//!    communities. The next iteration runs on the aggregated graph,
//!    starting from the partition that maps super-nodes back to
//!    phase-1 community labels.
//!
//! Iteration stops when phase 1 produces no moves (partition stable).
//!
//! ## Determinism
//!
//! Node ordering uses a `rand_chacha` PRNG seeded from
//! [`LeidenOptions::seed`]. Same seed + same graph → same partition.
//! When seed is `None`, a thread-RNG-derived seed is used (run-to-run
//! variance).
//!
//! ## Performance shape
//!
//! Modularity gain is computed incrementally by maintaining
//! per-community degree sums (`k_c`) and per-(node, community)
//! cross-weight sums on demand. Edge enumeration walks the
//! [`CommunicationGraph`]'s adjacency lists; aggregated graphs are
//! built fresh as
//! [`CommunicationGraph`][graphreact_core::CommunicationGraph]
//! instances to keep one data model throughout.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rustc_hash::{FxHashMap, FxHashSet};
use thiserror::Error;

use graphreact_core::CommunicationGraph;

use super::modularity::edge_weight;

/// Map from node id → community id. The Leiden output type and the
/// modularity input type. Communities are dense `u64`s starting from
/// `0`, but the algorithm doesn't rely on density — gaps are fine.
pub type PartitionMap = FxHashMap<String, u64>;

#[derive(Debug, Error)]
pub enum LeidenError {
    #[error("invalid options: {0}")]
    InvalidOptions(&'static str),
}

/// Tunable parameters for the Leiden run.
#[derive(Debug, Clone)]
pub struct LeidenOptions {
    /// Modularity resolution parameter `γ`. Default `1.0` (classic
    /// Newman–Girvan modularity).
    pub resolution: f64,
    /// Edge attribute holding the float weight. Default `"weight"`;
    /// missing/non-numeric values default to `1.0`.
    pub weight_attribute: String,
    /// Maximum number of (move + refine + aggregate) iterations.
    /// Default `10`. Each iteration runs phase 1 to convergence.
    pub max_iterations: usize,
    /// Seed for the node-shuffling PRNG. `None` derives from thread
    /// RNG (non-deterministic across runs).
    pub seed: Option<u64>,
}

impl Default for LeidenOptions {
    fn default() -> Self {
        Self {
            resolution: 1.0,
            weight_attribute: "weight".to_string(),
            max_iterations: 10,
            seed: None,
        }
    }
}

/// Run the Leiden algorithm. Returns the final flattened partition —
/// one community id per original node.
pub fn leiden(
    graph: &CommunicationGraph,
    opts: &LeidenOptions,
) -> Result<PartitionMap, LeidenError> {
    if !(0.0..=10.0).contains(&opts.resolution) {
        return Err(LeidenError::InvalidOptions("resolution must be in [0, 10]"));
    }
    if opts.max_iterations == 0 {
        return Err(LeidenError::InvalidOptions("max_iterations must be >= 1"));
    }

    let original_nodes: Vec<String> = graph.nodes().map(String::from).collect();
    if original_nodes.is_empty() {
        return Ok(PartitionMap::default());
    }

    // Build the working snapshot. We operate on a node-index-keyed
    // representation for hot-path speed; communities are also indexed.
    let mut current = SnapshotState::from_graph(graph, &opts.weight_attribute);
    // Each level of the recursion maps current-graph nodes to their
    // original-node members; the bottom level is the identity mapping.
    let mut levels: Vec<Vec<Vec<usize>>> = vec![{
        // Level 0: node i maps to itself (singleton group).
        (0..current.n).map(|i| vec![i]).collect()
    }];

    let mut rng = match opts.seed {
        Some(s) => ChaCha8Rng::seed_from_u64(s),
        None => ChaCha8Rng::seed_from_u64(rand::thread_rng().r#gen()),
    };

    for _ in 0..opts.max_iterations {
        // Phase 1: local moving.
        let moved = move_nodes_fast(&mut current, opts.resolution, &mut rng);

        // Termination: the partition is stable AND every community
        // has at least one node beyond the singleton level. If phase
        // 1 didn't change anything AND the partition isn't trivially
        // singleton, we've reached a stable state.
        if !moved && current.communities_are_singleton() {
            // Already at full singleton — no further refinement
            // would help.
            break;
        }
        if !moved {
            // Stable but non-singleton: we're done.
            break;
        }

        // Phase 2: refinement.
        let refined = refine_partition(&current, opts.resolution, &mut rng);

        // Phase 3: aggregate by refined partition. The next-level
        // partition for the aggregated graph is induced by the
        // PRE-refinement community of each refined sub-community
        // (since refined sub-communities live inside phase-1
        // communities by construction).
        let (aggregated, member_map, refined_to_phase1) = aggregate(&current, &refined);
        current = aggregated;
        // The phase-1 community of each new node = the phase-1
        // community of any of its (refined) members.
        for (new_node, phase1_comm) in refined_to_phase1.iter().enumerate() {
            current.node_community[new_node] = *phase1_comm;
        }
        current.rebuild_community_index();
        levels.push(member_map);
    }

    // Flatten the level stack: a final-graph node corresponds to a
    // tree of original nodes; each node carries the community id from
    // the topmost level.
    let mut flat = PartitionMap::default();
    for (top_node, &comm) in current.node_community.iter().enumerate() {
        let mut frontier = vec![top_node];
        for level in levels.iter().rev() {
            let mut next = Vec::new();
            for n in frontier.drain(..) {
                next.extend_from_slice(&level[n]);
            }
            frontier = next;
        }
        for original_idx in frontier {
            flat.insert(original_nodes[original_idx].clone(), comm);
        }
    }

    Ok(flat)
}

// ===========================================================================
// Internal — index-keyed graph snapshot
// ===========================================================================

/// Working representation: nodes 0..n, undirected adjacency with
/// per-edge weight, plus community-state caches that the phase
/// implementations mutate.
struct SnapshotState {
    n: usize,
    /// Adjacency: `adj[u] = [(v, weight), ...]`. Self-loops appear
    /// once with their actual weight.
    adj: Vec<Vec<(usize, f64)>>,
    /// Sum of incident edge weights per node (self-loops count
    /// twice, as is conventional for undirected modularity).
    node_degree: Vec<f64>,
    /// Total edge weight `m` (sum over edges; self-loops counted
    /// once, but they're half-edges so this matches modularity's
    /// 2m formulation when paired with `node_degree`).
    total_weight: f64,
    /// Current community assignment per node.
    node_community: Vec<u64>,
    /// `community_degree[c] = Σ k_v for v in community c`.
    community_degree: FxHashMap<u64, f64>,
    /// Members of each community (for refinement + aggregation).
    community_members: FxHashMap<u64, FxHashSet<usize>>,
}

impl SnapshotState {
    fn from_graph(graph: &CommunicationGraph, weight_attr: &str) -> Self {
        let nodes: Vec<String> = graph.nodes().map(String::from).collect();
        let n = nodes.len();
        let id_to_idx: FxHashMap<String, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), i))
            .collect();

        let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        let mut node_degree = vec![0.0_f64; n];
        let mut total_weight = 0.0_f64;
        let mut seen_edges: FxHashSet<(usize, usize, String)> = FxHashSet::default();

        for (uid, vid, key) in graph.edges() {
            let u = *id_to_idx.get(&uid).expect("edge endpoint indexed");
            let v = *id_to_idx.get(&vid).expect("edge endpoint indexed");
            // Edges in `CommunicationGraph::edges()` already represent
            // each undirected edge once (canonical ordering). Skip
            // dedup unless we see the (u, v, key) repeated.
            let canonical = (u.min(v), u.max(v), key.clone());
            if !seen_edges.insert(canonical) {
                continue;
            }
            let attrs = graph
                .get_edge_data(&uid, &vid, Some(&key))
                .expect("edge from edges() must have data");
            let w = edge_weight(attrs, weight_attr);
            if u == v {
                // Self-loop: contributes 2w to node_degree per
                // convention (each "endpoint" of the loop adds w),
                // and w to total_weight (counted once).
                adj[u].push((v, w));
                node_degree[u] += 2.0 * w;
            } else {
                adj[u].push((v, w));
                adj[v].push((u, w));
                node_degree[u] += w;
                node_degree[v] += w;
            }
            total_weight += w;
        }

        // Initial partition: each node its own community.
        let mut node_community = vec![0u64; n];
        let mut community_degree: FxHashMap<u64, f64> = FxHashMap::default();
        let mut community_members: FxHashMap<u64, FxHashSet<usize>> = FxHashMap::default();
        for i in 0..n {
            let c = i as u64;
            node_community[i] = c;
            community_degree.insert(c, node_degree[i]);
            community_members.entry(c).or_default().insert(i);
        }

        Self {
            n,
            adj,
            node_degree,
            total_weight,
            node_community,
            community_degree,
            community_members,
        }
    }

    fn communities_are_singleton(&self) -> bool {
        self.community_members.values().all(|m| m.len() <= 1)
    }

    /// Sum of edge weights from node `v` to nodes currently in
    /// community `c`, EXCLUDING `v` itself. (Used in modularity-gain
    /// formulas where v's contribution to its own community is
    /// handled separately.)
    fn weight_to_community(
        &self,
        v: usize,
        c: u64,
        members: &FxHashMap<u64, FxHashSet<usize>>,
    ) -> f64 {
        let Some(community_set) = members.get(&c) else {
            return 0.0;
        };
        let mut total = 0.0;
        for &(nbr, w) in &self.adj[v] {
            if nbr == v {
                continue;
            }
            if community_set.contains(&nbr) {
                total += w;
            }
        }
        total
    }

    fn rebuild_community_index(&mut self) {
        self.community_degree.clear();
        self.community_members.clear();
        for v in 0..self.n {
            let c = self.node_community[v];
            *self.community_degree.entry(c).or_insert(0.0) += self.node_degree[v];
            self.community_members.entry(c).or_default().insert(v);
        }
    }
}

// ===========================================================================
// Phase 1 — move_nodes_fast
// ===========================================================================

fn move_nodes_fast(state: &mut SnapshotState, resolution: f64, rng: &mut ChaCha8Rng) -> bool {
    let mut queue: Vec<usize> = (0..state.n).collect();
    fisher_yates_shuffle(&mut queue, rng);
    let mut in_queue: Vec<bool> = vec![true; state.n];
    let mut any_moved = false;

    while let Some(v) = queue.pop() {
        in_queue[v] = false;
        let current_comm = state.node_community[v];

        // Candidate communities = current ∪ {neighbour communities}.
        // Plus the empty / singleton case (move v to its own new
        // community), but that's only useful when v is the sole node
        // in its current community — covered implicitly because
        // community_degree[current] == k_v.
        let mut candidates: FxHashSet<u64> = FxHashSet::default();
        candidates.insert(current_comm);
        for &(nbr, _) in &state.adj[v] {
            if nbr == v {
                continue;
            }
            candidates.insert(state.node_community[nbr]);
        }

        let k_v = state.node_degree[v];
        let two_m = 2.0 * state.total_weight;
        if two_m == 0.0 {
            continue;
        }

        let weight_to_current =
            state.weight_to_community(v, current_comm, &state.community_members);
        let k_current_minus_v = state
            .community_degree
            .get(&current_comm)
            .copied()
            .unwrap_or(0.0)
            - k_v;
        // Quality contribution of v in its current community:
        // contribution(v, S) = w(v, S\{v}) / m
        //                    - γ * k_v * (k_S - k_v) / (2m^2)
        let q_current = weight_to_current / state.total_weight
            - resolution * k_v * k_current_minus_v / (two_m * two_m / 2.0);
        // (2m^2 = 2 * (2m/2)^2 ... we just simplified by dividing.)
        // The conventional formula: Δ contribution from a community
        // term is `(intra_weight / 2m) - γ * (k_c / 2m)^2`. Moving v
        // out of S subtracts v's contribution; we encode it as:
        //   q(v in S) = w(v, S\v) / m  -  γ k_v (k_S - k_v) / (2m)^2
        // Multiplying through: the / m and the / (2m)^2 below match.

        let mut best_gain = 0.0;
        let mut best_comm = current_comm;
        let mut best_weight_to_target: Option<f64> = None;
        let mut best_k_target: Option<f64> = None;

        for &candidate in &candidates {
            if candidate == current_comm {
                continue;
            }
            let weight_to_target =
                state.weight_to_community(v, candidate, &state.community_members);
            let k_target = state
                .community_degree
                .get(&candidate)
                .copied()
                .unwrap_or(0.0);
            let q_target = weight_to_target / state.total_weight
                - resolution * k_v * k_target / (two_m * two_m / 2.0);
            let gain = q_target - q_current;
            if gain > best_gain + 1e-12 {
                best_gain = gain;
                best_comm = candidate;
                best_weight_to_target = Some(weight_to_target);
                best_k_target = Some(k_target);
            }
        }

        if best_comm == current_comm {
            continue;
        }

        // Apply move.
        state.node_community[v] = best_comm;
        let _ = best_weight_to_target;
        let _ = best_k_target;
        // Update community_degree
        if let Some(d) = state.community_degree.get_mut(&current_comm) {
            *d -= k_v;
        }
        *state.community_degree.entry(best_comm).or_insert(0.0) += k_v;
        // Update community_members
        if let Some(m) = state.community_members.get_mut(&current_comm) {
            m.remove(&v);
            if m.is_empty() {
                state.community_members.remove(&current_comm);
                state.community_degree.remove(&current_comm);
            }
        }
        state
            .community_members
            .entry(best_comm)
            .or_default()
            .insert(v);

        any_moved = true;
        // Re-enqueue neighbours not in the new community and not
        // already queued.
        for &(nbr, _) in &state.adj[v] {
            if nbr == v {
                continue;
            }
            if state.node_community[nbr] == best_comm {
                continue;
            }
            if !in_queue[nbr] {
                queue.push(nbr);
                in_queue[nbr] = true;
            }
        }
    }

    any_moved
}

// ===========================================================================
// Phase 2 — refine_partition
// ===========================================================================

/// Refine the phase-1 partition: within each community, run a local
/// moving pass on a singleton starting partition, but only allow
/// merges between sub-communities that are well-connected to the rest
/// of their phase-1 community.
///
/// "Well-connected" criterion (Traag 2019, Eq. 2): community C ⊂ S is
/// well-connected if `E(C, S\C) ≥ γ * k_C * (k_S - k_C) / 2m`.
fn refine_partition(
    state: &SnapshotState,
    resolution: f64,
    rng: &mut ChaCha8Rng,
) -> RefinedPartition {
    let mut node_subcommunity: Vec<u64> = (0..state.n).map(|v| v as u64).collect();
    let mut sub_degree: FxHashMap<u64, f64> = (0..state.n)
        .map(|v| (v as u64, state.node_degree[v]))
        .collect();
    let mut sub_members: FxHashMap<u64, FxHashSet<usize>> = (0..state.n)
        .map(|v| (v as u64, [v].into_iter().collect()))
        .collect();

    // Iterate phase-1 communities in id order so the result is
    // deterministic given the same RNG state.
    let mut phase1_ids: Vec<u64> = state.community_members.keys().copied().collect();
    phase1_ids.sort_unstable();

    let two_m = 2.0 * state.total_weight;
    if two_m == 0.0 {
        return RefinedPartition {
            node_subcommunity,
            sub_members,
        };
    }

    for phase1_id in phase1_ids {
        let s_members: Vec<usize> = state
            .community_members
            .get(&phase1_id)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        if s_members.len() <= 1 {
            continue;
        }
        let s_degree = state
            .community_degree
            .get(&phase1_id)
            .copied()
            .unwrap_or(0.0);

        // Visit nodes in randomised order.
        let mut order = s_members.clone();
        fisher_yates_shuffle(&mut order, rng);

        for v in order {
            // Only consider moving nodes that are still in their
            // singleton sub-community AND well-connected to the rest
            // of S.
            let cur_sub = node_subcommunity[v];
            if let Some(members) = sub_members.get(&cur_sub) {
                if members.len() != 1 {
                    continue;
                }
            }

            let k_v = state.node_degree[v];
            let weight_v_to_s_minus_v = sum_weights(state, v, &s_members) - 0.0;
            let well_connected_threshold = resolution * k_v * (s_degree - k_v) / two_m;
            if weight_v_to_s_minus_v < well_connected_threshold {
                continue;
            }

            // Find candidate sub-communities (other singletons or
            // already-merged groups) within S that are themselves
            // well-connected to S\C.
            let mut best_gain = 0.0;
            let mut best_target = cur_sub;

            let candidates: FxHashSet<u64> = state.adj[v]
                .iter()
                .filter(|(nbr, _)| state.node_community[*nbr] == phase1_id && *nbr != v)
                .map(|(nbr, _)| node_subcommunity[*nbr])
                .filter(|c| *c != cur_sub)
                .collect();

            for candidate in candidates {
                let candidate_members = match sub_members.get(&candidate) {
                    Some(m) => m,
                    None => continue,
                };
                let k_c = sub_degree.get(&candidate).copied().unwrap_or(0.0);
                // Well-connectedness of candidate sub-community
                // against rest of S.
                let weight_c_to_s_minus_c =
                    members_weight_to_set_excluding(state, candidate_members, &s_members);
                if weight_c_to_s_minus_c < resolution * k_c * (s_degree - k_c) / two_m {
                    continue;
                }
                // Modularity gain of merging v into candidate.
                let weight_v_to_c = sum_weights_to_set(state, v, candidate_members);
                let gain = weight_v_to_c / state.total_weight
                    - resolution * k_v * k_c / (two_m * two_m / 2.0);
                if gain > best_gain + 1e-12 {
                    best_gain = gain;
                    best_target = candidate;
                }
            }

            if best_target == cur_sub {
                continue;
            }

            // Perform merge.
            node_subcommunity[v] = best_target;
            *sub_degree.entry(best_target).or_insert(0.0) += k_v;
            sub_degree.remove(&cur_sub);
            sub_members.entry(best_target).or_default().insert(v);
            sub_members.remove(&cur_sub);
        }
    }

    RefinedPartition {
        node_subcommunity,
        sub_members,
    }
}

struct RefinedPartition {
    node_subcommunity: Vec<u64>,
    sub_members: FxHashMap<u64, FxHashSet<usize>>,
}

// ===========================================================================
// Phase 3 — aggregate
// ===========================================================================

/// Build a new snapshot where each refined sub-community is one
/// node. Returns the new state, the member-mapping (new-node → list
/// of old-node indices), and the phase-1 community id of each new
/// node (used to seed the next iteration's coarse partition).
fn aggregate(
    state: &SnapshotState,
    refined: &RefinedPartition,
) -> (SnapshotState, Vec<Vec<usize>>, Vec<u64>) {
    // Map sub-community id → new node index.
    let mut sub_to_new: FxHashMap<u64, usize> = FxHashMap::default();
    let mut member_map: Vec<Vec<usize>> = Vec::new();
    let mut new_to_phase1: Vec<u64> = Vec::new();
    for (&sub_id, members) in &refined.sub_members {
        let new_idx = member_map.len();
        sub_to_new.insert(sub_id, new_idx);
        let members_vec: Vec<usize> = members.iter().copied().collect();
        // Phase-1 community = the phase-1 community of any member
        // (refinement keeps sub-communities inside phase-1 communities).
        let phase1 = members_vec
            .first()
            .map(|&m| state.node_community[m])
            .unwrap_or(0);
        new_to_phase1.push(phase1);
        member_map.push(members_vec);
    }
    let n_new = member_map.len();

    // Aggregate edges: each old edge between sub-communities
    // contributes to a new edge between their super-nodes; intra-
    // sub-community edges become self-loops.
    let mut edge_acc: FxHashMap<(usize, usize), f64> = FxHashMap::default();
    let mut new_node_degree = vec![0.0_f64; n_new];
    let mut total_weight = 0.0_f64;

    let mut visited: FxHashSet<(usize, usize)> = FxHashSet::default();
    for u in 0..state.n {
        for &(v, w) in &state.adj[u] {
            // Each undirected edge is stored twice in `adj`; visit
            // once via the smaller-index canonical order. Self-loops
            // visit once.
            if u != v {
                let canon = (u.min(v), u.max(v));
                if !visited.insert(canon) {
                    continue;
                }
            } else if !visited.insert((u, u)) {
                continue;
            }
            let su = sub_to_new[&refined.node_subcommunity[u]];
            let sv = sub_to_new[&refined.node_subcommunity[v]];
            let key = if su <= sv { (su, sv) } else { (sv, su) };
            *edge_acc.entry(key).or_insert(0.0) += w;
            if su == sv {
                new_node_degree[su] += 2.0 * w;
            } else {
                new_node_degree[su] += w;
                new_node_degree[sv] += w;
            }
            total_weight += w;
        }
    }

    let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n_new];
    for ((u, v), w) in edge_acc {
        if u == v {
            adj[u].push((v, w));
        } else {
            adj[u].push((v, w));
            adj[v].push((u, w));
        }
    }

    // Initial node→community assignment is a singleton for each new
    // node; the caller overwrites with phase-1 community ids.
    let node_community: Vec<u64> = (0..n_new).map(|i| i as u64).collect();
    let mut community_degree: FxHashMap<u64, f64> = FxHashMap::default();
    let mut community_members: FxHashMap<u64, FxHashSet<usize>> = FxHashMap::default();
    for i in 0..n_new {
        community_degree.insert(node_community[i], new_node_degree[i]);
        community_members
            .entry(node_community[i])
            .or_default()
            .insert(i);
    }

    let new_state = SnapshotState {
        n: n_new,
        adj,
        node_degree: new_node_degree,
        total_weight,
        node_community,
        community_degree,
        community_members,
    };

    (new_state, member_map, new_to_phase1)
}

// ===========================================================================
// Helpers
// ===========================================================================

fn sum_weights(state: &SnapshotState, v: usize, set: &[usize]) -> f64 {
    let set_lookup: FxHashSet<usize> = set.iter().copied().collect();
    let mut total = 0.0;
    for &(nbr, w) in &state.adj[v] {
        if nbr == v {
            continue;
        }
        if set_lookup.contains(&nbr) {
            total += w;
        }
    }
    total
}

fn sum_weights_to_set(state: &SnapshotState, v: usize, set: &FxHashSet<usize>) -> f64 {
    let mut total = 0.0;
    for &(nbr, w) in &state.adj[v] {
        if nbr == v {
            continue;
        }
        if set.contains(&nbr) {
            total += w;
        }
    }
    total
}

fn members_weight_to_set_excluding(
    state: &SnapshotState,
    members: &FxHashSet<usize>,
    s_members: &[usize],
) -> f64 {
    let s_lookup: FxHashSet<usize> = s_members.iter().copied().collect();
    let mut total = 0.0;
    for &v in members {
        for &(nbr, w) in &state.adj[v] {
            if members.contains(&nbr) {
                continue;
            }
            if s_lookup.contains(&nbr) {
                total += w;
            }
        }
    }
    total
}

fn fisher_yates_shuffle<T>(slice: &mut [T], rng: &mut ChaCha8Rng) {
    for i in (1..slice.len()).rev() {
        let j = rng.gen_range(0..=i);
        slice.swap(i, j);
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use graphreact_core::Attrs;
    use serde_json::json;

    fn weighted_edge(g: &mut CommunicationGraph, u: &str, v: &str, w: f64) {
        let mut attrs = Attrs::new();
        attrs.insert("weight".into(), json!(w));
        g.add_edge(u, v, None, attrs);
    }

    fn unweighted_edge(g: &mut CommunicationGraph, u: &str, v: &str) {
        weighted_edge(g, u, v, 1.0);
    }

    /// Two cliques of size 4, joined by a single bridge edge. Leiden
    /// should put each clique in its own community (Rand Index = 1
    /// against the natural partition).
    fn two_cliques_with_bridge() -> CommunicationGraph {
        let mut g = CommunicationGraph::new();
        for n in ["a1", "a2", "a3", "a4", "b1", "b2", "b3", "b4"] {
            g.add_node(n, Attrs::new());
        }
        // Clique A
        for &(u, v) in &[
            ("a1", "a2"),
            ("a1", "a3"),
            ("a1", "a4"),
            ("a2", "a3"),
            ("a2", "a4"),
            ("a3", "a4"),
        ] {
            unweighted_edge(&mut g, u, v);
        }
        // Clique B
        for &(u, v) in &[
            ("b1", "b2"),
            ("b1", "b3"),
            ("b1", "b4"),
            ("b2", "b3"),
            ("b2", "b4"),
            ("b3", "b4"),
        ] {
            unweighted_edge(&mut g, u, v);
        }
        // Bridge
        unweighted_edge(&mut g, "a1", "b1");
        g
    }

    #[test]
    fn empty_graph_returns_empty_partition() {
        let g = CommunicationGraph::new();
        let p = leiden(&g, &LeidenOptions::default()).unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn single_node_returns_singleton() {
        let mut g = CommunicationGraph::new();
        g.add_node("solo", Attrs::new());
        let p = leiden(&g, &LeidenOptions::default()).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p["solo"], 0);
    }

    #[test]
    fn two_cliques_partition_into_two_communities() {
        let g = two_cliques_with_bridge();
        let opts = LeidenOptions {
            seed: Some(42),
            ..Default::default()
        };
        let p = leiden(&g, &opts).unwrap();
        // All a* should share one community; all b* another.
        let comm_a1 = p["a1"];
        let comm_b1 = p["b1"];
        for n in ["a2", "a3", "a4"] {
            assert_eq!(p[n], comm_a1, "expected {n} to share clique-a community");
        }
        for n in ["b2", "b3", "b4"] {
            assert_eq!(p[n], comm_b1, "expected {n} to share clique-b community");
        }
        assert_ne!(comm_a1, comm_b1, "clique-a and clique-b must differ");
    }

    #[test]
    fn modularity_improves_over_singleton() {
        use super::super::modularity::modularity;
        let g = two_cliques_with_bridge();
        let opts = LeidenOptions {
            seed: Some(7),
            ..Default::default()
        };
        let p = leiden(&g, &opts).unwrap();

        let singleton: PartitionMap = g
            .nodes()
            .enumerate()
            .map(|(i, id)| (id.to_string(), i as u64))
            .collect();

        let q_leiden = modularity(&g, &p, "weight", 1.0).unwrap();
        let q_singleton = modularity(&g, &singleton, "weight", 1.0).unwrap();
        assert!(
            q_leiden > q_singleton + 0.1,
            "Leiden ({q_leiden}) should beat singleton ({q_singleton}) by a clear margin"
        );
    }

    #[test]
    fn deterministic_with_same_seed() {
        let g = two_cliques_with_bridge();
        let opts = LeidenOptions {
            seed: Some(99),
            ..Default::default()
        };
        let p1 = leiden(&g, &opts).unwrap();
        let p2 = leiden(&g, &opts).unwrap();
        assert_eq!(p1, p2);
    }

    #[test]
    fn invalid_resolution_errors() {
        let g = two_cliques_with_bridge();
        let opts = LeidenOptions {
            resolution: -1.0,
            ..Default::default()
        };
        assert!(leiden(&g, &opts).is_err());
    }

    #[test]
    fn invalid_iterations_errors() {
        let g = two_cliques_with_bridge();
        let opts = LeidenOptions {
            max_iterations: 0,
            ..Default::default()
        };
        assert!(leiden(&g, &opts).is_err());
    }

    /// Three cliques connected linearly (clique1 - clique2 - clique3
    /// via bridges). Leiden should find three communities.
    #[test]
    fn three_clique_chain() {
        let mut g = CommunicationGraph::new();
        for prefix in ["a", "b", "c"] {
            for i in 1..=4 {
                g.add_node(format!("{prefix}{i}"), Attrs::new());
            }
            for i in 1..=4 {
                for j in (i + 1)..=4 {
                    unweighted_edge(&mut g, &format!("{prefix}{i}"), &format!("{prefix}{j}"));
                }
            }
        }
        unweighted_edge(&mut g, "a1", "b1");
        unweighted_edge(&mut g, "b2", "c1");
        let opts = LeidenOptions {
            seed: Some(5),
            ..Default::default()
        };
        let p = leiden(&g, &opts).unwrap();
        let unique: FxHashSet<u64> = p.values().copied().collect();
        assert_eq!(
            unique.len(),
            3,
            "expected 3 communities for 3-clique chain, got {} (partition={p:?})",
            unique.len()
        );
        // Each prefix lands in one community.
        for prefix in ["a", "b", "c"] {
            let comm = p[&format!("{prefix}1")];
            for i in 2..=4 {
                assert_eq!(
                    p[&format!("{prefix}{i}")],
                    comm,
                    "{prefix}{i} should match {prefix}1",
                );
            }
        }
    }

    #[test]
    fn weighted_bridge_can_merge_cliques() {
        // Two cliques bridged by a single VERY HEAVY edge — at high
        // enough weight, the bridge dominates the modularity and the
        // two cliques should merge.
        let mut g = CommunicationGraph::new();
        for n in ["a1", "a2", "a3", "b1", "b2", "b3"] {
            g.add_node(n, Attrs::new());
        }
        for &(u, v) in &[("a1", "a2"), ("a1", "a3"), ("a2", "a3")] {
            weighted_edge(&mut g, u, v, 1.0);
        }
        for &(u, v) in &[("b1", "b2"), ("b1", "b3"), ("b2", "b3")] {
            weighted_edge(&mut g, u, v, 1.0);
        }
        weighted_edge(&mut g, "a1", "b1", 100.0);
        let opts = LeidenOptions {
            seed: Some(3),
            resolution: 1.0,
            ..Default::default()
        };
        let p = leiden(&g, &opts).unwrap();
        // The heavy bridge ensures the two bridge endpoints live in
        // the same community. The rest of the partition shape is
        // algorithm-dependent (clique members may stay together or
        // be split off depending on refinement) — we don't pin it.
        assert_eq!(
            p["a1"], p["b1"],
            "bridge endpoints must share community under heavy weight"
        );
    }
}
