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

/// Scheduler durability policy. `Off` is the compatibility default and does
/// not alter the existing ephemeral processing path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityMode {
    #[default]
    Off,
    Local,
    Cluster,
}

/// Recovery payload containing only explicitly named durable hook values.
/// Callback state, positional hooks, engine objects, and arbitrary Rust
/// values are intentionally absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DurableStateSnapshot {
    pub nodes:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Default)]
pub struct DurableQueueSnapshot {
    pub inbox: Vec<Message>,
    pub postponed: Vec<Message>,
}

/// One durable message-handler activation presented to the host commit
/// boundary. Core owns evaluation and buffering; the host translates this
/// storage-agnostic value into persistence records and acknowledges only
/// after its backend has made them durable.
#[derive(Debug, Clone)]
pub struct DurableActivation {
    pub input: Message,
    pub state_before: DurableStateSnapshot,
    pub state_after: DurableStateSnapshot,
    pub emitted_messages: Vec<Message>,
    pub effect_intents: Vec<DurableEffectIntent>,
    pub timers_scheduled: Vec<DurableTimerIntent>,
    pub timers_cancelled: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableReplayPolicy {
    Idempotent,
    #[serde(alias = "nonReplayable")]
    NonReplayable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableEffectRequest {
    pub key: String,
    pub adapter: String,
    pub operation: String,
    pub input: serde_json::Value,
    #[serde(alias = "result_type")]
    pub result_type: String,
    #[serde(alias = "error_type")]
    pub error_type: String,
    #[serde(alias = "replay_policy")]
    pub replay_policy: DurableReplayPolicy,
}

#[derive(Debug, Clone)]
pub struct DurableEffectIntent {
    pub id: String,
    pub key: String,
    pub run_id: String,
    pub attempt_id: String,
    pub node_id: String,
    pub adapter: String,
    pub operation: String,
    pub input: serde_json::Value,
    pub result_type: String,
    pub error_type: String,
    pub replay_policy: DurableReplayPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableTimerRequest {
    pub key: String,
    #[serde(alias = "deadline_unix_ms")]
    pub deadline_unix_ms: u64,
    #[serde(alias = "message_type")]
    pub message_type: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum DurableTimerCommand {
    Schedule(DurableTimerRequest),
    Cancel { timer_id: String },
}

#[derive(Debug, Clone)]
pub struct DurableTimerIntent {
    pub id: String,
    pub key: String,
    pub run_id: String,
    pub attempt_id: String,
    pub node_id: String,
    pub deadline_unix_ms: u64,
    pub message_type: String,
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod durable_wire_tests {
    use super::{DurableEffectRequest, DurableReplayPolicy, DurableTimerRequest};
    use serde_json::json;

    #[test]
    fn durable_requests_use_the_language_host_wire_shape() {
        let effect: DurableEffectRequest = serde_json::from_value(json!({
            "key": "lookup",
            "adapter": "test",
            "operation": "record",
            "input": {"value": 7},
            "resultType": "effect.ok",
            "errorType": "effect.error",
            "replayPolicy": "idempotent"
        }))
        .expect("effect request");
        assert_eq!(effect.replay_policy, DurableReplayPolicy::Idempotent);
        assert_eq!(
            serde_json::to_value(&effect).expect("serialize")["resultType"],
            "effect.ok"
        );

        let timer: DurableTimerRequest = serde_json::from_value(json!({
            "key": "wake",
            "deadlineUnixMs": 42,
            "messageType": "timer.fired",
            "payload": null
        }))
        .expect("timer request");
        assert_eq!(timer.deadline_unix_ms, 42);
        assert_eq!(
            serde_json::to_value(&timer).expect("serialize")["messageType"],
            "timer.fired"
        );

        let python_effect: DurableEffectRequest = serde_json::from_value(json!({
            "key": "lookup",
            "adapter": "test",
            "operation": "record",
            "input": null,
            "result_type": "effect.ok",
            "error_type": "effect.error",
            "replay_policy": "non_replayable"
        }))
        .expect("snake-case effect request");
        assert_eq!(
            python_effect.replay_policy,
            DurableReplayPolicy::NonReplayable
        );
    }
}

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
    /// **M-Ingress phase 2.** Opaque correlation identifier carried
    /// end-to-end across the runtime so a synchronous HTTP call
    /// (`POST /v1/nodes/:id/call`) can route the eventual reply
    /// back to the awaiting `tokio::oneshot`. The HTTP call handler
    /// generates a UUIDv4, attaches it to the inbound ingress
    /// message, and registers an entry in
    /// `graphreact_observer::management_api::PendingCalls`. The
    /// scheduler captures the inbound id in the per-handler-invocation
    /// context and **auto-attaches** it to the first outgoing
    /// emit whose `type` matches the call's declared `reply_type`
    /// (per the manifest's `accepts_external_call` table). The
    /// observer's PendingCalls sink, watching `MessageDelivered`
    /// events, looks up the correlation_id and resolves the
    /// oneshot. `None` for messages that aren't part of a call
    /// flow. Cluster wire propagation is gated by the
    /// `INGRESS_CALL_V1` capability — peers without it strip the
    /// field outbound and ignore it inbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Phase A durable-run identity. These fields propagate only through
    /// local scheduler emissions; the existing distribution wire remains
    /// unchanged and durable emissions to remote peers are rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
}

/// Trimmed view of a [`Message`] handed to user `useMessage` handlers.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub from: String,
    pub r#type: String,
    pub payload: Value,
    pub ttl: u32,
    pub run_id: Option<String>,
    pub attempt_id: Option<String>,
}

impl IncomingMessage {
    #[must_use]
    pub fn from_message(msg: &Message) -> Self {
        Self {
            from: msg.from.clone(),
            r#type: msg.r#type.clone(),
            payload: msg.payload.clone(),
            ttl: msg.ttl,
            run_id: msg.run_id.clone(),
            attempt_id: msg.attempt_id.clone(),
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

/// **M-Ingress phase 2.** Declared call-shape for `POST /v1/nodes/:id/call`.
///
/// Each entry in [`PermissionManifest::accepts_external_call`]
/// pairs a `request_type` (the inbound ingress message type) with a
/// `reply_type` (the outgoing message type the runtime auto-tags
/// with the inbound's `correlation_id`). The component author
/// writes `useEmit('reply_type', payload)` as normal; the scheduler
/// identifies the matching outgoing emit and copies the
/// correlation_id onto exactly that one outgoing message
/// (first-match-only, so handlers emitting multiple downstream
/// messages don't accidentally tag them all as replies).
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct ExternalCall {
    pub request_type: String,
    pub reply_type: String,
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
    /// **M-Ingress.** Message types this node accepts when injected by
    /// an external HTTP client via `POST /v1/nodes/:id/messages`. Empty
    /// by default — default-deny semantics. Distinct from `accepts`,
    /// which governs node-to-node delivery; external HTTP injection is
    /// a separate trust boundary and the manifest declares opt-in.
    pub accepts_external: Vec<String>,
    /// **M-Ingress phase 2.** Call-shaped external acceptance —
    /// declares which `(request_type, reply_type)` pairs the node
    /// will service for `POST /v1/nodes/:id/call`. Inbound HTTP
    /// calls whose `type` is NOT a `request_type` here are
    /// rejected with 403 (manifest gating). Outgoing emits whose
    /// `type` matches the corresponding `reply_type` are
    /// auto-tagged with the inbound `correlation_id` so the HTTP
    /// handler can match the reply. Distinct from `accepts_external`
    /// (single-shot inject) — call shape is request/response.
    pub accepts_external_call: Vec<ExternalCall>,
    /// **M-Prod sub-track #3 Phase B Part 1.** Per-component
    /// allowlist of secret names the body declares it may read via
    /// `ctx.secret(name)`. `None` = no secrets (default-deny: every
    /// `ctx.secret(name)` call resolves to null). `Some(vec!["FOO"])`
    /// = bodies may read `FOO`; any other name still resolves to
    /// null. Mirrors the `accepts_external` default-deny shape.
    ///
    /// The Phase B Part 1 production host-callback factory (in
    /// `graphreact-cli/src/jsruntime.rs`, deferred to Part 2) reads
    /// the last-committed manifest for the active node via
    /// `Scheduler::manifests` and checks `manifest.needs_secret`
    /// before dispatching the read to the registered `SecretStore`.
    /// See ADR-0007.
    pub needs_secret: Option<Vec<String>>,
}

impl std::fmt::Debug for PermissionManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionManifest")
            .field("accepts", &self.accepts)
            .field("emits", &self.emits)
            .field("channels", &self.channels)
            .field("accepts_external", &self.accepts_external)
            .field("accepts_external_call", &self.accepts_external_call)
            .field("needs_secret", &self.needs_secret)
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
            accepts_external: Vec::new(),
            accepts_external_call: Vec::new(),
            needs_secret: None,
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
