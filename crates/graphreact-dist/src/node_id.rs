//! Cluster-side identity primitives.
//!
//! - [`NodeId`] is the wire-level + user-facing node identifier
//!   (`"name@host"` form). Validated at construction so downstream
//!   parsing can rely on the `@` invariant.
//! - [`Creation`] is OTP's boot epoch — a u32 counter that
//!   increments on every cluster-rejoin so monitors and links from
//!   pre-restart sessions don't accidentally resurrect against the
//!   new instance.
//! - [`RemoteNodeRef`] pairs a `NodeId` with a `Creation` for cases
//!   where a stale reference must be detected (post-restart auto-
//!   reattach is rejected).
//!
//! `NodeId` is **distinct** from `graphreact_core::NodeKey` (the
//! single-node `slotmap::Key`). The single-node runtime stays
//! string-keyed at its public boundary; the dist layer maintains a
//! translation table from `(NodeId, local_id_string)` to local
//! `NodeKey` (lands in M14.5's `MonitorTranslationTable`).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Cluster-wide node identifier. Canonical wire form: `"name@host"`.
///
/// Validated at construction — once you hold a `NodeId`, the `@`
/// separator is guaranteed to be present and the `name` / `host`
/// parts are both non-empty.
///
/// # Examples
///
/// ```
/// use graphreact_dist::NodeId;
/// let id: NodeId = "alice@127.0.0.1".parse().unwrap();
/// assert_eq!(id.name(), "alice");
/// assert_eq!(id.host(), "127.0.0.1");
/// assert!("@invalid".parse::<NodeId>().is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId {
    full: String,
    /// Byte offset of the `@` separator within `full`. Cached so
    /// `name()` / `host()` are O(1) and don't re-scan.
    at_offset: usize,
}

impl NodeId {
    pub fn name(&self) -> &str {
        &self.full[..self.at_offset]
    }

    pub fn host(&self) -> &str {
        &self.full[self.at_offset + 1..]
    }

    pub fn as_str(&self) -> &str {
        &self.full
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NodeIdParseError {
    #[error("node id must contain exactly one '@' separator: got `{0}`")]
    BadSeparator(String),
    #[error("node id name part must not be empty: got `{0}`")]
    EmptyName(String),
    #[error("node id host part must not be empty: got `{0}`")]
    EmptyHost(String),
}

impl std::str::FromStr for NodeId {
    type Err = NodeIdParseError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        // Exactly one '@' — reject "alice@b@c" (ambiguous host).
        let mut iter = s.match_indices('@');
        let at = iter.next();
        let extra = iter.next();
        match (at, extra) {
            (Some((idx, _)), None) => {
                if idx == 0 {
                    return Err(NodeIdParseError::EmptyName(s.to_string()));
                }
                if idx == s.len() - 1 {
                    return Err(NodeIdParseError::EmptyHost(s.to_string()));
                }
                Ok(Self {
                    full: s.to_string(),
                    at_offset: idx,
                })
            }
            _ => Err(NodeIdParseError::BadSeparator(s.to_string())),
        }
    }
}

impl TryFrom<String> for NodeId {
    type Error = NodeIdParseError;
    fn try_from(s: String) -> std::result::Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<NodeId> for String {
    fn from(id: NodeId) -> Self {
        id.full
    }
}

/// OTP-style boot epoch — incremented on every (re-)start. Carried in
/// every handshake packet + every monitor reference so post-restart
/// sessions can't have monitors silently auto-reattach (which would
/// be a correctness violation: the user's intent expired with the
/// previous boot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Creation(pub u32);

/// `NodeId` + `Creation` paired for stale-reference detection. Used
/// by M14.5 (`MonitorTranslationTable`) to refuse auto-reattach
/// across reconnects.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoteNodeRef {
    pub node: NodeId,
    pub creation: Creation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip() {
        let id: NodeId = "alice@127.0.0.1".parse().unwrap();
        assert_eq!(id.name(), "alice");
        assert_eq!(id.host(), "127.0.0.1");
        assert_eq!(id.as_str(), "alice@127.0.0.1");
        assert_eq!(id.to_string(), "alice@127.0.0.1");
    }

    #[test]
    fn reject_no_separator() {
        let err = "alice".parse::<NodeId>().unwrap_err();
        assert!(matches!(err, NodeIdParseError::BadSeparator(_)));
    }

    #[test]
    fn reject_two_separators() {
        let err = "alice@b@c".parse::<NodeId>().unwrap_err();
        assert!(matches!(err, NodeIdParseError::BadSeparator(_)));
    }

    #[test]
    fn reject_empty_name() {
        let err = "@host".parse::<NodeId>().unwrap_err();
        assert!(matches!(err, NodeIdParseError::EmptyName(_)));
    }

    #[test]
    fn reject_empty_host() {
        let err = "alice@".parse::<NodeId>().unwrap_err();
        assert!(matches!(err, NodeIdParseError::EmptyHost(_)));
    }

    #[test]
    fn serde_roundtrip_uses_string_form() {
        let id: NodeId = "alice@127.0.0.1".parse().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"alice@127.0.0.1\"");
        let back: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }
}
