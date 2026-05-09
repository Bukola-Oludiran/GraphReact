//! **M16-Backend Phase 1.** Telemetry sink trait + per-event payloads.
//!
//! The trait definition lives in `graphreact-core` so the [`Scheduler`] can
//! call it without depending on `graphreact-observer`. Rich consumer impls
//! (Prometheus, JSON logs, OTel, WebSocket fan-out) live in the observer
//! crate. Mirrors the layering of [`crate::runtime::scheduler::RemoteSink`]:
//! trait surface in core, runtime-shaped impls in `graphreact-dist`.
//!
//! ## Hot-path discipline (read before adding a method)
//!
//! Every `TelemetrySink` method runs from inside the scheduler hot path
//! while the scheduler holds internal `RefCell` borrows (and, for
//! [`Scheduler::install_telemetry_sink`]'s slot, a [`parking_lot::RwLock`]
//! read guard). The same constraint already documented for
//! [`crate::runtime::scheduler::SuperviseDecisionHook`] applies here:
//!
//! - Sink methods MUST NOT call back into the scheduler.
//! - Sink methods MUST NOT block (no synchronous `Mutex` waits, no I/O
//!   that could spill milliseconds).
//! - Defaults are `#[inline] fn ...(&self, _ev: &Event) {}` so a
//!   `NoopSink` compiles to a single `Option::is_some` branch on the
//!   scheduler hot path.
//!
//! Test coverage in `crates/graphreact-observer/tests/telemetry_zero_cost.rs`
//! enforces the no-borrow contract with a panicking sink.
//!
//! [`Scheduler`]: crate::runtime::scheduler::Scheduler
//! [`Scheduler::install_telemetry_sink`]:
//!   crate::runtime::scheduler::Scheduler::install_telemetry_sink

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::runtime::supervision::ExitReason;

/// Snapshot of every scheduler queue's current depth. Sampled once per
/// `process_batch` cascade round (NOT per message) at the top of the
/// loop body so steady-state load shows up as ~1 sample/ms without
/// dominating the hot path.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueDepthsSnapshot {
    pub messages: usize,
    pub dirty: usize,
    pub pending_topology: usize,
    pub external_inbox: usize,
    pub pending_mutations: usize,
}

/// Fired exactly once at the entry of every `Scheduler::process_batch`
/// call. The companion [`SchedulerTickEnd`] fires when `process_batch`
/// returns (whether by quiescence or `MaxDepthExceeded`).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct SchedulerTickStart {}

/// Fired exactly once at the exit of every `Scheduler::process_batch`
/// call, regardless of outcome. `iterations` is the cascade-round
/// depth reached. `terminated_by_depth_cap` is `true` when
/// `process_batch` returned `MaxDepthExceeded` rather than quiescing.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SchedulerTickEnd {
    pub iterations: u32,
    pub terminated_by_depth_cap: bool,
}

/// Fired once per `process_batch` cascade round, after queues have been
/// classified for that round. Use the queue depths to power
/// `grx_queue_depth_*` gauges in Phase 2 and the live-tail panel in
/// Phase 6.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct QueueDepths {
    pub depth: u32,
    pub snapshot: QueueDepthsSnapshot,
}

/// Fired immediately before `evaluate_node` runs for a single node.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct NodeEvaluateStart<'a> {
    pub node_id: &'a str,
}

/// Fired immediately after `evaluate_node` returns. Pair with the
/// preceding [`NodeEvaluateStart`] to bound per-node evaluation time
/// (Phase 10 OTel uses the pair as a span; Phase 2 Prometheus
/// increments `grx_node_renders_total` here).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct NodeEvaluateEnd<'a> {
    pub node_id: &'a str,
}

/// Fired once at the end of `Scheduler::deliver`. `accepted` reflects
/// the post-policy disposition: `false` means the channel-policy /
/// accept-list / transform path rejected the message; `true` means the
/// target's handlers ran. Direct (`Phase 9`) emissions are flagged via
/// `direct`.
///
/// **M16-Backend P10.** [`MessageDelivered::trace_context`] carries a
/// 24-byte `TraceID(16) || SpanID(8)` blob when the message arrived
/// from a remote peer with an OTel span attached at the source. The
/// `graphreact-observer::OtelSink` consumer attaches this as an OTel
/// `Link` to the receiver-side `message.deliver` span so 3-hop
/// messages produce 3-link traces in the collector view. Local
/// messages (no remote origin) leave the field `None`.
#[derive(Debug, Clone, Serialize)]
pub struct MessageDelivered<'a> {
    pub from: &'a str,
    pub to: &'a str,
    #[serde(rename = "type")]
    pub message_type: &'a str,
    pub ttl: u32,
    pub direct: bool,
    pub accepted: bool,
    /// **M16-Backend P10.** Remote OTel span context propagated from
    /// the wire packet. `Option<[u8; 24]>` =
    /// `TraceID(16) || SpanID(8)`. `None` for local-origin messages.
    /// Skipped from JSON serialization when `None` to keep the P3
    /// JSON-log shape stable for non-OTel deployments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<[u8; 24]>,
}

