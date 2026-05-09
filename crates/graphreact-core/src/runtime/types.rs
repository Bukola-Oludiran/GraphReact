//! Public runtime types — messages, manifests, contexts.
//!
//! `Payload` is `serde_json::Value` (parity with the rest of the system —
//! attributes, JSON serialization, NAPI boundary all use the same shape).
//! Strongly-typed payload variants can be added later via `From<T>`.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::graph::Attrs;

/// Wire-level message — what flows through the scheduler's queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Monotonically-increasing identifier.
    pub id: String,
    /// Type tag — used for dispatch via [`use_message`].
    pub r#type: String,
    /// Sender node ID (or `__external__` for runtime-injected messages).
    pub from: String,
    /// Target node ID.
    pub to: String,
    /// Arbitrary payload.
    pub payload: Value,
    /// Hops remaining before this message is dropped.
    pub ttl: u32,
    /// Wall-clock millisecond timestamp at creation.
    pub timestamp: u64,
    /// `true` if emitted with `EmitOptions::direct` — bypasses channel-policy
    /// resolution at delivery; only flat `manifest.accepts` is checked.
    /// Mirrors the TS scheduler's `_direct` flag.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub direct: bool,
    /// **M16-Backend P10.** Remote OTel span context propagated from
    /// the wire packet for cross-node distributed traces.
    /// `Option<[u8; 24]>` = `TraceID(16) || SpanID(8)` (W3C SpanContext
    /// minimum for span linkage). The dist crate's `dispatch_inbound`
    /// populates this from `graphreact_dist::wire::Message::trace_context`
    /// when an inbound remote message arrives; the scheduler's `deliver`
    /// path forwards it into the `MessageDelivered` event for the
    /// `graphreact_observer::OtelSink` consumer to attach as a `Link`
    /// attribute on the receiver-side `message.deliver` span. `None` for
    /// local-origin messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<[u8; 24]>,
}

/// Trimmed view of a [`Message`] handed to user `useMessage` handlers.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub from: String,
    pub r#type: String,
    pub payload: Value,
    pub ttl: u32,
}

impl IncomingMessage {
    #[must_use]
    pub fn from_message(msg: &Message) -> Self {
        Self {
            from: msg.from.clone(),
            r#type: msg.r#type.clone(),
            payload: msg.payload.clone(),
            ttl: msg.ttl,
        }
    }
}

/// Per-neighbor channel policy — overrides the flat `accepts`/`emits` for a
/// specific connection. See `permissions::resolve_channel_policy`.
#[derive(Default, Clone)]
pub struct ChannelPolicy {
    pub accepts: Option<Vec<String>>,
    pub emits: Option<Vec<String>>,
    /// Optional in-transit transform — return `Some(modified)` to forward
    /// or `None` to drop the message.
    pub transform: Option<TransformFn>,
    pub rate_limit: Option<f64>,
    pub priority: Option<i64>,
}

impl std::fmt::Debug for ChannelPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelPolicy")
            .field("accepts", &self.accepts)
            .field("emits", &self.emits)
            .field("transform", &self.transform.as_ref().map(|_| "<fn>"))
            .field("rate_limit", &self.rate_limit)
            .field("priority", &self.priority)
            .finish()
    }
}

/// Permission manifest returned from a node's component function.
///
/// Defines what messages the node accepts/emits globally, plus optional
/// per-neighbor overrides keyed by neighbor ID (with `*` as a wildcard
/// default for unlisted neighbors).
#[derive(Default, Clone)]
pub struct PermissionManifest {
    pub accepts: Vec<String>,
    pub emits: Vec<String>,
    /// Per-neighbor channel policies. Keys are neighbor node IDs, with
    /// `*` reserved for the wildcard default.
    pub channels: IndexMap<String, ChannelPolicy>,
}

impl std::fmt::Debug for PermissionManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionManifest")
            .field("accepts", &self.accepts)
            .field("emits", &self.emits)
            .field("channels", &self.channels)
            .finish()
    }
}

impl PermissionManifest {
    /// Convenience: build a manifest accepting and emitting the given types.
    #[must_use]
    pub fn new<A, E, S1, S2>(accepts: A, emits: E) -> Self
    where
        A: IntoIterator<Item = S1>,
        E: IntoIterator<Item = S2>,
        S1: Into<String>,
        S2: Into<String>,
    {
        Self {
            accepts: accepts.into_iter().map(Into::into).collect(),
            emits: emits.into_iter().map(Into::into).collect(),
            channels: IndexMap::new(),
        }
    }
}

