//! Error types for the core runtime.

use thiserror::Error;

/// Errors raised by [`crate::graph::CommunicationGraph`] operations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// A node with the given ID was expected but not found.
    #[error("Node '{id}' not in graph")]
    NodeMissing { id: String },

    /// An edge between `u` and `v` was expected but not found.
    #[error("Edge ({u}, {v}) not in graph")]
    EdgeMissing { u: String, v: String },

    /// An edge with the specific key was expected but not found.
    #[error("Edge ({u}, {v}, {key}) not in graph")]
    EdgeKeyMissing { u: String, v: String, key: String },
}

impl GraphError {
    pub(crate) fn node_missing(id: &str) -> Self {
        Self::NodeMissing { id: id.to_string() }
    }

    pub(crate) fn edge_missing(u: &str, v: &str) -> Self {
        Self::EdgeMissing {
            u: u.to_string(),
            v: v.to_string(),
        }
    }
}

/// Errors raised by the reactive runtime — scheduler, hooks, lifecycle.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// A hook was called outside a node component evaluation.
    #[error("hook called outside a node component evaluation")]
    HookOutsideComponent,

    /// Scheduler exceeded `MAX_PROCESS_DEPTH` iterations.
    #[error("scheduler exceeded max process depth ({max}) — likely a runaway message loop")]
    MaxDepthExceeded { max: usize },

    /// Permission manifest rejected a message.
    #[error("node '{node}' does not accept message type '{message_type}' from '{from}'")]
    PermissionDenied {
        node: String,
        from: String,
        message_type: String,
    },

    /// A scheduler operation referenced a node that has been unmounted.
    #[error("node '{id}' has been unmounted")]
    NodeUnmounted { id: String },

    /// A scheduler operation referenced a node that doesn't exist.
    #[error("node '{id}' not registered with scheduler")]
    NodeNotRegistered { id: String },

    /// Wrapper around a graph-layer error.
    #[error(transparent)]
    Graph(#[from] GraphError),
}
