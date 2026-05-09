//! Channel policy resolution and permission checks.
//!
//! Resolution order (matches TypeScript implementation):
//! 1. `manifest.channels[sender_id]` — exact neighbor match
//! 2. `manifest.channels["*"]` — wildcard default
//! 3. `manifest.accepts` / `manifest.emits` — flat fallback
//! 4. Edge-attribute overlay (`filter`, `rate_limit`, `priority`, `transform`)

use std::collections::HashSet;

use crate::graph::{
    attributes::edge_fields, attributes::filter as edge_filter, CommunicationGraph,
};
use crate::runtime::node_instance::NodeInstance;
use crate::runtime::transforms::get_transform;
use crate::runtime::types::{ChannelPolicy, IncomingMessage, TransformFn};

/// Fully-resolved policy for a single sender→target pair.
#[derive(Default, Clone)]
pub struct ResolvedPolicy {
    pub accepts: Vec<String>,
    pub emits: Vec<String>,
    pub transform: Option<TransformFn>,
    pub rate_limit: Option<f64>,
    pub priority: i64,
}

impl std::fmt::Debug for ResolvedPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedPolicy")
            .field("accepts", &self.accepts)
            .field("emits", &self.emits)
            .field("transform", &self.transform.as_ref().map(|_| "<fn>"))
            .field("rate_limit", &self.rate_limit)
            .field("priority", &self.priority)
            .finish()
    }
}

/// Resolve the full channel policy for `sender` → `target_node`.
pub fn resolve_channel_policy(
    target_node: &NodeInstance,
    sender_id: &str,
    graph: &CommunicationGraph,
    edge_key: Option<&str>,
) -> ResolvedPolicy {
    let Some(manifest) = target_node.manifest.as_ref() else {
        return ResolvedPolicy::default();
    };

    // Step 1-3: node-level policy. Exact match wins; fall back to wildcard;
    // then to flat manifest fields.
    let channel_policy: Option<&ChannelPolicy> = manifest
        .channels
        .get(sender_id)
        .or_else(|| manifest.channels.get("*"));

    let mut accepts = channel_policy
        .and_then(|c| c.accepts.clone())
        .unwrap_or_else(|| manifest.accepts.clone());
    let emits = channel_policy
        .and_then(|c| c.emits.clone())
        .unwrap_or_else(|| manifest.emits.clone());
    let mut transform = channel_policy.and_then(|c| c.transform.clone());
    let mut rate_limit = channel_policy.and_then(|c| c.rate_limit);
    let mut priority = channel_policy.and_then(|c| c.priority).unwrap_or(0);

    // Step 4: edge-attribute overlay.
    if !graph.has_edge(sender_id, &target_node.id, None) {
        return ResolvedPolicy {
            accepts,
            emits,
            transform,
            rate_limit,
            priority,
        };
    }

    if let Some(key) = edge_key {
        if let Ok(edge_data) = graph.get_edge_data(sender_id, &target_node.id, Some(key)) {
            if let Some(filter_list) = edge_filter(edge_data) {
                let allowed: HashSet<String> = filter_list.into_iter().collect();
                accepts.retain(|t| allowed.contains(t));
            }
            if transform.is_none() {
                if let Some(name) = edge_data
                    .get(edge_fields::TRANSFORM)
                    .and_then(serde_json::Value::as_str)
                {
                    if let Some(named) = get_transform(name) {
                        transform = Some(wrap_named_transform(&named, edge_data));
                    }
                }
            }
            if let Some(rl) = edge_data
                .get(edge_fields::RATE_LIMIT)
                .and_then(serde_json::Value::as_f64)
            {
                rate_limit = Some(rl);
            }
            if let Some(p) = edge_data
                .get(edge_fields::PRIORITY)
                .and_then(serde_json::Value::as_i64)
            {
                priority = p;
            }
        }
        return ResolvedPolicy {
            accepts,
            emits,
            transform,
            rate_limit,
            priority,
        };
    }

    // No specific edge key — union filters across all edges between this pair.
    let keys = graph.edge_keys(sender_id, &target_node.id);
    if keys.is_empty() {
        return ResolvedPolicy {
            accepts,
            emits,
            transform,
            rate_limit,
            priority,
        };
    }

    let mut has_any_filter = false;
    let mut allowed_types: HashSet<String> = HashSet::new();
    let mut best_transform = transform;
    let mut best_rate_limit = rate_limit;
    let mut best_priority = priority;

    for key in &keys {
        let Ok(edge_data) = graph.get_edge_data(sender_id, &target_node.id, Some(key)) else {
            continue;
        };
        if let Some(filter_list) = edge_filter(edge_data) {
            has_any_filter = true;
            for t in filter_list {
                allowed_types.insert(t);
            }
        }
        if let Some(rl) = edge_data
            .get(edge_fields::RATE_LIMIT)
            .and_then(serde_json::Value::as_f64)
        {
            best_rate_limit = Some(rl);
        }
        if let Some(p) = edge_data
            .get(edge_fields::PRIORITY)
            .and_then(serde_json::Value::as_i64)
        {
            best_priority = best_priority.max(p);
        }
        if best_transform.is_none() {
            if let Some(name) = edge_data
                .get(edge_fields::TRANSFORM)
                .and_then(serde_json::Value::as_str)
            {
                if let Some(named) = get_transform(name) {
                    best_transform = Some(wrap_named_transform(&named, edge_data));
                }
            }
        }
    }

    if has_any_filter {
        accepts.retain(|t| allowed_types.contains(t));
    }

    ResolvedPolicy {
        accepts,
        emits,
        transform: best_transform,
        rate_limit: best_rate_limit,
        priority: best_priority,
    }
}

/// Wrap a named transform with the edge's attributes pre-bound. We snapshot
/// the attributes by clone so the transform stays valid even if the edge is
/// later mutated.
fn wrap_named_transform(named: &TransformFn, edge_attrs: &crate::graph::Attrs) -> TransformFn {
    let attrs_snapshot = edge_attrs.clone();
    let named_clone = named.clone();
    std::sync::Arc::new(move |msg: &IncomingMessage, _attrs| named_clone(msg, &attrs_snapshot))
}

/// Whether `node`'s manifest accepts the given message type (flat check).
#[must_use]
pub fn node_accepts(node: &NodeInstance, message_type: &str) -> bool {
    node.manifest
        .as_ref()
        .is_some_and(|m| m.accepts.iter().any(|t| t == message_type))
}

/// Whether `node`'s manifest emits the given message type (flat check).
#[must_use]
pub fn node_can_emit(node: &NodeInstance, message_type: &str) -> bool {
    node.manifest
        .as_ref()
        .is_some_and(|m| m.emits.iter().any(|t| t == message_type))
}