/// Optional steering for [`emit`].
#[derive(Default, Clone, Debug)]
pub struct EmitOptions {
    /// Restrict delivery to one or more specific neighbors by ID.
    pub to: Option<Vec<String>>,
    /// Override the default TTL for this single emission.
    pub ttl: Option<u32>,
    /// Target a specific multi-edge key.
    pub edge_key: Option<String>,
    /// Bypass edge checks and deliver directly to a node by ID — Phase 9
    /// direct messaging. Permission manifest is still honored on the target.
    pub direct: Option<String>,
}

/// Reference to a child node in a component's nesting hierarchy.
#[derive(Debug, Clone)]
pub struct ChildRef {
    pub id: String,
    pub component: String,
}

/// Specification for a child node to be spawned via [`ctx.spawnChild`] or
/// declared via [`ctx.compose`].
///
/// Carries the same data as the four-arg `add_node_at_runtime` signature
/// — bundled into a struct so `Compose` can pass a list of them.
pub struct ComposeChildSpec {
    pub id: String,
    pub definition: crate::runtime::node_instance::NodeDefinition,
    pub props: Attrs,
    pub connect_to: Vec<String>,
}

impl std::fmt::Debug for ComposeChildSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComposeChildSpec")
            .field("id", &self.id)
            .field("definition", &self.definition)
            .field("props", &self.props)
            .field("connect_to", &self.connect_to)
            .finish()
    }
}

/// Context handed to a node's component function on every evaluation.
///
/// Hooks are free functions (`use_state`, `use_effect`, …) that read
/// thread-locals — they don't take the context as an argument. The context
/// only carries data: identity, props, structural neighbors, directory
/// access. Mutation surface (compose, accept_edge) is exposed as scheduler
/// callbacks invoked through the context's stored fn pointers.
pub struct NodeContext {
    pub id: String,
    pub props: Attrs,
    pub children: Vec<ChildRef>,
    pub my_depth: u32,
    pub my_path: Vec<String>,
    /// Snapshot of structural neighbors as observed at evaluation time.
    /// Hooks like `use_neighbors` read a live version through the
    /// scheduler; this field is what the component sees inline.
    pub neighbors: Vec<String>,
}

/// Transform function — invoked during message delivery on edges that
/// declare one. Return `Some(modified)` to forward, `None` to drop.
///
/// Bounded `Send + Sync` so transforms can be looked up from registries
/// shared across threads via `Arc`. They are still invoked only on the
/// scheduler thread.
pub type TransformFn =
    Arc<dyn Fn(&IncomingMessage, &Attrs) -> Option<IncomingMessage> + Send + Sync>;

/// Message handler — registered via `use_message`. The M4 baseline
/// scheduler is single-threaded; handlers are `!Send` (`Rc`-backed) to
/// permit capturing per-node `Rc`-shared state without locks.
pub type MessageHandler = std::rc::Rc<dyn Fn(&IncomingMessage)>;

/// Topology event handler — registered via `use_topology`. Same threading
/// rationale as [`MessageHandler`].
pub type TopologyHandler = std::rc::Rc<dyn Fn(&crate::graph::TopologyEvent)>;

/// Filter for `use_topology` — narrow which topology events the handler
/// should receive.
#[derive(Default, Clone, Debug)]
pub struct TopologyFilterOptions {
    /// Only events of these kinds. `None` accepts all kinds.
    pub kinds: Option<Vec<TopologyEventKind>>,
    /// If `true`, only events involving this node's direct neighbors fire.
    pub neighbors_only: bool,
}

/// Discriminator for [`crate::graph::TopologyEvent`] used in filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TopologyEventKind {
    NodeAdded,
    NodeRemoved,
    EdgeAdded,
    EdgeRemoved,
    EdgeUpdated,
}

impl TopologyEventKind {
    #[must_use]
    pub fn of(event: &crate::graph::TopologyEvent) -> Self {
        match event {
            crate::graph::TopologyEvent::NodeAdded { .. } => Self::NodeAdded,
            crate::graph::TopologyEvent::NodeRemoved { .. } => Self::NodeRemoved,
            crate::graph::TopologyEvent::EdgeAdded { .. } => Self::EdgeAdded,
            crate::graph::TopologyEvent::EdgeRemoved { .. } => Self::EdgeRemoved,
            crate::graph::TopologyEvent::EdgeUpdated { .. } => Self::EdgeUpdated,
        }
    }
}