/// Fired by `Scheduler::add_node_at_runtime` after the node has been
/// registered + connected + its mount cycle drained. NOT fired for
/// nodes added via the initial `register_node` mount (those land
/// through the existing `process_batch` path; M14/M15 substrate
/// already covers them). Phase-2 Prometheus increments
/// `grx_node_mount_total` here.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct NodeMounted<'a> {
    pub node_id: &'a str,
}

/// Fired by `Scheduler::unmount_with_reason` after the local DOWN/EXIT
/// fan-out completes and after any `unmount_hooks` have run. Carries
/// the termination reason as a stable string (`"normal"`, `"shutdown"`,
/// `"crashed"`) plus an optional `detail` field for the `Crashed`
/// variant.
#[derive(Debug, Clone, Serialize)]
pub struct NodeUnmounted<'a> {
    pub node_id: &'a str,
    pub reason: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_detail: Option<&'a str>,
}

impl<'a> NodeUnmounted<'a> {
    /// Build a [`NodeUnmounted`] event from a node id and an
    /// [`ExitReason`]. Lives in core so callers (Scheduler) don't have
    /// to duplicate the reason-stringification logic.
    #[must_use]
    pub fn from_reason(node_id: &'a str, reason: &'a ExitReason) -> Self {
        Self {
            node_id,
            reason: reason.as_str(),
            reason_detail: reason.detail(),
        }
    }
}

/// Fired by `Scheduler::dispatch_topology_event` once per dispatched
/// topology event, regardless of how many node handlers it fans out
/// to. `kind` is one of `"node_added"`, `"node_removed"`, `"edge_added"`,
/// `"edge_removed"`, `"edge_updated"` — stable strings consumers can
/// use as Prometheus label values.
#[derive(Debug, Clone, Serialize)]
pub struct TopologyEventOccurred<'a> {
    pub kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_u: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_v: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_key: Option<&'a str>,
}

/// Action taken by the supervisor for a deferred decision. `Proceeded`
/// = hook returned `Proceed`; `Deferred` = hook re-deferred; `Escalated`
/// = `retry_after` deadline elapsed and the plan ran unconditionally.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorAction {
    Proceeded,
    Deferred,
    Escalated,
}

/// Fired by `Scheduler::drain_pending_supervisor_decisions` once per
/// re-evaluated decision, AFTER the corresponding supervisor plan has
/// run (or been parked / escalated). Captures the failed child + the
/// termination reason + the action taken.
#[derive(Debug, Clone, Serialize)]
pub struct SupervisorDecisionMade<'a> {
    pub failed_id: &'a str,
    pub reason: &'a str,
    pub action: SupervisorAction,
}

/// Direction of a remote message relative to this node. Outbound =
/// `make_emit` shipped to a peer; Inbound = the dist crate's accept
/// loop received a message from a peer and routed it through
/// `Scheduler::send`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteDirection {
    Outbound,
    Inbound,
}

/// Fired from `graphreact-dist` (NOT the scheduler — see option (a) in
/// the M16-Backend Phase-1 architectural decision) when a `Message` is
/// shipped to or received from a remote peer. The dist crate holds
/// its own [`TelemetrySink`] slot installed via
/// `Cluster::install_telemetry_sink`.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteMessage<'a> {
    pub direction: RemoteDirection,
    pub peer: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    #[serde(rename = "type")]
    pub message_type: &'a str,
    pub byte_size: usize,
}

/// Fired from `graphreact-hotreload`'s `ReloadOrchestrator::apply` after
/// a delta has been applied (or rolled back). The orchestrator holds
/// its own [`TelemetrySink`] slot installed via
/// `ReloadOrchestrator::install_telemetry_sink`.
///
/// `nodes_replaced` counts compatible-swap successes, `nodes_remounted`
/// counts incompatible drains + remounts, `edges_changed` counts
/// added/removed/updated edges. `rolled_back` is `true` if the
/// post-swap render errored or panicked and the orchestrator restored
/// prior definitions. `error_count` is the number of per-node /
/// per-edge errors collected across the apply (orchestrator does not
/// abort on first error — see Phase 6/7 substrate at
/// `crates/graphreact-hotreload/src/orchestrator.rs`).
#[derive(Debug, Clone, Serialize)]
pub struct HotReload<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_peer: Option<&'a str>,
    pub nodes_replaced: usize,
    pub nodes_remounted: usize,
    pub edges_changed: usize,
    pub rolled_back: bool,
    pub error_count: usize,
}

