//! Typed attribute accessors for common GraphReact node and edge fields.
//!
//! Mirrors TypeScript's `GraphReactNodeAttributes` and
//! `GraphReactEdgeAttributes` interfaces. These helpers exist so callers
//! don't have to spell out `attrs["weight"].as_f64().unwrap_or(...)`
//! everywhere — they encapsulate the JSON-shaped attribute storage and
//! return strongly-typed values.

use serde_json::Value;

use super::types::Attrs;

/// Common edge field names. Keep this in sync with TypeScript's
/// [`graphreact/src/core/graph/attributes.ts`].
pub mod edge_fields {
    /// Named channel for multi-edge disambiguation.
    pub const CHANNEL: &str = "channel";
    /// Message-type whitelist on this edge.
    pub const FILTER: &str = "filter";
    /// Per-edge TTL override.
    pub const TTL: &str = "ttl";
    /// Numeric weight (used by transforms like `multiply-weight`).
    pub const WEIGHT: &str = "weight";
    /// Delivery priority — higher is delivered first.
    pub const PRIORITY: &str = "priority";
    /// Max messages per second on this edge.
    pub const RATE_LIMIT: &str = "rateLimit";
    /// Named transform applied in transit.
    pub const TRANSFORM: &str = "transform";
}

/// Common node field names.
pub mod node_fields {
    /// The component type name this node was created from.
    pub const COMPONENT: &str = "component";
}

/// Read the `weight` attribute as `f64`, if present.
pub fn weight(attrs: &Attrs) -> Option<f64> {
    attrs.get(edge_fields::WEIGHT).and_then(Value::as_f64)
}

/// Read the `channel` attribute as a string slice, if present.
pub fn channel(attrs: &Attrs) -> Option<&str> {
    attrs.get(edge_fields::CHANNEL).and_then(Value::as_str)
}

/// Read the `priority` attribute as `i64`, if present.
pub fn priority(attrs: &Attrs) -> Option<i64> {
    attrs.get(edge_fields::PRIORITY).and_then(Value::as_i64)
}

/// Read the `ttl` attribute as `u64`, if present.
pub fn ttl(attrs: &Attrs) -> Option<u64> {
    attrs.get(edge_fields::TTL).and_then(Value::as_u64)
}

/// Read the `transform` attribute as a string slice, if present.
pub fn transform(attrs: &Attrs) -> Option<&str> {
    attrs.get(edge_fields::TRANSFORM).and_then(Value::as_str)
}

/// Read the `filter` attribute as a list of strings, if present.
pub fn filter(attrs: &Attrs) -> Option<Vec<String>> {
    attrs.get(edge_fields::FILTER).and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
    })
}

/// Read the `component` attribute as a string slice, if present.
pub fn component(attrs: &Attrs) -> Option<&str> {
    attrs.get(node_fields::COMPONENT).and_then(Value::as_str)
}
