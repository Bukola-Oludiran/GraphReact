//! Cross-engine value representation.
//!
//! `HostValue` is the marshalled-value type passed between Rust and
//! the host language. Each engine converts to/from its native value
//! type at the FFI boundary.
//!
//! For JSC: `HostValue::Number` ↔ `JSValueMakeNumber`,
//! `HostValue::String` ↔ `JSValueMakeString`, etc. V8 mappings (M17)
//! follow the same shape with `v8::Number` / `v8::String`.
//!
//! Aligned with `serde_json::Value` shape so JSON-serialisable
//! payloads cross the boundary verbatim. This is the same trade-off
//! Bun makes in its `JSValue` / `JSONValue` bridge — JSON is the
//! universal interchange.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Object payload as ordered `(key, value)` pairs. Mirrors JS object
/// literal insertion-order semantics without depending on
/// `serde_json::Map`'s `Value`-only key type.
pub type HostObject = Vec<(String, HostValue)>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HostValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<HostValue>),
    Object(HostObject),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HostValueKind {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

impl HostValue {
    pub const fn kind(&self) -> HostValueKind {
        match self {
            Self::Null => HostValueKind::Null,
            Self::Bool(_) => HostValueKind::Bool,
            Self::Number(_) => HostValueKind::Number,
            Self::String(_) => HostValueKind::String,
            Self::Array(_) => HostValueKind::Array,
            Self::Object(_) => HostValueKind::Object,
        }
    }

    pub fn from_json(value: JsonValue) -> Self {
        match value {
            JsonValue::Null => Self::Null,
            JsonValue::Bool(b) => Self::Bool(b),
            JsonValue::Number(n) => Self::Number(n.as_f64().unwrap_or(0.0)),
            JsonValue::String(s) => Self::String(s),
            JsonValue::Array(a) => Self::Array(a.into_iter().map(Self::from_json).collect()),
            JsonValue::Object(o) => {
                let mut pairs: HostObject = Vec::with_capacity(o.len());
                for (k, v) in o {
                    pairs.push((k, Self::from_json(v)));
                }
                Self::Object(pairs)
            }
        }
    }

    pub fn into_json(self) -> JsonValue {
        match self {
            Self::Null => JsonValue::Null,
            Self::Bool(b) => JsonValue::Bool(b),
            Self::Number(n) => serde_json::Number::from_f64(n)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null),
            Self::String(s) => JsonValue::String(s),
            Self::Array(a) => JsonValue::Array(a.into_iter().map(Self::into_json).collect()),
            Self::Object(pairs) => {
                let mut m = serde_json::Map::new();
                for (k, v) in pairs {
                    m.insert(k, v.into_json());
                }
                JsonValue::Object(m)
            }
        }
    }
}

impl From<JsonValue> for HostValue {
    fn from(v: JsonValue) -> Self {
        Self::from_json(v)
    }
}

impl From<HostValue> for JsonValue {
    fn from(v: HostValue) -> Self {
        v.into_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_value_round_trips_through_json() {
        let original = HostValue::Object(vec![
            ("count".to_string(), HostValue::Number(42.0)),
            ("name".to_string(), HostValue::String("test".to_string())),
            (
                "tags".to_string(),
                HostValue::Array(vec![
                    HostValue::String("a".to_string()),
                    HostValue::String("b".to_string()),
                ]),
            ),
        ]);
        let json = original.clone().into_json();
        let back = HostValue::from_json(json);
        assert_eq!(original, back);
    }

    #[test]
    fn host_value_kind_matches_variant() {
        assert_eq!(HostValue::Null.kind(), HostValueKind::Null);
        assert_eq!(HostValue::Bool(true).kind(), HostValueKind::Bool);
        assert_eq!(HostValue::Number(1.0).kind(), HostValueKind::Number);
        assert_eq!(
            HostValue::String("x".to_string()).kind(),
            HostValueKind::String
        );
    }
}