/// **M16-Backend P4.** Snapshot of a single registered node, suitable
/// for serving via the management API's `GET /v1/nodes` and
/// `GET /v1/nodes/{id}` endpoints. Built once per scrape from the
/// scheduler's [`crate::runtime::directory::GraphDirectory`] +
/// supervisor reverse index. `serde::Serialize` is derived so the
/// management API handler can return it directly via `axum::Json`.
#[derive(Debug, Clone, Serialize)]
pub struct NodeIntrospection {
    pub id: String,
    pub role: String,
    pub depth: u32,
    pub path: Vec<String>,
    /// Stable supervisor key (e.g. `"sup:1"`) when this node is
    /// under a supervisor; `None` for top-level nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<String>,
}

/// **M16-Backend P4.** Snapshot of one supervisor's runtime state,
/// suitable for serving `GET /v1/supervisor/tree`.
#[derive(Debug, Clone, Serialize)]
pub struct SupervisorIntrospection {
    /// Stable string key generated from the underlying `SupervisorKey`
    /// slotmap index — `"sup:0"`, `"sup:1"`, etc. Stable across the
    /// lifetime of one daemon process; resets on restart.
    pub key: String,
    /// One of `"OneForOne"`, `"OneForAll"`, `"RestForOne"`.
    pub strategy: String,
    /// Configured intensity ceiling.
    pub max_restarts: u32,
    /// Configured intensity window in seconds.
    pub period_secs: u64,
    /// Number of restarts currently inside the active window.
    pub restart_count: usize,
    /// Node IDs currently mounted under this supervisor. Resolved
    /// from `node_to_supervisor` reverse index — reflects RUNTIME
    /// state, not the registered child specs (which may include
    /// children awaiting restart).
    pub children: Vec<String>,
}

/// Sized wrapper around `Arc<dyn TelemetrySink>`. Required because
/// `arc_swap::ArcSwapOption<T>` requires `Arc<T>: arc_swap::RefCnt`,
/// which has a `T: Sized` bound — so `Arc<dyn TelemetrySink>` can't
/// be stored directly in an `ArcSwap`. Wrapping makes the outer type
/// Sized; the inner `Arc<dyn TelemetrySink>` is a (data-ptr, vtable-
/// ptr) fat pointer pair.
///
/// Implements `Deref<Target = dyn TelemetrySink>` so callsites can
/// invoke trait methods directly without a `.0.` field access.
pub struct TelemetrySinkRef(pub Arc<dyn TelemetrySink>);

impl TelemetrySinkRef {
    /// Wrap an `Arc<dyn TelemetrySink>` for storage in an `ArcSwap`.
    #[must_use]
    pub fn new(sink: Arc<dyn TelemetrySink>) -> Self {
        Self(sink)
    }
}

impl std::ops::Deref for TelemetrySinkRef {
    type Target = dyn TelemetrySink + 'static;

    #[inline]
    fn deref(&self) -> &(dyn TelemetrySink + 'static) {
        &*self.0
    }
}

/// Telemetry consumer trait. Every method has a no-op default so
/// consumers opt into the events they care about. The [`Scheduler`]
/// holds an `Option<Arc<dyn TelemetrySink>>`; uninstalled = single
/// `Option::is_some` branch on the hot path.
///
/// **Hard contract**: implementations MUST NOT call back into the
/// scheduler — see the module-level doc for the full discipline.
///
/// [`Scheduler`]: crate::runtime::scheduler::Scheduler
pub trait TelemetrySink: Send + Sync {
    #[inline]
    fn on_scheduler_tick_start(&self, _ev: &SchedulerTickStart) {}

    #[inline]
    fn on_scheduler_tick_end(&self, _ev: &SchedulerTickEnd) {}

    #[inline]
    fn on_queue_depths(&self, _ev: &QueueDepths) {}

    #[inline]
    fn on_node_evaluate_start(&self, _ev: &NodeEvaluateStart<'_>) {}

    #[inline]
    fn on_node_evaluate_end(&self, _ev: &NodeEvaluateEnd<'_>) {}

    #[inline]
    fn on_message_delivered(&self, _ev: &MessageDelivered<'_>) {}

    #[inline]
    fn on_node_mounted(&self, _ev: &NodeMounted<'_>) {}

    #[inline]
    fn on_node_unmounted(&self, _ev: &NodeUnmounted<'_>) {}

    #[inline]
    fn on_topology_event(&self, _ev: &TopologyEventOccurred<'_>) {}

    #[inline]
    fn on_supervisor_decision(&self, _ev: &SupervisorDecisionMade<'_>) {}

    #[inline]
    fn on_remote_message(&self, _ev: &RemoteMessage<'_>) {}

    #[inline]
    fn on_hot_reload(&self, _ev: &HotReload<'_>) {}
}
