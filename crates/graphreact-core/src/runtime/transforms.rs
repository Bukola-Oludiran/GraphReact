//! Edge transform registry — built-in transforms plus a registration API
//! for user-defined ones.
//!
//! Transforms run during message delivery on edges that declare a
//! `transform` attribute (Phase 10). They receive the message and edge
//! attributes, and return either a (possibly modified) message or `None`
//! to drop it.

use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use serde_json::Value;
use std::sync::{Arc, OnceLock};

use crate::graph::Attrs;
use crate::runtime::types::{IncomingMessage, TransformFn};

fn registry() -> &'static RwLock<FxHashMap<String, TransformFn>> {
    static REGISTRY: OnceLock<RwLock<FxHashMap<String, TransformFn>>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut map: FxHashMap<String, TransformFn> = FxHashMap::default();
        map.insert("multiply-weight".into(), multiply_weight());
        map.insert("attenuate".into(), attenuate());
        map.insert("amplify".into(), amplify());
        map.insert("add-weight".into(), add_weight());
        map.insert("threshold-gate".into(), threshold_gate());
        RwLock::new(map)
    })
}

/// Lookup a transform by name. Returns `None` if no such transform is
/// registered.
#[must_use]
pub fn get_transform(name: &str) -> Option<TransformFn> {
    registry().read().get(name).cloned()
}

/// Register a custom transform. Replaces any existing transform with the
/// same name (including built-ins).
pub fn register_transform(name: impl Into<String>, fn_: TransformFn) {
    registry().write().insert(name.into(), fn_);
}

/// All registered transform names.
#[must_use]
pub fn list_transforms() -> Vec<String> {
    registry().read().keys().cloned().collect()
}

// ---- Built-in transforms ----

fn multiply_weight() -> TransformFn {
    Arc::new(|msg, attrs| {
        let weight = attrs.get("weight").and_then(Value::as_f64).unwrap_or(1.0);
        Some(scale_payload_value(msg, weight))
    })
}

fn attenuate() -> TransformFn {
    Arc::new(|msg, attrs| {
        let weight = attrs.get("weight").and_then(Value::as_f64).unwrap_or(0.5);
        Some(scale_payload_value(msg, weight))
    })
}

fn amplify() -> TransformFn {
    Arc::new(|msg, attrs| {
        let weight = attrs.get("weight").and_then(Value::as_f64).unwrap_or(0.5);
        if weight == 0.0 {
            return Some(msg.clone());
        }
        Some(scale_payload_value(msg, 1.0 / weight))
    })
}

fn add_weight() -> TransformFn {
    Arc::new(|msg, attrs| {
        let weight = attrs.get("weight").and_then(Value::as_f64).unwrap_or(0.0);
        Some(map_payload_value(msg, |v| v + weight))
    })
}

fn threshold_gate() -> TransformFn {
    Arc::new(|msg, attrs| {
        let threshold = attrs.get("weight").and_then(Value::as_f64).unwrap_or(0.0);
        let value = msg
            .payload
            .get("value")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if value < threshold {
            None
        } else {
            Some(msg.clone())
        }
    })
}

fn scale_payload_value(msg: &IncomingMessage, factor: f64) -> IncomingMessage {
    map_payload_value(msg, |v| v * factor)
}

fn map_payload_value(msg: &IncomingMessage, f: impl FnOnce(f64) -> f64) -> IncomingMessage {
    let value = msg
        .payload
        .get("value")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let new_value = f(value);
    let mut payload = msg.payload.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("value".into(), serde_json::json!(new_value));
    }
    IncomingMessage {
        from: msg.from.clone(),
        r#type: msg.r#type.clone(),
        payload,
        ttl: msg.ttl,
        run_id: msg.run_id.clone(),
        attempt_id: msg.attempt_id.clone(),
    }
}

#[allow(dead_code)]
fn _attrs_marker(_a: &Attrs) {}
