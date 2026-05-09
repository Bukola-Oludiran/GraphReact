//! Bridges between Rust types and the JS values NAPI hands us.
//!
//! `Attrs` (the graph's attribute bag) is `BTreeMap<String, serde_json::Value>`
//! on the Rust side; on the JS side it's a plain object. The `napi`
//! `serde-json` feature lets us hand `serde_json::Value` directly across
//! the boundary, so we just need a couple of helpers to assemble/disassemble
//! `Attrs` from JS objects.

use graphreact_core::Attrs;
use napi::Result;
use serde_json::{Map, Value};

/// Convert an `Attrs` (alphabetically-ordered string→Value map) to a flat
/// JSON object that NAPI will deliver to JS as a plain `{}`.
pub fn attrs_to_json(attrs: &Attrs) -> Value {
    let mut map = Map::with_capacity(attrs.len());
    for (k, v) in attrs {
        map.insert(k.clone(), v.clone());
    }
    Value::Object(map)
}

/// Convert an arbitrary JS value (already deserialized to `Value`) into
/// an `Attrs`. Non-object values produce an empty `Attrs` — callers that
/// require an object should validate first.
pub fn json_to_attrs(value: Value) -> Attrs {
    let mut attrs = Attrs::new();
    if let Value::Object(map) = value {
        for (k, v) in map {
            attrs.insert(k, v);
        }
    }
    attrs
}

/// Standard error wrapper — converts any `Display` error into a NAPI error
/// preserving the message. Callers use this with `?` to surface Rust errors
/// as JS `Error` objects.
pub fn err_to_napi<E: std::fmt::Display>(e: E) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Helper: NAPI `Result` shorthand.
pub type NResult<T> = Result<T>;
