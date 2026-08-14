//! Reactive scheduler — message queue, dirty queue, processing loop.
//!
//! M4 baseline:
//!
//! * `BTreeSet<(Height, NodeKey)>` dirty queue, drained in height order so
//!   parents settle before children.
//! * FIFO message queue with TTL-driven cycle protection.
//! * Channel policies resolved per `(sender, target)` pair, transforms
//!   applied during delivery.
//! * `process_batch` drains both queues until quiescent or
//!   [`MAX_PROCESS_DEPTH`] iterations elapse.
//!
//! The scheduler is intentionally `!Send` — it owns `Rc`-shared state.
//! Daemon mode + multi-thread variants land in M5.

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use serde_json::Value;
use slotmap::{new_key_type, SlotMap};
use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use crate::graph::Subscription;

use crate::error::RuntimeError;
use crate::graph::{Attrs, CommunicationGraph, TopologyEvent};
use crate::runtime::constants::{DEFAULT_TTL, MAX_PROCESS_DEPTH};
use crate::runtime::directory::{DirectoryEntry, GraphDirectory};
use crate::runtime::hooks::{set_hook_ctx, HookCtx};
use crate::runtime::lifecycle::{
    evaluate_node, evaluate_node_async, unmount_node, unmount_node_async,
};
use crate::runtime::message::{create_message, decrement_ttl};
use crate::runtime::node_instance::{NodeDefinition, NodeInstance};
use crate::runtime::permissions::resolve_channel_policy;
use crate::runtime::sources::EventSource;
use crate::runtime::supervision::postpone::take_postpone_flag;
use crate::runtime::supervision::{
    ChildSpec, ExitReason, GroupRegistry, LinkRegistry, MonitorRef, MonitorRegistry, PostponeQueue,
    RestartPolicy, Supervisor, SupervisorError, SYSTEM_DOWN, SYSTEM_EXIT,
};
use crate::runtime::types::{
    ComposeChildSpec, DurableActivation, DurableEffectIntent, DurableEffectRequest,
    DurableStateSnapshot, EmitOptions, IncomingMessage, Message, NodeContext, PermissionManifest,
    TopologyEventKind,
};

new_key_type! {
    /// Internal slot key for node instances within the scheduler.
    pub struct NodeKey;
    /// Internal slot key for supervisors installed on the scheduler.
    pub struct SupervisorKey;
}

/// Height of a node in the hierarchy — used for dirty-queue ordering.
pub type Height = u32;

/// Storage-agnostic host callback invoked at the durable activation boundary.
pub type DurableActivationCommitter = Rc<dyn Fn(&DurableActivation) -> Result<(), RuntimeError>>;

/// **M-Ingress phase 2.** Per-handler-invocation context capturing
/// the inbound HTTP-call shape so `make_emit` can auto-tag the
/// reply.
///
/// `correlation_id` is the opaque id minted by the call handler;
/// `reply_type` is the message type the runtime should auto-tag
/// (sourced from the matching `accepts_external_call` entry's
/// `reply_type`); `tagged` flips on first match so a handler
/// emitting multiple downstream messages doesn't accidentally tag
/// them all as replies.
#[derive(Debug, Clone)]
struct CallContext {
    correlation_id: String,
    reply_type: String,
    tagged: bool,
}

#[derive(Debug, Clone)]
struct DurableRunContext {
    run_id: String,
    attempt_id: String,
}

#[derive(Default)]
struct DurableActivationBuffer {
    emitted_messages: Vec<Message>,
    effect_intents: Vec<DurableEffectIntent>,
    effect_keys: std::collections::BTreeSet<String>,
}

/// Reactive scheduler. Owns nodes, the message queue, and the dirty queue.
///
/// Single-threaded by design — `Rc` and `RefCell` carry shared state. Use
/// the host's tokio runtime to drive `process_batch` from async contexts.
pub struct Scheduler {
    /// The live communication graph. Wrapped in `Rc<RefCell<_>>` (M12.3a-followon-C,
    /// 2026-05-02) so external consumers — notably `graphreact-cli`'s
    /// `__graphQueryProxy` — can hold a live handle that observes runtime
    /// mutations (`spawnChild`, `compose`, `acceptEdge`) instead of a frozen
    /// snapshot. In-crate call sites borrow on demand; mutations go through
    /// `borrow_mut()`.
    pub graph: Rc<RefCell<CommunicationGraph>>,
    pub directory: Rc<RefCell<GraphDirectory>>,
    /// Live index of permission manifests keyed by node id, kept in
    /// sync with `NodeInstance::manifest` after each evaluate. Exposed
    /// as an `Rc<RefCell<_>>` so external consumers (notably
    /// `graphreact-cli`'s Tier 2 / Tier 3 query plumbing) can hold a
    /// shared handle that observes runtime mutations as nodes mount /
    /// unmount / re-render with updated manifests. Populated by
    /// `evaluate` / `evaluate_async` (post body execution) and pruned
    /// by `unmount` / `unmount_async`. M12.3a-followon-E.
    pub manifests: Rc<RefCell<FxHashMap<String, PermissionManifest>>>,
    /// Live index of per-node edge-accept predicates, keyed by node id.
    /// Mirrors the `manifests` index lifecycle: populated post-eval from
    /// `NodeInstance::edge_accept_predicate`, pruned on unmount. Consumed
    /// by `drain_pending_mutations::AcceptEdge` (veto path) and by
    /// `graphreact-cli`'s Tier 3 query plumbing. Nodes without an entry
    /// inherit the default-accept behavior (preserves Phase-9 status
    /// quo). M12.3a-followon-F.
    pub edge_accept_predicates:
        Rc<RefCell<FxHashMap<String, crate::runtime::node_instance::EdgeAcceptPredicate>>>,
    nodes: SlotMap<NodeKey, Rc<RefCell<NodeInstance>>>,
    id_to_key: FxHashMap<String, NodeKey>,
    heights: FxHashMap<NodeKey, Height>,

    messages: Rc<RefCell<VecDeque<Message>>>,
    dirty: Rc<RefCell<BTreeSet<(Height, NodeKey)>>>,
    sources: Rc<RefCell<FxHashMap<String, NodeKey>>>,
    /// TTL of the message currently being delivered. Inherited by `emit`
    /// calls inside message handlers so cycles bound at the original TTL.
    /// `None` outside a delivery — `emit` then uses the default TTL.
    current_delivery_ttl: Rc<std::cell::Cell<Option<u32>>>,
    /// TTL of the most-recently-received message at each node, captured
    /// during `deliver()` and read by `make_emit` so that emits issued
    /// from a re-rendered node's `flush_effects` (after the synchronous
    /// handler has returned and `current_delivery_ttl` has been reset)
    /// still inherit the cycle's decaying TTL. Cleaned up on unmount.
    /// Closes the M4 scheduler-level gap surfaced by M12.3a-phase3d
    /// (ping-pong ran unbounded because each re-emit reset to
    /// `DEFAULT_TTL`).
    last_received_ttl: Rc<RefCell<FxHashMap<NodeKey, u32>>>,

    /// **M-Ingress phase 2.** Per-handler-invocation context for
    /// auto-propagating `correlation_id` from an inbound HTTP-call
    /// message onto the outgoing reply. Set by `deliver()` BEFORE
    /// invoking handlers when the inbound `Message.correlation_id`
    /// is `Some(_)` AND the target's manifest declares a matching
    /// `accepts_external_call` entry; cleared on handler exit (with
    /// save/restore so nested deliveries don't cross-contaminate).
    /// Read by `make_emit` to overlay the captured correlation_id
    /// onto exactly the first outgoing message whose `type` matches
    /// `reply_type` — `tagged` flips on first match so
    /// multi-emit handlers don't accidentally tag downstream
    /// messages as replies.
    current_call_context: Rc<RefCell<Option<CallContext>>>,
    /// Run identity active while a durable message handler executes. Local
    /// emits inherit it; remote emits are suppressed during Phase A.
    current_durable_run_context: Rc<RefCell<Option<DurableRunContext>>>,
    durable_activation_committer: Option<DurableActivationCommitter>,
    /// Local output messages produced by the active durable handler. `None`
    /// means the compatibility path should enqueue directly.
    durable_output_buffer: Rc<RefCell<Option<DurableActivationBuffer>>>,

    /// Active monitors. Drained per-target on `unmount` to deliver
    /// `__DOWN__` system messages.
    monitors: Rc<RefCell<MonitorRegistry>>,
    /// Bidirectional links. Drained per-node on `unmount` to deliver
    /// `__EXIT__` system messages.
    links: Rc<RefCell<LinkRegistry>>,
    /// pg-style process groups. Auto-cleans on `unmount`.
    groups: Rc<RefCell<GroupRegistry>>,
    /// Per-node postpone queues. Populated by `deliver()` when a handler
    /// invokes [`postpone`]; drained by `evaluate()` so postponed
    /// messages re-enter the main queue at the next state transition.
    ///
    /// [`postpone`]: crate::runtime::supervision::postpone
    postponed: Rc<RefCell<FxHashMap<NodeKey, PostponeQueue>>>,

    /// Topology events captured from the graph since the last drain.
    /// Populated by a graph listener (`Send + Sync` requirement → `Arc<Mutex<_>>`)
    /// and drained synchronously on every `process_batch`.
    pending_topology: Arc<Mutex<Vec<TopologyEvent>>>,
    /// Hold the listener alive — drop is the unsubscribe.
    _topology_subscription: Subscription,

    /// Wake-up signal for the daemon mode loop. Notified whenever new
    /// work appears (message enqueued, dirty mark, source push, topology
    /// event). `Send + Sync`-friendly so cross-thread signals (sources
    /// running on tokio workers) can wake the local-set scheduler.
    notify: Arc<tokio::sync::Notify>,
    /// Daemon mode termination flag. Toggled by `stop_daemon`, polled
    /// after each batch.
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Cross-thread message inbox. External callers (other tokio tasks)
    /// push through a [`SchedulerHandle`]; the daemon drains this into
    /// the local message queue at the head of each batch. Avoids the
    /// `Rc<RefCell<Scheduler>>` double-borrow that arises if a pusher
    /// task tries to call `&self` methods while `run_daemon` holds
    /// the scheduler.
    external_inbox: Arc<Mutex<Vec<Message>>>,

    /// Installed supervisors. A supervisor governs the lifecycle of a
    /// set of children — abnormal exits trigger its restart strategy.
    supervisors: SlotMap<SupervisorKey, Supervisor>,
    /// Reverse index: which supervisor (if any) governs a given node.
    /// Populated by `supervise` and consulted by `unmount_with_reason`
    /// on abnormal exit.
    node_to_supervisor: FxHashMap<NodeKey, SupervisorKey>,

    running: RefCell<bool>,

    /// Per-instance cap on `process_batch` cascade rounds. Defaults to the
    /// crate-level [`MAX_PROCESS_DEPTH`]. Hosts with legitimately deep
    /// cascades (recursive type trees, deep dependency DAGs) bump this via
    /// [`Self::set_max_process_depth`].
    max_process_depth: usize,

    /// Mutations enqueued by `ctx.spawnChild` / `ctx.unmount` /
    /// `ctx.compose` during evaluation. The scheduler can't apply these
    /// inline (it holds a `&mut self` borrow during a node's component
    /// callback) so they're deferred and drained per cascade round.
    pending_mutations: Rc<RefCell<Vec<PendingMutation>>>,

    /// External wake-up bridge — invoked from any thread after a source
    /// pushes into `external_inbox`. Hosts that drive evaluation on a
    /// dedicated thread (e.g., the napi bridge running on the V8 main
    /// thread) install a closure here that thread-safely posts a
    /// `process_batch` request to that thread (typically via a
    /// threadsafe-function). The daemon loop's `notify` handles
    /// in-process wake-ups; this handles cross-thread ones.
    external_wake: parking_lot::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,

    /// **M14 distribution hook.** Optional callback consulted from
    /// `make_emit` when a target ID resolves to a *remote* node
    /// (i.e. the target string contains `@<some-other-node>` and we
    /// have a `node_name` set). The dist crate installs this to
    /// route messages over TLS to remote peers; if not installed,
    /// remote-shaped targets are silently dropped (matches OTP's
    /// "send to unknown node" behavior). Sink return value: `true`
    /// = "I handled the routing", `false` = "decline; fall through
    /// to the default drop".
    remote_sink: parking_lot::RwLock<Option<RemoteSink>>,

    /// **M14 distribution hook.** Cluster-unique node name (the
    /// `name` part of a `name@host` `NodeId`). When set, `make_emit`
    /// recognises `target@<our_name>` as a *local* address (strips
    /// the suffix and routes locally) and `target@<other_name>` as
    /// a *remote* address (consults `remote_sink`). Targets without
    /// `@` are always treated as local (today's behavior, unchanged).
    node_name: parking_lot::RwLock<Option<String>>,

    /// **M14.5 distribution hook.** Single shared sentinel `NodeKey`
    /// used as the `target` for every cross-node monitor and link.
    /// Allocated once via [`Self::set_remote_sentinel`]; lives in
    /// `nodes` as a synthetic placeholder (never registered in
    /// `id_to_key`, never enters the dirty queue, never receives
    /// messages directly). Per-peer indexing of remote refs lives
    /// in the dist crate's `RemoteSupervision`; this NodeKey only
    /// exists so the local `MonitorRegistry` / `LinkRegistry` have
    /// a valid target to associate with.
    remote_sentinel: parking_lot::RwLock<Option<NodeKey>>,

    /// **M14.5 distribution hook.** Optional callback fired from
    /// inside a node body's `use_monitor("name@other")` after the
    /// local stub `MonitorRef` has been allocated. The dist crate
    /// installs this to ship a `MonitorRequest` packet to the peer.
    remote_monitor_sink: parking_lot::RwLock<Option<RemoteMonitorSink>>,

    /// **M14.5 distribution hook.** Optional callback fired from
    /// `use_link("name@other")` after the (observer, sentinel) link
    /// has been recorded locally. Returns `true` if the dist crate
    /// considered the link request routable. The bool propagates
    /// back through [`crate::runtime::hooks::use_link`] so callers
    /// can branch on cluster-membership.
    remote_link_sink: parking_lot::RwLock<Option<RemoteLinkSink>>,

    /// **M14.5 distribution hook.** Optional callback fired from
    /// `use_unlink("name@other")` so the dist crate can ship an
    /// `Unlink` packet and decrement its per-observer link counter.
    remote_unlink_sink: parking_lot::RwLock<Option<RemoteUnlinkSink>>,

    /// **M14.5 distribution hook.** `Send + Sync` queue of
    /// `MonitorRef`s the dist crate wants the scheduler thread to
    /// remove from `MonitorRegistry`. Drained at the head of every
    /// `process_batch`. Pushed to from tokio worker threads via
    /// [`SchedulerHandle::demonitor_remote`] when a peer connection
    /// drops or an inbound `Demonitor` packet arrives — neither
    /// path can touch the `Rc<RefCell<MonitorRegistry>>` directly
    /// because it is `!Send`.
    pending_remote_demonitors: Arc<Mutex<Vec<MonitorRef>>>,

    /// **M14.5 distribution hook.** `Send + Sync` queue of observer
    /// `NodeKey`s whose link to the remote sentinel should be
    /// removed at the next batch boundary. Pushed to from tokio
    /// worker threads via [`SchedulerHandle::unlink_remote`] when a
    /// peer disconnects and the observer's per-peer remote-link
    /// count drops to zero. Drained alongside
    /// `pending_remote_demonitors`.
    pending_remote_unlinks: Arc<Mutex<Vec<NodeKey>>>,

    /// **M-Prod sub-track #4 Phase γ (M14.11).** `Send + Sync` queue of
    /// cross-node delegated-restart requests pushed by the dist
    /// crate's `dispatch_inbound` arm via
    /// [`SchedulerHandle::request_delegated_restart`]. Drained at the
    /// head of every `process_batch` round. The drain logic looks up
    /// the named child via `id_to_key` + `node_to_supervisor` and
    /// drives the EXISTING `unmount_with_reason` → supervisor-plan
    /// path with an `ExitReason::Crashed { detail }` synthesizing the
    /// origin peer + correlation id — this PRESERVES the existing
    /// `Supervisor::Strategy` / `should_restart` contract bit-for-bit;
    /// the operator-driven restart is treated as the same abnormal
    /// exit shape any local fault would produce, gated by the same
    /// `RestartPolicy` + intensity tracker. Per-request outcomes are
    /// pushed onto [`Self::pending_delegated_restart_outcomes`] for
    /// the dist crate to drain + ship as `RestartDelegationResult`
    /// packets.
    pending_delegated_restarts: Arc<Mutex<Vec<DelegatedRestartRequest>>>,

    /// **Phase γ (M14.11).** `Send + Sync` queue of outcomes from
    /// the [`Self::pending_delegated_restarts`] drain. Pushed by the
    /// scheduler thread after each drain; polled from the dist
    /// crate's per-connection task via
    /// [`SchedulerHandle::poll_delegated_restart_outcomes`].
    pending_delegated_restart_outcomes: Arc<Mutex<Vec<DelegatedRestartOutcome>>>,

    /// **M14.5 distribution hook.** Callbacks fired from
    /// [`Self::unmount_with_reason`] after the local DOWN/EXIT
    /// fan-out completes. The dist crate registers one hook to
    /// (a) ship wire `Down` packets for inbound monitors against
    /// the unmounting id, (b) ship wire `Exit` packets for
    /// inbound links, and (c) prune its outbound translation
    /// tables when the unmounting node is the observer/linker of
    /// any remote ref.
    unmount_hooks: parking_lot::RwLock<Vec<UnmountHook>>,

    /// **M14.6 distribution hook.** Callback fired by
    /// [`Self::make_group_join`] / [`Self::make_group_leave`]
    /// (and indirectly by [`Self::unmount_with_reason`] when an
    /// unmounting node was a group member). The dist crate ships
    /// `PgJoin` / `PgLeave` packets to all connected peers and
    /// updates its local view. `None` for non-distributed
    /// schedulers — group state stays single-node.
    remote_group_sink: parking_lot::RwLock<Option<RemoteGroupSink>>,

    /// **M14.6 distribution hook.** Callback fired by
    /// [`Self::group_broadcast`] AFTER local fan-out. The dist
    /// crate uses it to send one `PgBroadcast` packet per remote
    /// peer holding ≥1 member of `group` (hub-and-spoke).
    remote_broadcast_sink: parking_lot::RwLock<Option<RemoteBroadcastSink>>,

    /// **M14.10 distribution hook.** Optional decision-gating hook
    /// consulted before [`Self::execute_supervisor_plan`] runs. The
    /// dist crate installs this with a closure that queries its
    /// partition view; under PauseOnSplit, supervisors of children
    /// dependent on partitioned peers can defer their restart until
    /// the peer heals or grace expires.
    supervise_decision_hook: parking_lot::RwLock<Option<SuperviseDecisionHook>>,

    /// **M14.10.** Restart decisions parked by the
    /// [`SuperviseDecisionHook`]. Re-evaluated at the head of every
    /// `process_batch` round (see
    /// [`Self::drain_pending_supervisor_decisions`]). Past their
    /// `retry_after` deadline, the plan runs unconditionally
    /// (escalation).
    pending_supervisor_decisions: RefCell<Vec<DeferredSupervisorDecision>>,

    /// **M16-Backend Phase 1.** Optional telemetry sink. Consulted at
    /// every scheduler hot-path boundary (tick start/end, queue
    /// depths, node evaluate start/end, message deliver, mount,
    /// unmount, topology dispatch, supervisor decision). Uninstalled
    /// = single `Option::is_some` branch on the hot path.
    ///
    /// Storage: `ArcSwapOption` — lock-free reads (~3-5ns) without
    /// any `Arc::clone` per callsite, satisfying the "(b) within 1%
    /// of (a)" bench gate. The previous `parking_lot::RwLock<Option<
    /// Arc<dyn TelemetrySink>>>` shape required a read lock + Arc
    /// clone per snapshot, costing ~40ns/tick which pushed (b) over
    /// the gate. `ArcSwap`'s `Guard` holds a hazard-pointer reference
    /// for the duration of the trait method call without bumping
    /// the strong count.
    ///
    /// Sink methods MUST NOT borrow the scheduler — same constraint
    /// as [`SuperviseDecisionHook`]. See
    /// [`crate::telemetry`] for the full discipline.
    telemetry: arc_swap::ArcSwapOption<crate::telemetry::TelemetrySinkRef>,
}

/// Type alias for the remote-routing callback installed by the dist
/// crate. Must be `Send + Sync` because the closure is invoked from
/// inside hook closures that may be called from any thread the
/// scheduler is mounted on.
pub type RemoteSink = std::sync::Arc<dyn Fn(&Message) -> bool + Send + Sync>;

/// **M14.5.** Callback fired after a local stub `MonitorRef` is
/// allocated for a `use_monitor("name@other")`. Receives the
/// observer's `NodeKey`, the observer's bare local id, the full
/// remote-target string, and the freshly allocated ref. The dist
/// crate uses these to ship a `MonitorRequest` packet and to record
/// the entry in its per-peer fan-out index.
pub type RemoteMonitorSink = std::sync::Arc<dyn Fn(NodeKey, &str, &str, MonitorRef) + Send + Sync>;

/// **M14.5.** Callback fired after a `(observer, sentinel)` link is
/// recorded locally for a `use_link("name@other")`. Receives the
/// observer's `NodeKey`, the observer's bare local id, and the
/// full remote-target string. Returns `true` if the dist crate
/// considered the request routable.
pub type RemoteLinkSink = std::sync::Arc<dyn Fn(NodeKey, &str, &str) -> bool + Send + Sync>;

/// **M14.5.** Callback fired by `use_unlink("name@other")`. Same
/// arguments as [`RemoteLinkSink`]; the dist crate ships an
/// `Unlink` packet and updates its per-observer link counter,
/// pushing onto `pending_remote_unlinks` if the observer no longer
/// has any remote links pending.
pub type RemoteUnlinkSink = std::sync::Arc<dyn Fn(NodeKey, &str, &str) + Send + Sync>;

/// **M14.5.** Callback fired by [`Scheduler::unmount_with_reason`]
/// after the local `__DOWN__` / `__EXIT__` fan-out completes.
/// Receives the unmounting node's id plus its termination reason.
/// The dist crate uses this to (a) ship wire `Down` packets for
/// any inbound monitors targeting the unmounting node and (b)
/// prune outbound translation tables that may contain stale
/// references to the now-unmounted observer.
pub type UnmountHook = std::sync::Arc<dyn Fn(&str, &ExitReason) + Send + Sync>;

/// **M14.6.** Group-membership operation. Carried by the
/// [`RemoteGroupSink`] callback to indicate whether a member is
/// joining or leaving a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupOp {
    Join,
    Leave,
}

/// **M14.6.** Callback fired after a local node joins or leaves a
/// pg-style group. Receives the operation, the group name, and
/// the bare local id of the joining/leaving node. The dist crate
/// uses it to ship `PgJoin` / `PgLeave` packets to peers and to
/// update its own view of local membership for inbound
/// `PgBroadcast` fan-out.
pub type RemoteGroupSink = std::sync::Arc<dyn Fn(GroupOp, &str, &str) + Send + Sync>;

/// **M14.10.** Decision returned by [`SuperviseDecisionHook`] when
/// a supervisor is about to restart a failed child. Allows external
/// observers (today: the dist crate, querying its partition view)
/// to defer a restart until the partition heals or escalate after
/// a grace window.
#[derive(Debug, Clone)]
pub enum SuperviseDecision {
    /// Run the supervisor plan immediately. The default behaviour
    /// when no hook is installed.
    Proceed,
    /// Park the decision; re-evaluate the hook on every
    /// `process_batch` until either the hook returns
    /// [`SuperviseDecision::Proceed`] or `retry_after` elapses (in
    /// which case the plan runs as if the hook returned `Proceed`).
    /// The `reason` is human-readable and surfaced through tracing
    /// for ops visibility.
    Defer {
        reason: String,
        retry_after: Option<std::time::Duration>,
    },
}

/// **M14.10.** Hook consulted before [`Scheduler::execute_supervisor_plan`]
/// runs. Receives the supervisor key, the failed child's id, and
/// the termination reason. Return [`SuperviseDecision::Proceed`] to
/// run the plan; [`SuperviseDecision::Defer`] to park it.
///
/// The hook MUST NOT borrow the scheduler — it is invoked while the
/// scheduler holds internal `RefCell` borrows during the unmount
/// path. Same constraint as [`UnmountHook`].
pub type SuperviseDecisionHook =
    std::sync::Arc<dyn Fn(SupervisorKey, &str, &ExitReason) -> SuperviseDecision + Send + Sync>;

/// **M-Prod sub-track #4 Phase γ (M14.11).** Cross-node delegated-
/// restart request — pushed by the dist crate's per-connection
/// `dispatch_inbound` arm onto the
/// [`SchedulerHandle::request_delegated_restart`] cross-thread
/// queue, drained by the scheduler thread at the head of
/// `process_batch`. The drain looks up the named child via
/// `id_to_key` + `node_to_supervisor`, then calls the existing
/// `unmount_with_reason` → `execute_supervisor_plan` path with a
/// synthesized `ExitReason::Crashed { detail }` that names the
/// origin peer + correlation id. The existing `Supervisor::Strategy`
/// / `RestartPolicy::should_restart` contract is preserved
/// bit-for-bit — the operator-driven restart is the same abnormal
/// exit shape a local fault would produce.
#[derive(Debug, Clone)]
pub struct DelegatedRestartRequest {
    /// Bare authenticated peer name of the originator.
    pub origin_peer: String,
    /// Per-origin correlation id (carried for outcome correlation +
    /// audit trails).
    pub correlation_id: u64,
    /// Child node id to restart on this node. Looked up via
    /// `id_to_key`; missing children resolve to
    /// [`DelegatedRestartOutcome`] with `error_kind = "unknown_child"`.
    pub child_node_id: String,
    /// Stable string form of the reason code (snake_case, matches
    /// the wire enum's serde rename — Phase γ uses
    /// `"supervisor_escalation"`, `"operator_request"`,
    /// `"reload_driven"`, `"health_check_failure"`).
    pub reason_code: &'static str,
}

/// **Phase γ (M14.11).** Outcome of a single
/// [`DelegatedRestartRequest`] drain. Pushed onto
/// [`Scheduler::pending_delegated_restart_outcomes`] by the
/// scheduler thread; drained by the dist crate's per-connection
/// task via
/// [`SchedulerHandle::poll_delegated_restart_outcomes`].
#[derive(Debug, Clone)]
pub struct DelegatedRestartOutcome {
    /// Echoes [`DelegatedRestartRequest::origin_peer`].
    pub origin_peer: String,
    /// Echoes [`DelegatedRestartRequest::correlation_id`].
    pub correlation_id: u64,
    /// Echoes [`DelegatedRestartRequest::child_node_id`].
    pub child_node_id: String,
    /// `true` if the supervisor's restart plan ran successfully
    /// (the child re-appeared in `id_to_key` after `unmount_with_reason`
    /// fired). `false` if the child was missing, not supervised, or
    /// the supervisor's policy/intensity refused.
    pub completed: bool,
    /// Stable machine-grep tag describing the failure mode. `None`
    /// when `completed = true`.
    pub error_kind: Option<&'static str>,
    /// Human-readable failure description. `None` when
    /// `completed = true`.
    pub reason: Option<String>,
    /// Wall-clock duration of the local restart attempt (from the
    /// drain entry through the post-`unmount_with_reason` check).
    pub duration_ms: u64,
}

/// **M14.10.** Internal record of a supervisor decision that the
/// hook deferred. Kept on the scheduler; re-evaluated at the head
/// of every `process_batch`.
#[derive(Debug, Clone)]
struct DeferredSupervisorDecision {
    sup_key: SupervisorKey,
    failed_id: String,
    reason: ExitReason,
    deferred_at: std::time::Instant,
    retry_after: Option<std::time::Duration>,
}

/// **M14.6.** Callback fired by [`Scheduler::group_broadcast`]
/// AFTER local-member fan-out. The dist crate sends one
/// `PgBroadcast` packet per peer that hosts at least one member
/// of `group`. The `from` argument is already qualified with the
/// local node name (or unchanged if already qualified) so peers
/// receive an authoritative `from` field.
pub type RemoteBroadcastSink =
    std::sync::Arc<dyn Fn(&str, &str, &str, &serde_json::Value) + Send + Sync>;

/// Mutation a hook callback wants the scheduler to apply once the current
/// evaluation pass finishes. Drained at the head of each `process_batch`
/// cascade round so spawned/unmounted nodes participate in the next pass.
pub enum PendingMutation {
    /// Spawn a new child node — wraps `add_node_at_runtime`. Idempotent on
    /// existing IDs (skipped without error).
    SpawnChild {
        spec: ComposeChildSpec,
        parent_id: String,
    },
    /// Unmount a node — wraps `remove_node_at_runtime`.
    UnmountNode { id: String },
    /// Set the parent's `composed_children` list and spawn any missing
    /// children. Children are auto-connected to the parent if not already
    /// listed in their `connect_to`.
    Compose {
        parent_id: String,
        specs: Vec<ComposeChildSpec>,
    },
    /// Phase-9 edge negotiation — `ctx.acceptEdge(target_id)`. Adds an
    /// undirected edge between `from` (active node) and `to` (target).
    /// Applied inside `drain_pending_mutations` so the graph isn't
    /// mutated mid-evaluation. No-op if either endpoint is missing or
    /// the edge already exists.
    AcceptEdge { from: String, to: String },
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new(CommunicationGraph::new())
    }
}

impl Scheduler {
    #[must_use]
    pub fn new(graph: CommunicationGraph) -> Self {
        let pending_topology: Arc<Mutex<Vec<TopologyEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let notify = Arc::new(tokio::sync::Notify::new());
        let queue_clone = Arc::clone(&pending_topology);
        let notify_clone = Arc::clone(&notify);
        let _topology_subscription = graph.on_topology_change(move |event: &TopologyEvent| {
            queue_clone.lock().push(event.clone());
            // Wake any daemon loop — graph mutations happening on
            // another thread (or from inside a hook) should never leave
            // the scheduler asleep with pending topology events.
            notify_clone.notify_one();
        });
        Self {
            graph: Rc::new(RefCell::new(graph)),
            directory: Rc::new(RefCell::new(GraphDirectory::new())),
            manifests: Rc::new(RefCell::new(FxHashMap::default())),
            edge_accept_predicates: Rc::new(RefCell::new(FxHashMap::default())),
            nodes: SlotMap::with_key(),
            id_to_key: FxHashMap::default(),
            heights: FxHashMap::default(),
            messages: Rc::new(RefCell::new(VecDeque::new())),
            dirty: Rc::new(RefCell::new(BTreeSet::new())),
            sources: Rc::new(RefCell::new(FxHashMap::default())),
            current_delivery_ttl: Rc::new(std::cell::Cell::new(None)),
            last_received_ttl: Rc::new(RefCell::new(FxHashMap::default())),
            current_call_context: Rc::new(RefCell::new(None)),
            current_durable_run_context: Rc::new(RefCell::new(None)),
            durable_activation_committer: None,
            durable_output_buffer: Rc::new(RefCell::new(None)),
            monitors: Rc::new(RefCell::new(MonitorRegistry::new())),
            links: Rc::new(RefCell::new(LinkRegistry::new())),
            groups: Rc::new(RefCell::new(GroupRegistry::new())),
            postponed: Rc::new(RefCell::new(FxHashMap::default())),
            pending_topology,
            notify,
            stop_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            external_inbox: Arc::new(Mutex::new(Vec::new())),
            supervisors: SlotMap::with_key(),
            node_to_supervisor: FxHashMap::default(),
            _topology_subscription,
            running: RefCell::new(false),
            max_process_depth: MAX_PROCESS_DEPTH,
            pending_mutations: Rc::new(RefCell::new(Vec::new())),
            external_wake: parking_lot::Mutex::new(None),
            remote_sink: parking_lot::RwLock::new(None),
            node_name: parking_lot::RwLock::new(None),
            remote_sentinel: parking_lot::RwLock::new(None),
            remote_monitor_sink: parking_lot::RwLock::new(None),
            remote_link_sink: parking_lot::RwLock::new(None),
            remote_unlink_sink: parking_lot::RwLock::new(None),
            pending_remote_demonitors: Arc::new(Mutex::new(Vec::new())),
            pending_remote_unlinks: Arc::new(Mutex::new(Vec::new())),
            pending_delegated_restarts: Arc::new(Mutex::new(Vec::new())),
            pending_delegated_restart_outcomes: Arc::new(Mutex::new(Vec::new())),
            unmount_hooks: parking_lot::RwLock::new(Vec::new()),
            remote_group_sink: parking_lot::RwLock::new(None),
            remote_broadcast_sink: parking_lot::RwLock::new(None),
            supervise_decision_hook: parking_lot::RwLock::new(None),
            pending_supervisor_decisions: RefCell::new(Vec::new()),
            telemetry: arc_swap::ArcSwapOption::empty(),
        }
    }

    /// **M14 distribution hook.** Install the dist-crate's remote
    /// routing callback. Subsequent `make_emit` invocations consult
    /// it for `name@host`-shaped targets. Calling again replaces
    /// the previously-installed sink.
    pub fn install_remote_sink(&self, sink: RemoteSink) {
        *self.remote_sink.write() = Some(sink);
    }

    /// **M14 distribution hook.** Set this scheduler's cluster-unique
    /// node name. After this is set, `make_emit` interprets
    /// `target@<this_name>` as local (strips the suffix) and
    /// `target@<other>` as remote (consults `remote_sink`).
    pub fn set_node_name(&self, name: String) {
        *self.node_name.write() = Some(name);
    }

    /// Install the host's durable activation commit boundary. Merely
    /// installing it does not affect ephemeral messages; only messages with
    /// both run and attempt identifiers take the buffered path.
    pub fn install_durable_activation_committer(&mut self, committer: DurableActivationCommitter) {
        self.durable_activation_committer = Some(committer);
    }

    /// Read the cluster-unique node name set via `set_node_name`.
    /// Returns `None` for non-distributed schedulers — `make_emit`
    /// then ignores cross-node targets entirely.
    pub fn node_name(&self) -> Option<String> {
        self.node_name.read().clone()
    }

    /// **M14.5 distribution hook.** Allocate (idempotent) the shared
    /// remote-sentinel `NodeKey` used as the target for every
    /// cross-node monitor and link. The sentinel is a synthetic
    /// `NodeInstance` placed in the slotmap so the
    /// `MonitorRegistry` and `LinkRegistry` have a valid target
    /// key to associate remote refs with; it is NOT registered in
    /// `id_to_key`, never appears in the dirty queue, never
    /// receives messages directly. Per-peer fan-out indexing lives
    /// in the dist crate's `RemoteSupervision`.
    ///
    /// Calling this on a scheduler that already has a sentinel is
    /// a no-op and returns the previously-allocated key.
    pub fn set_remote_sentinel(&mut self) -> NodeKey {
        if let Some(key) = *self.remote_sentinel.read() {
            return key;
        }
        let definition = NodeDefinition::new("__remote_sentinel__", |_ctx| {
            PermissionManifest::new(Vec::<String>::new(), Vec::<String>::new())
        });
        let instance = Rc::new(RefCell::new(NodeInstance::new(
            definition,
            "__remote_sentinel__".to_string(),
            Attrs::new(),
        )));
        let key = self.nodes.insert(instance);
        *self.remote_sentinel.write() = Some(key);
        key
    }

    /// Read the remote sentinel `NodeKey`, if allocated. Returns
    /// `None` on schedulers that haven't run [`Self::set_remote_sentinel`].
    #[must_use]
    pub fn remote_sentinel(&self) -> Option<NodeKey> {
        *self.remote_sentinel.read()
    }

    /// **M14.5 distribution hook.** Install the dist crate's
    /// remote-monitor callback. Subsequent
    /// `use_monitor("name@other")` calls invoke it after the local
    /// stub `MonitorRef` is allocated.
    pub fn install_remote_monitor_sink(&self, sink: RemoteMonitorSink) {
        *self.remote_monitor_sink.write() = Some(sink);
    }

    /// **M14.5 distribution hook.** Install the dist crate's
    /// remote-link callback. Subsequent `use_link("name@other")`
    /// calls invoke it after recording the (observer, sentinel)
    /// link locally.
    pub fn install_remote_link_sink(&self, sink: RemoteLinkSink) {
        *self.remote_link_sink.write() = Some(sink);
    }

    /// **M14.5 distribution hook.** Install the dist crate's
    /// remote-unlink callback. Subsequent
    /// `use_unlink("name@other")` calls invoke it.
    pub fn install_remote_unlink_sink(&self, sink: RemoteUnlinkSink) {
        *self.remote_unlink_sink.write() = Some(sink);
    }

    /// **M14.5 distribution hook.** Append an unmount hook fired
    /// from [`Self::unmount_with_reason`]. Multiple hooks compose;
    /// each fires in registration order with the unmounting
    /// node's id and termination reason. Hooks must NOT borrow
    /// the scheduler — they fire while the scheduler holds
    /// internal `RefCell` borrows during the unmount path.
    pub fn install_unmount_hook(&self, hook: UnmountHook) {
        self.unmount_hooks.write().push(hook);
    }

    /// **M14.10 distribution hook.** Install a partition-aware
    /// decision hook consulted before
    /// [`Self::execute_supervisor_plan`] runs the restart loop for
    /// a failed child. The hook returns
    /// [`SuperviseDecision::Proceed`] (run the plan) or
    /// [`SuperviseDecision::Defer`] (park the decision; the next
    /// `process_batch` re-evaluates it; past the optional
    /// `retry_after` deadline, the plan runs unconditionally).
    ///
    /// The hook MUST NOT borrow the scheduler — it fires while
    /// the scheduler holds internal `RefCell` borrows during the
    /// unmount path.
    ///
    /// Calling again replaces any previously-installed hook (the
    /// dist crate is the canonical caller; tests can install a
    /// stub without leaking).
    pub fn install_supervise_decision_hook(&self, hook: SuperviseDecisionHook) {
        *self.supervise_decision_hook.write() = Some(hook);
    }

    /// **M14.10.** Number of supervisor decisions parked by the
    /// [`SuperviseDecisionHook`] and awaiting re-evaluation /
    /// escalation. Test surface + ops visibility.
    #[must_use]
    pub fn pending_supervisor_decision_count(&self) -> usize {
        self.pending_supervisor_decisions.borrow().len()
    }

    /// **M16-Backend Phase 1.** Install a telemetry sink. Subsequent
    /// `process_batch` calls fan events through it at every hot-path
    /// boundary (tick start/end, queue depths, node evaluate
    /// start/end, message deliver, mount, unmount, topology
    /// dispatch, supervisor decision). Re-installing replaces the
    /// previously-installed sink; pass an
    /// `Arc<crate::telemetry::MultiplexSink>` (defined in
    /// `graphreact-observer`) when more than one consumer needs to
    /// observe the scheduler.
    ///
    /// The sink MUST NOT borrow the scheduler — same constraint
    /// already documented for [`SuperviseDecisionHook`]. See
    /// [`crate::telemetry`] for the full discipline.
    pub fn install_telemetry_sink(&self, sink: Arc<dyn crate::telemetry::TelemetrySink>) {
        self.telemetry
            .store(Some(Arc::new(crate::telemetry::TelemetrySinkRef::new(
                sink,
            ))));
    }

    /// Clear any installed telemetry sink. After this returns, the
    /// scheduler hot path collapses to the same `Option::is_some`
    /// branch that uninstalled schedulers see. Test surface;
    /// production teardown can also use this during graceful
    /// shutdown.
    pub fn clear_telemetry_sink(&self) {
        self.telemetry.store(None);
    }

    /// **M14.6 distribution hook.** Install the dist crate's
    /// group-change callback. Subsequent local
    /// `use_group_join` / `use_group_leave` invocations fire
    /// the sink after the local registry mutation; unmounts
    /// fan a `Leave` per group the unmounting node was in.
    ///
    /// **M14.8.x retrofit**: synthesize a `Join` for every
    /// membership ALREADY in the local `GroupRegistry`. This
    /// closes the engine-hosted-daemon ordering gap: when
    /// `mount_with_scheduler` (or `mount_python_definitions`)
    /// runs the first `process_batch` BEFORE
    /// `Cluster::start` installs its sinks, the bodies'
    /// `use_group_join` calls register locally but the dist
    /// crate doesn't see them — `Cluster::start`'s subsequent
    /// `install_remote_group_sink` call sweeps the registry
    /// and ships a `Join` event per pre-existing member so
    /// `DistGroups` populates its state and PgSync replies
    /// include them.
    pub fn install_remote_group_sink(&self, sink: RemoteGroupSink) {
        *self.remote_group_sink.write() = Some(Arc::clone(&sink));
        // Retroactive Join fan-out. Snapshot the registry into
        // (group_name, node_id) pairs first so we don't hold
        // the borrow across the user-supplied closure.
        let pairs: Vec<(String, String)> = {
            let groups = self.groups.borrow();
            let mut out = Vec::new();
            for group_name in groups.group_names() {
                for member_key in groups.members(&group_name) {
                    if let Some(node_rc) = self.nodes.get(member_key) {
                        let node_id = node_rc.borrow().id.clone();
                        out.push((group_name.clone(), node_id));
                    }
                }
            }
            out
        };
        for (group_name, node_id) in pairs {
            sink(GroupOp::Join, &group_name, &node_id);
        }
    }

    /// **M14.6 distribution hook.** Install the dist crate's
    /// broadcast callback. [`Self::group_broadcast`] fires it
    /// after local-member fan-out so the dist crate ships one
    /// `PgBroadcast` packet per remote peer holding members.
    pub fn install_remote_broadcast_sink(&self, sink: RemoteBroadcastSink) {
        *self.remote_broadcast_sink.write() = Some(sink);
    }

    /// Replace the component function of an already-registered node.
    /// Hook state and identity are preserved — the node is marked dirty
    /// and re-evaluated on the next batch with the new behavior.
    ///
    /// Used by the host's hot-reload path. Errors if the ID isn't
    /// registered.
    /// **M15 hot-reload substrate.** Read the current
    /// [`NodeDefinition`] for a node id. Returns a clone (the
    /// definition's `component` is `Rc<dyn Fn(...)>` so cloning is
    /// cheap). Used by hot-reload's rollback path to snapshot the
    /// pre-swap definition before [`Self::replace_definition`]
    /// fires; on first-tick failure, the snapshot restores the
    /// node to its working body.
    pub fn get_definition(&self, id: &str) -> Option<NodeDefinition> {
        let key = self.id_to_key.get(id).copied()?;
        let node = self.nodes.get(key)?;
        Some(node.borrow().definition.clone())
    }

    pub fn replace_definition(
        &mut self,
        id: &str,
        new_def: NodeDefinition,
    ) -> Result<(), RuntimeError> {
        let Some(&key) = self.id_to_key.get(id) else {
            return Err(RuntimeError::NodeNotRegistered { id: id.to_string() });
        };
        if let Some(node_rc) = self.nodes.get(key) {
            node_rc.borrow_mut().definition = new_def;
        }
        self.mark_dirty(key);
        Ok(())
    }

    /// Override the per-instance cap on cascade rounds. Useful for
    /// recursive type trees or deep dependency DAGs that legitimately
    /// produce more than the default 100 rounds before quiescing.
    pub fn set_max_process_depth(&mut self, max: usize) {
        self.max_process_depth = max;
    }

    /// Current cap on cascade rounds (default [`MAX_PROCESS_DEPTH`]).
    #[must_use]
    pub fn max_process_depth(&self) -> usize {
        self.max_process_depth
    }

    /// Register a node with the scheduler. The graph must already contain
    /// the node ID. Adds the node to the dirty queue so it evaluates on
    /// the next [`process_batch`].
    pub fn register_node(
        &mut self,
        definition: NodeDefinition,
        id: impl Into<String>,
        props: Attrs,
        height: Height,
    ) -> Result<NodeKey, RuntimeError> {
        let id = id.into();
        if !self.graph.borrow().has_node(&id) {
            return Err(RuntimeError::NodeNotRegistered { id });
        }
        let role = definition.name.clone();
        let instance = Rc::new(RefCell::new(NodeInstance::new(
            definition,
            id.clone(),
            props,
        )));
        let key = self.nodes.insert(instance);
        self.id_to_key.insert(id.clone(), key);
        self.heights.insert(key, height);
        self.directory.borrow_mut().add_entry(DirectoryEntry {
            id: id.clone(),
            role,
            depth: height,
            path: vec![id],
        });
        self.dirty.borrow_mut().insert((height, key));
        Ok(key)
    }

    /// Lookup a node's key by string ID.
    #[must_use]
    pub fn key_for(&self, id: &str) -> Option<NodeKey> {
        self.id_to_key.get(id).copied()
    }

    /// Snapshot every registered node's permission manifest, keyed by node
    /// ID. Used by validation tooling (`graphreact-algorithms::validation`)
    /// and any host that needs to audit the message-type contract after
    /// `start()` has populated manifests.
    ///
    /// Manifests are cloned — callers can mutate the returned map without
    /// affecting scheduler state.
    pub fn snapshot_manifests(&self) -> std::collections::BTreeMap<String, PermissionManifest> {
        let mut map = std::collections::BTreeMap::new();
        for (_key, node_rc) in &self.nodes {
            let node = node_rc.borrow();
            if let Some(manifest) = &node.manifest {
                map.insert(node.id.clone(), manifest.clone());
            }
        }
        map
    }

    /// Capture the JSON-compatible values owned by explicitly durable hooks.
    /// Ephemeral positional hook state and executable callbacks never enter
    /// this recovery payload.
    #[must_use]
    pub fn snapshot_durable_state(&self) -> DurableStateSnapshot {
        let nodes = self
            .id_to_key
            .iter()
            .filter_map(|(id, key)| {
                self.nodes
                    .get(*key)
                    .map(|node| (id.clone(), node.borrow().durable_state_snapshot()))
            })
            .filter(|(_, values)| !values.is_empty())
            .collect();
        DurableStateSnapshot { nodes }
    }

    /// Restore named durable values after node definitions are rebuilt.
    /// This may be called before initial evaluation; values are staged until
    /// the matching hook key is installed by the component body.
    pub fn restore_durable_state(
        &self,
        snapshot: DurableStateSnapshot,
    ) -> Result<(), RuntimeError> {
        for (node_id, values) in snapshot.nodes {
            let key = self.id_to_key.get(&node_id).copied().ok_or_else(|| {
                RuntimeError::NodeNotRegistered {
                    id: node_id.clone(),
                }
            })?;
            let node = self
                .nodes
                .get(key)
                .ok_or_else(|| RuntimeError::NodeNotRegistered {
                    id: node_id.clone(),
                })?;
            node.borrow_mut().restore_durable_state(values);
        }
        Ok(())
    }

    /// Whether a node is registered with the scheduler.
    #[must_use]
    pub fn has_node(&self, id: &str) -> bool {
        self.id_to_key.contains_key(id)
    }

    /// Register a node and connect it to a list of existing peers — the
    /// runtime equivalent of `addNodeAtRuntime` in the TypeScript scheduler.
    ///
    /// The node is added to the underlying [`CommunicationGraph`] (firing
    /// the appropriate topology events), connected via undirected edges to
    /// each `connect_to` neighbor, registered with the scheduler, and
    /// evaluated immediately in a fresh batch. Topology events fire via
    /// the graph's listener registered by [`register_graph_listener`].
    pub fn add_node_at_runtime(
        &mut self,
        definition: NodeDefinition,
        id: impl Into<String>,
        props: Attrs,
        connect_to: Vec<String>,
    ) -> Result<NodeKey, RuntimeError> {
        let id = id.into();
        // Add to graph first — fires NodeAdded.
        self.graph.borrow_mut().add_node(id.clone(), Attrs::new());
        // Register with scheduler so topology dispatchers see it.
        let key = self.register_node(definition, id.clone(), props, 0)?;
        // Connect edges — fires EdgeAdded for each.
        for peer in connect_to {
            let mut g = self.graph.borrow_mut();
            if g.has_node(&peer) {
                g.add_edge(peer, id.clone(), None, Attrs::new());
            }
        }
        // Mark all neighbors dirty too — they may want to re-evaluate
        // because their `useNeighbors` snapshot changed.
        let neighbors = self.graph.borrow().neighbors(&id).unwrap_or_default();
        for nbr_id in neighbors {
            if let Some(&nbr_key) = self.id_to_key.get(&nbr_id) {
                self.mark_dirty(nbr_key);
            }
        }
        // Evaluate the new node immediately so its mount effect runs in
        // the same call (matches TS `addNodeAtRuntime` semantics).
        self.process_batch()?;
        // M16-Backend Phase 1: fire `on_node_mounted` AFTER the
        // initial-mount cascade has drained so consumers observing
        // the mount see a node whose first render has already
        // happened (matches BEAM's `process_flag(:trap_exit, true) ->
        // spawned` ordering — observers see live processes, not
        // pre-init ones).
        if let Some(sink) = self.telemetry.load().as_ref() {
            sink.on_node_mounted(&crate::telemetry::NodeMounted { node_id: &id });
        }
        Ok(key)
    }

    /// Remove a node and its edges from the runtime. No-op if not
    /// registered. Mirror of `removeNodeAtRuntime` in TypeScript.
    pub fn remove_node_at_runtime(&mut self, id: &str) -> Result<(), RuntimeError> {
        let Some(key) = self.id_to_key.get(id).copied() else {
            return Ok(());
        };
        // Mark neighbors dirty so they observe the mutation.
        let neighbors = self.graph.borrow().neighbors(id).unwrap_or_default();
        for nbr_id in neighbors {
            if let Some(&nbr_key) = self.id_to_key.get(&nbr_id) {
                self.mark_dirty(nbr_key);
            }
        }
        // Unmount first so cleanups run while the node is still in scope.
        self.unmount(key);
        // Remove from graph — fires EdgeRemoved + NodeRemoved.
        let _ = self.graph.borrow_mut().remove_node(id);
        // Drain anything that bubbled up as a result.
        self.process_batch()?;
        Ok(())
    }

    /// Run the initial mount cycle. Convenience for tests / hosts that
    /// just want to drain the dirty queue once.
    pub fn start(&mut self) -> Result<(), RuntimeError> {
        self.process_batch()
    }

    /// Run the daemon loop until [`stop_daemon`] is called.
    ///
    /// Awaits the [`Notify`] handle whenever the queues are empty, so
    /// the scheduler stays idle (no busy spin) between bursts of work.
    /// `&mut self` is required because `process_batch` mutates state;
    /// callers must run this on a `tokio::task::LocalSet` since the
    /// scheduler is `!Send`.
    ///
    /// [`Notify`]: tokio::sync::Notify
    /// [`stop_daemon`]: Self::stop_daemon
    pub async fn run_daemon(&mut self) -> Result<(), RuntimeError> {
        self.stop_flag
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // Drain whatever is already pending before parking.
        self.process_batch()?;
        loop {
            if self.stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            self.notify.notified().await;
            if self.stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            self.process_batch()?;
        }
        Ok(())
    }

    /// Signal the daemon loop to exit on its next wake-up. Idempotent.
    /// Does NOT run cleanups — call [`stop`] for that.
    ///
    /// [`stop`]: Self::stop
    pub fn stop_daemon(&self) {
        self.stop_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.notify.notify_one();
    }

    /// Inject a message into the runtime from outside.
    pub fn send(&self, target_id: impl Into<String>, r#type: impl Into<String>, payload: Value) {
        let msg = create_message(r#type, "__external__", target_id, payload, None);
        self.messages.borrow_mut().push_back(msg);
        self.notify.notify_one();
    }

    /// Enqueue a fully formed local message without changing its durable
    /// identity or timestamp. Used by recovery after storage replay.
    pub fn enqueue_recovered_message(&self, message: Message) {
        self.messages.borrow_mut().push_back(message);
        self.notify.notify_one();
    }

    /// Restore messages parked at a committed snapshot boundary.
    pub fn restore_postponed_messages(
        &self,
        messages: impl IntoIterator<Item = Message>,
    ) -> Result<(), RuntimeError> {
        for message in messages {
            let key = self.id_to_key.get(&message.to).copied().ok_or_else(|| {
                RuntimeError::NodeNotRegistered {
                    id: message.to.clone(),
                }
            })?;
            self.postponed
                .borrow_mut()
                .entry(key)
                .or_default()
                .push(message);
        }
        Ok(())
    }

    /// Inject a message belonging to a durable run. Downstream local emits
    /// inherit the same run and attempt identifiers.
    pub fn send_durable(
        &self,
        target_id: impl Into<String>,
        r#type: impl Into<String>,
        payload: Value,
        run_id: impl Into<String>,
        attempt_id: impl Into<String>,
    ) {
        let mut msg = create_message(r#type, "__external__", target_id, payload, None);
        msg.id = uuid::Uuid::now_v7().to_string();
        msg.run_id = Some(run_id.into());
        msg.attempt_id = Some(attempt_id.into());
        self.messages.borrow_mut().push_back(msg);
        self.notify.notify_one();
    }

    /// Mark a node dirty for re-evaluation in the next batch.
    pub fn mark_dirty(&self, key: NodeKey) {
        let height = self.heights.get(&key).copied().unwrap_or(0);
        self.dirty.borrow_mut().insert((height, key));
        self.notify.notify_one();
    }

    /// Clone the daemon-wakeup handle. Useful for callers that want to
    /// poke the scheduler from another thread (e.g. a tokio task that
    /// just pushed a message via `send`).
    #[must_use]
    pub fn notify_handle(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }

    /// `Send + Sync` handle for cross-thread message injection and
    /// daemon control. Use this when another task / thread / source
    /// needs to inject work without holding a borrow on the scheduler
    /// itself. Internal `send` (from this thread) is still fine via
    /// `Scheduler::send`.
    #[must_use]
    pub fn handle(&self) -> SchedulerHandle {
        SchedulerHandle {
            inbox: Arc::clone(&self.external_inbox),
            notify: Arc::clone(&self.notify),
            stop_flag: Arc::clone(&self.stop_flag),
            pending_remote_demonitors: Arc::clone(&self.pending_remote_demonitors),
            pending_remote_unlinks: Arc::clone(&self.pending_remote_unlinks),
            pending_delegated_restarts: Arc::clone(&self.pending_delegated_restarts),
            pending_delegated_restart_outcomes: Arc::clone(
                &self.pending_delegated_restart_outcomes,
            ),
        }
    }

    /// **M16-Backend Phase 1.** Snapshot the current depth of every
    /// queue the scheduler maintains. Cheap (just borrows + len()
    /// reads); fired once per `process_batch` cascade round, NOT per
    /// message, so steady-state load shows up as ~1 sample/ms.
    fn queue_depths_snapshot(&self) -> crate::telemetry::QueueDepthsSnapshot {
        crate::telemetry::QueueDepthsSnapshot {
            messages: self.messages.borrow().len(),
            dirty: self.dirty.borrow().len(),
            pending_topology: self.pending_topology.lock().len(),
            external_inbox: self.external_inbox.lock().len(),
            pending_mutations: self.pending_mutations.borrow().len(),
        }
    }

    /// Install a cross-thread wake-up callback. Invoked from any thread
    /// after a source pushes into the external inbox; hosts use it to
    /// post a `process_batch` request onto whatever thread drives
    /// evaluation. The napi bridge installs a threadsafe function that
    /// calls `processBatchAsync` on the V8 main thread.
    ///
    /// Pass `None` to clear (e.g., during tear-down).
    pub fn set_external_wake(&self, wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        *self.external_wake.lock() = wake;
    }

    /// Drain message, dirty, and topology event queues until quiescent or
    /// [`MAX_PROCESS_DEPTH`] iterations elapse.
    pub fn process_batch(&mut self) -> Result<(), RuntimeError> {
        if *self.running.borrow() {
            return Ok(());
        }
        *self.running.borrow_mut() = true;
        // M16-Backend Phase 1: tick_start / tick_end + per-round
        // queue_depths sample. Hold an `arc_swap::Guard` across the
        // entire batch so per-callsite cost is just an `Option`
        // null-check — no Arc clone, no atomic refcount per event.
        let telemetry_guard = self.telemetry.load();
        let telemetry: Option<&Arc<crate::telemetry::TelemetrySinkRef>> = telemetry_guard.as_ref();
        if let Some(sink) = telemetry {
            sink.on_scheduler_tick_start(&crate::telemetry::SchedulerTickStart::default());
        }
        let mut depth = 0u32;
        loop {
            depth = depth.saturating_add(1);
            if depth as usize > self.max_process_depth {
                let max = self.max_process_depth;
                *self.running.borrow_mut() = false;
                if let Some(sink) = telemetry {
                    sink.on_scheduler_tick_end(&crate::telemetry::SchedulerTickEnd {
                        iterations: depth.saturating_sub(1),
                        terminated_by_depth_cap: true,
                    });
                }
                return Err(RuntimeError::MaxDepthExceeded { max });
            }

            // M16-Backend Phase 1: per-cascade-round queue depths
            // sample. Fired ONCE per round (not per message); steady
            // ~1 sample/ms cadence under load.
            if let Some(sink) = telemetry {
                let snapshot = self.queue_depths_snapshot();
                sink.on_queue_depths(&crate::telemetry::QueueDepths { depth, snapshot });
            }

            // Apply any deferred ctx-mutations (spawnChild / unmount /
            // compose) queued during the previous round's evaluations or
            // delivery. They populate the dirty queue and topology
            // events that the rest of this iteration consumes.
            self.drain_pending_mutations();

            // M14.5: drain remote-supervision cleanup requests pushed
            // from tokio worker threads (peer disconnect, inbound
            // Demonitor packet). Runs before message draining so the
            // synthesized `__DOWN__` messages already in the
            // external inbox land on a registry that no longer holds
            // their stale refs.
            self.drain_pending_remote_supervision();

            // Phase γ (M14.11): drain queued cross-node delegated-
            // restart requests pushed by the dist crate's
            // `dispatch_inbound`. Synthesizes `ExitReason::Crashed`
            // and drives the existing `unmount_with_reason` →
            // `execute_supervisor_plan` path, preserving the
            // `RestartPolicy` / `should_restart` contract.
            self.drain_pending_delegated_restarts();

            // M14.10: re-evaluate supervisor decisions parked by
            // the partition-aware decision hook. Either runs the
            // plan (hook now says Proceed, or retry_after elapsed)
            // or leaves the decision parked for next round.
            self.drain_pending_supervisor_decisions();

            // Drain external (cross-thread) inbox into the local message
            // queue so daemon-mode pushers see their messages routed
            // through the same delivery path as in-process emits.
            let external: Vec<Message> = {
                let mut buf = self.external_inbox.lock();
                std::mem::take(&mut *buf)
            };
            if !external.is_empty() {
                let mut q = self.messages.borrow_mut();
                for msg in external {
                    q.push_back(msg);
                }
            }

            // Dispatch any topology events accumulated since the previous
            // iteration. Run *first* so handlers see structural changes
            // before the subsequent re-evaluation passes observe their
            // effects (neighbor lists, etc.).
            let topology_events: Vec<TopologyEvent> = {
                let mut q = self.pending_topology.lock();
                std::mem::take(&mut *q)
            };
            for event in &topology_events {
                self.dispatch_topology_event(event);
            }

            // Drain dirty queue.
            let to_evaluate: Vec<NodeKey> = {
                let mut dirty = self.dirty.borrow_mut();
                let keys: Vec<NodeKey> = dirty.iter().map(|(_, k)| *k).collect();
                dirty.clear();
                keys
            };
            for key in to_evaluate {
                self.evaluate(key);
            }

            // Drain message queue.
            let next_batch: Vec<Message> = {
                let mut q = self.messages.borrow_mut();
                std::mem::take(&mut *q).into_iter().collect()
            };

            if next_batch.is_empty()
                && self.dirty.borrow().is_empty()
                && self.pending_topology.lock().is_empty()
                && self.external_inbox.lock().is_empty()
                && self.pending_mutations.borrow().is_empty()
                && self.pending_remote_demonitors.lock().is_empty()
                && self.pending_remote_unlinks.lock().is_empty()
                && self.pending_delegated_restarts.lock().is_empty()
            {
                break;
            }
            for msg in next_batch {
                if let Err(error) = self.deliver(msg) {
                    *self.running.borrow_mut() = false;
                    return Err(error);
                }
            }
        }
        *self.running.borrow_mut() = false;
        if let Some(sink) = telemetry {
            sink.on_scheduler_tick_end(&crate::telemetry::SchedulerTickEnd {
                iterations: depth,
                terminated_by_depth_cap: false,
            });
        }
        Ok(())
    }

    /// Async sibling of [`process_batch`]. Mirrors the TS Phase 6 design:
    /// async is opt-in. Differences from the sync path:
    ///
    /// * Independent dirty nodes (no shared edges) evaluate in parallel
    ///   via [`futures::future::join_all`]. Same-group nodes run sequentially.
    /// * `evaluate_node_async` awaits `flush_effects_async`, which runs
    ///   both sync and async effect callbacks.
    ///
    /// Message delivery still uses the sync path — async message handlers
    /// are deferred to a follow-up. This still gives concurrent I/O for the
    /// common case (LLM/HTTP/DB calls live in `useEffect`/`useEffectAsync`,
    /// not in message handlers).
    ///
    /// Must be driven from a `LocalSet` (or other single-threaded executor)
    /// because the scheduler is `!Send`.
    pub async fn process_batch_async(&mut self) -> Result<(), RuntimeError> {
        if *self.running.borrow() {
            return Ok(());
        }
        *self.running.borrow_mut() = true;
        let telemetry_guard = self.telemetry.load();
        let telemetry: Option<&Arc<crate::telemetry::TelemetrySinkRef>> = telemetry_guard.as_ref();
        if let Some(sink) = telemetry {
            sink.on_scheduler_tick_start(&crate::telemetry::SchedulerTickStart::default());
        }
        let mut depth = 0u32;
        loop {
            depth = depth.saturating_add(1);
            if depth as usize > self.max_process_depth {
                let max = self.max_process_depth;
                *self.running.borrow_mut() = false;
                if let Some(sink) = telemetry {
                    sink.on_scheduler_tick_end(&crate::telemetry::SchedulerTickEnd {
                        iterations: depth.saturating_sub(1),
                        terminated_by_depth_cap: true,
                    });
                }
                return Err(RuntimeError::MaxDepthExceeded { max });
            }

            if let Some(sink) = telemetry {
                let snapshot = self.queue_depths_snapshot();
                sink.on_queue_depths(&crate::telemetry::QueueDepths { depth, snapshot });
            }

            // Apply any deferred ctx-mutations from the previous round.
            self.drain_pending_mutations();

            // M14.5: same remote-supervision cleanup drain as the sync
            // path — tokio workers push here when peers disconnect.
            self.drain_pending_remote_supervision();

            // Phase γ (M14.11): drain queued cross-node delegated-
            // restart requests pushed by the dist crate's
            // `dispatch_inbound`. Synthesizes `ExitReason::Crashed`
            // and drives the existing `unmount_with_reason` →
            // `execute_supervisor_plan` path, preserving the
            // `RestartPolicy` / `should_restart` contract.
            self.drain_pending_delegated_restarts();

            // M14.10: re-evaluate parked supervisor decisions —
            // mirrored from the sync path.
            self.drain_pending_supervisor_decisions();

            // Drain external (cross-thread) inbox.
            let external: Vec<Message> = {
                let mut buf = self.external_inbox.lock();
                std::mem::take(&mut *buf)
            };
            if !external.is_empty() {
                let mut q = self.messages.borrow_mut();
                for msg in external {
                    q.push_back(msg);
                }
            }

            // Topology events first.
            let topology_events: Vec<TopologyEvent> = {
                let mut q = self.pending_topology.lock();
                std::mem::take(&mut *q)
            };
            for event in &topology_events {
                self.dispatch_topology_event(event);
            }

            // Drain dirty queue, then group by independence.
            let to_evaluate: Vec<NodeKey> = {
                let mut dirty = self.dirty.borrow_mut();
                let keys: Vec<NodeKey> = dirty.iter().map(|(_, k)| *k).collect();
                dirty.clear();
                keys
            };
            if !to_evaluate.is_empty() {
                let groups = self.group_independent_keys(&to_evaluate);
                let scheduler: &Self = self;
                let group_futures = groups.into_iter().map(|group| async move {
                    for key in group {
                        scheduler.evaluate_async(key).await;
                    }
                });
                futures::future::join_all(group_futures).await;
            }

            // Deliver messages (sync handlers — see note above).
            let next_batch: Vec<Message> = {
                let mut q = self.messages.borrow_mut();
                std::mem::take(&mut *q).into_iter().collect()
            };

            if next_batch.is_empty()
                && self.dirty.borrow().is_empty()
                && self.pending_topology.lock().is_empty()
                && self.external_inbox.lock().is_empty()
                && self.pending_mutations.borrow().is_empty()
                && self.pending_remote_demonitors.lock().is_empty()
                && self.pending_remote_unlinks.lock().is_empty()
                && self.pending_delegated_restarts.lock().is_empty()
            {
                break;
            }
            for msg in next_batch {
                if let Err(error) = self.deliver(msg) {
                    *self.running.borrow_mut() = false;
                    return Err(error);
                }
            }
        }
        *self.running.borrow_mut() = false;
        if let Some(sink) = telemetry {
            sink.on_scheduler_tick_end(&crate::telemetry::SchedulerTickEnd {
                iterations: depth,
                terminated_by_depth_cap: false,
            });
        }
        Ok(())
    }

    /// Async sibling of [`Self::start`]. Mounts every registered node via
    /// [`evaluate_node_async`] (so async effects fire on initial mount),
    /// then drives the async batch loop until quiescent.
    ///
    /// **Initial-mount parallelism:** unlike [`Self::process_batch_async`] —
    /// which uses union-find on shared edges to keep nodes that might write
    /// to each other's state in the same group — `start_async` puts every
    /// node in its own group. At initial mount no inter-node messaging has
    /// happened yet, so the conservative grouping is unnecessary; siblings
    /// in fan-in trees (the typical project-DAG / agent-tree shape) get
    /// proper parallelism from the first round instead of serializing
    /// behind their shared parent. After mount, `process_batch_async`'s
    /// edge-aware grouping resumes for cascade rounds.
    pub async fn start_async(&mut self) -> Result<(), RuntimeError> {
        let keys: Vec<NodeKey> = self.nodes.keys().collect();
        if !keys.is_empty() {
            let scheduler: &Self = self;
            let mount_futures = keys.into_iter().map(|key| async move {
                scheduler.evaluate_async(key).await;
            });
            futures::future::join_all(mount_futures).await;
        }
        self.process_batch_async().await
    }

    /// Async sibling of [`Self::send`]. Injects a host-side message and
    /// drives the async batch loop until quiescent.
    pub async fn send_async(
        &mut self,
        target: &str,
        r#type: &str,
        payload: Value,
    ) -> Result<(), RuntimeError> {
        self.send(target, r#type, payload);
        self.process_batch_async().await
    }

    /// Async stop — awaits async cleanups during unmount.
    pub async fn stop_async(&mut self) {
        let keys: Vec<NodeKey> = self.nodes.keys().collect();
        for key in keys {
            self.unmount_async(key).await;
        }
        self.messages.borrow_mut().clear();
        self.dirty.borrow_mut().clear();
        self.pending_topology.lock().clear();
    }

    /// Async unmount — awaits async effect cleanups before tearing down.
    /// Equivalent to `unmount(ExitReason::Normal)` plus async cleanup
    /// awaits. Skips supervisor signal propagation: graceful shutdown via
    /// [`Self::stop_async`] is the intended caller; abnormal exits go
    /// through the sync [`Self::unmount_with_reason`] path so supervisor
    /// restart logic stays synchronous and predictable.
    pub async fn unmount_async(&mut self, key: NodeKey) {
        if let Some(node_rc) = self.nodes.get(key).cloned() {
            unmount_node_async(&node_rc).await;
        }

        let id_opt = self.nodes.remove(key).map(|n| n.borrow().id.clone());
        self.heights.remove(&key);
        if let Some(id) = id_opt {
            self.id_to_key.remove(&id);
            let _ = self.graph.borrow_mut().remove_node(&id);
            self.directory.borrow_mut().remove_entry(&id);
            // M12.3a-followon-E: drop the manifest entry so Tier 2/3
            // queries don't see stale policy for unmounted nodes.
            self.manifests.borrow_mut().remove(&id);
            // M12.3a-followon-F: drop the predicate entry on async unmount.
            self.edge_accept_predicates.borrow_mut().remove(&id);
        }
        self.postponed.borrow_mut().remove(&key);
        self.last_received_ttl.borrow_mut().remove(&key);
        self.node_to_supervisor.remove(&key);
    }

    /// Group dirty `NodeKey`s into independent sets via union-find on the
    /// shared-edge relation. Mirror of TS `groupIndependentNodes` (Phase 6).
    /// Independent groups can evaluate in parallel; same-group keys must
    /// run sequentially (their state may depend on each other's emits).
    fn group_independent_keys(&self, keys: &[NodeKey]) -> Vec<Vec<NodeKey>> {
        if keys.len() <= 1 {
            return keys.iter().map(|k| vec![*k]).collect();
        }
        let dirty_set: std::collections::HashSet<NodeKey> = keys.iter().copied().collect();
        let mut parent: FxHashMap<NodeKey, NodeKey> = keys.iter().map(|k| (*k, *k)).collect();

        for &k in keys {
            let id = self.nodes.get(k).map(|n| n.borrow().id.clone());
            let Some(id) = id else { continue };
            let neighbors = self.graph.borrow().neighbors(&id).unwrap_or_default();
            for nbr in neighbors {
                if let Some(&nbr_key) = self.id_to_key.get(&nbr) {
                    if dirty_set.contains(&nbr_key) {
                        union_find_union(&mut parent, k, nbr_key);
                    }
                }
            }
        }

        let mut groups: FxHashMap<NodeKey, Vec<NodeKey>> = FxHashMap::default();
        for &k in keys {
            let root = union_find_find(&mut parent, k);
            groups.entry(root).or_default().push(k);
        }
        groups.into_values().collect()
    }

    async fn evaluate_async(&self, key: NodeKey) {
        let Some(node_rc) = self.nodes.get(key) else {
            return;
        };
        if node_rc.borrow().is_unmounted() {
            return;
        }

        let node_id = node_rc.borrow().id.clone();
        let neighbors = self.graph.borrow().neighbors(&node_id).unwrap_or_default();
        let (depth, path) = self
            .directory
            .borrow()
            .find_by_id(&node_id)
            .map(|e| (e.depth, e.path.clone()))
            .unwrap_or((0, vec![node_id.clone()]));
        let props = node_rc.borrow().props.clone();
        let children = node_rc.borrow().external_children.clone();

        let ctx = NodeContext {
            id: node_id.clone(),
            props,
            children,
            my_depth: depth,
            my_path: path,
            neighbors: neighbors.clone(),
        };

        let hook_ctx = self.build_hook_ctx(key, node_id.clone(), neighbors);

        // M16-Backend Phase 1: same per-node evaluate start/end pair
        // as the sync path. The async path can't hold an
        // `arc_swap::Guard` across an `.await` (it isn't `Send` in
        // current arc-swap; nor is it desirable to pin a hazard
        // pointer across an await point), so we use `load_full()` to
        // snapshot once with a single Arc clone, paying that cost
        // once per `evaluate_async` rather than per-event.
        let telemetry_snapshot: Option<Arc<crate::telemetry::TelemetrySinkRef>> =
            self.telemetry.load_full();
        if let Some(sink) = &telemetry_snapshot {
            sink.on_node_evaluate_start(&crate::telemetry::NodeEvaluateStart { node_id: &node_id });
        }

        evaluate_node_async(node_rc, ctx, hook_ctx).await;

        if let Some(sink) = &telemetry_snapshot {
            sink.on_node_evaluate_end(&crate::telemetry::NodeEvaluateEnd { node_id: &node_id });
        }

        // M12.3a-followon-E: sync manifest into the runtime-shared
        // index after async eval too — same contract as the sync path.
        if let Some(m) = node_rc.borrow().manifest.clone() {
            self.manifests.borrow_mut().insert(node_id.clone(), m);
        }
        // M12.3a-followon-F: same predicate sync contract as the sync
        // evaluate path.
        match node_rc.borrow().edge_accept_predicate.clone() {
            Some(p) => {
                self.edge_accept_predicates
                    .borrow_mut()
                    .insert(node_id.clone(), p);
            }
            None => {
                self.edge_accept_predicates.borrow_mut().remove(&node_id);
            }
        }

        // Postpone replay (same as sync path).
        let drained = self
            .postponed
            .borrow_mut()
            .get_mut(&key)
            .map(PostponeQueue::drain)
            .unwrap_or_default();
        if !drained.is_empty() {
            let mut q = self.messages.borrow_mut();
            for msg in drained.into_iter().rev() {
                q.push_front(msg);
            }
        }
    }

    /// Stop the runtime — unmount every node, run effect cleanups, drop
    /// pending messages. Mirror of TS `scheduler.stop()`.
    pub fn stop(&mut self) {
        let keys: Vec<NodeKey> = self.nodes.keys().collect();
        for key in keys {
            self.unmount(key);
        }
        self.messages.borrow_mut().clear();
        self.dirty.borrow_mut().clear();
        self.pending_topology.lock().clear();
    }

    /// Unmount a node, run cleanups, deliver `__DOWN__`/`__EXIT__` system
    /// messages to monitors and linked peers, and remove from the
    /// scheduler. Default exit reason is [`ExitReason::Normal`]; use
    /// [`unmount_with_reason`] to signal an abnormal termination so
    /// supervisors and `trap_exit`-style handlers can react.
    ///
    /// [`unmount_with_reason`]: Self::unmount_with_reason
    pub fn unmount(&mut self, key: NodeKey) {
        self.unmount_with_reason(key, ExitReason::Normal);
    }

    /// Unmount with an explicit reason — deliver DOWN/EXIT signals
    /// reflecting it. Linked peers receive EXIT regardless; abnormal
    /// reasons will eventually feed supervisor restart logic (M5 stage 3).
    pub fn unmount_with_reason(&mut self, key: NodeKey, reason: ExitReason) {
        let id_opt = self.nodes.get(key).map(|n| n.borrow().id.clone());

        // Drain monitors and links *before* tearing down so handlers fire
        // while observers are still alive.
        let monitor_targets = self.monitors.borrow_mut().drain_target(key);
        let linked_peers = self.links.borrow_mut().drain_node(key);
        // Observers/linked peers' own monitors of others still stand —
        // only this node's outgoing-monitor entries are cleared.
        self.monitors.borrow_mut().drain_observer(key);

        // M14.6: snapshot group memberships BEFORE `leave_all`
        // clears them so the dist crate can fan a `Leave` per
        // group. Sink fires AFTER the local registry mutation
        // for parity with `make_group_leave`'s shape.
        let groups_left = self.groups.borrow().groups_for_node(key);
        self.groups.borrow_mut().leave_all(key);
        if !groups_left.is_empty() {
            if let Some(sink) = self.remote_group_sink.read().clone() {
                if let Some(id) = id_opt.as_deref() {
                    for group in &groups_left {
                        sink(GroupOp::Leave, group, id);
                    }
                }
            }
        }

        if let Some(id) = id_opt.as_deref() {
            for (mref, observer_key) in monitor_targets {
                if let Some(observer_rc) = self.nodes.get(observer_key) {
                    let observer_id = observer_rc.borrow().id.clone();
                    let payload = serde_json::json!({
                        "ref": mref.0,
                        "target": id,
                        "reason": reason.as_str(),
                    });
                    self.messages.borrow_mut().push_back(create_message(
                        SYSTEM_DOWN,
                        "__system__",
                        observer_id,
                        payload,
                        None,
                    ));
                }
            }
            for peer_key in linked_peers {
                if let Some(peer_rc) = self.nodes.get(peer_key) {
                    let peer_id = peer_rc.borrow().id.clone();
                    let payload = serde_json::json!({
                        "from": id,
                        "reason": reason.as_str(),
                    });
                    self.messages.borrow_mut().push_back(create_message(
                        SYSTEM_EXIT,
                        "__system__",
                        peer_id,
                        payload,
                        None,
                    ));
                }
            }
        }

        if let Some(node_rc) = self.nodes.get(key) {
            unmount_node(node_rc);
        }
        let removed_id = if let Some(node_rc) = self.nodes.remove(key) {
            let id = node_rc.borrow().id.clone();
            self.id_to_key.remove(&id);
            self.directory.borrow_mut().remove_entry(&id);
            self.heights.remove(&key);
            // M12.3a-followon-E: same manifest-prune contract as
            // `unmount_async`.
            self.manifests.borrow_mut().remove(&id);
            // M12.3a-followon-F: same predicate-prune contract as
            // `unmount_async`.
            self.edge_accept_predicates.borrow_mut().remove(&id);
            Some(id)
        } else {
            None
        };
        self.postponed.borrow_mut().remove(&key);
        self.last_received_ttl.borrow_mut().remove(&key);
        let governing_supervisor = self.node_to_supervisor.remove(&key);

        // M14.5: fire installed unmount hooks. The dist crate uses
        // these to ship wire `Down` packets for inbound monitors
        // and to prune outbound translation tables. Cloned snapshot
        // so hooks can't deadlock against `install_unmount_hook`.
        if let Some(id) = removed_id.as_deref() {
            let hooks: Vec<UnmountHook> = self.unmount_hooks.read().clone();
            for hook in &hooks {
                hook(id, &reason);
            }
        }

        // M16-Backend Phase 1: fire `on_node_unmounted` AFTER the
        // unmount hooks so consumers see the same ordering as the
        // dist crate's wire `Down` / `Exit` fan-out (local fan-out
        // first, telemetry second).
        if let Some(id) = removed_id.as_deref() {
            if let Some(sink) = self.telemetry.load().as_ref() {
                sink.on_node_unmounted(&crate::telemetry::NodeUnmounted::from_reason(id, &reason));
            }
        }

        // Live supervisor execution: only abnormal exits trigger
        // restart strategies. Clean shutdowns (cascaded by an upstream
        // supervisor) do not fan out additional restart work.
        if reason.is_abnormal() {
            if let (Some(sup_key), Some(failed_id)) = (governing_supervisor, removed_id) {
                let _ = self.execute_supervisor_plan(sup_key, &failed_id, &reason);
            }
        }
    }

    // ---- Supervision API (monitor / link / groups) ----

    /// Watch `target` from `observer`. The observer receives a
    /// `__DOWN__` system message when `target` unmounts.
    ///
    /// Returns a [`MonitorRef`] for use with [`demonitor`].
    ///
    /// [`demonitor`]: Self::demonitor
    pub fn monitor(&self, observer: NodeKey, target: NodeKey) -> MonitorRef {
        self.monitors.borrow_mut().add(observer, target)
    }

    /// Cancel a monitor. No-op if the ref isn't active.
    pub fn demonitor(&self, mref: MonitorRef) {
        self.monitors.borrow_mut().remove(mref);
    }

    /// Establish a bidirectional link between two nodes. Both will receive
    /// `__EXIT__` system messages when either unmounts. Linking a node to
    /// itself is a no-op (matches OTP).
    pub fn link(&self, a: NodeKey, b: NodeKey) {
        self.links.borrow_mut().add(a, b);
    }

    /// Remove the link between two nodes. No-op if absent.
    pub fn unlink(&self, a: NodeKey, b: NodeKey) {
        self.links.borrow_mut().remove(a, b);
    }

    /// Whether `a` and `b` are linked (in either direction).
    #[must_use]
    pub fn is_linked(&self, a: NodeKey, b: NodeKey) -> bool {
        self.links.borrow().is_linked(a, b)
    }

    /// Add `node` to a named process group. Idempotent.
    pub fn group_join(&self, group: impl Into<String>, node: NodeKey) {
        self.groups.borrow_mut().join(group, node);
    }

    /// Remove `node` from a named process group.
    pub fn group_leave(&self, group: &str, node: NodeKey) {
        self.groups.borrow_mut().leave(group, node);
    }

    /// Current members of a group, in join order.
    #[must_use]
    pub fn group_members(&self, group: &str) -> Vec<NodeKey> {
        self.groups.borrow().members(group)
    }

    /// Broadcast a message to every member of `group` from `from`.
    /// Each member receives a copy through the normal delivery path —
    /// permission manifests still apply.
    ///
    /// M14.6: after local fan-out, fires the installed
    /// [`RemoteBroadcastSink`] (if any) so the dist crate can
    /// ship `PgBroadcast` packets to remote peers holding members
    /// of `group`. The dist crate is responsible for the
    /// hub-and-spoke logic (one packet per peer regardless of
    /// member count).
    pub fn group_broadcast(
        &self,
        group: &str,
        from: impl Into<String>,
        r#type: impl Into<String>,
        payload: serde_json::Value,
    ) {
        let from = from.into();
        let r#type = r#type.into();
        let members = self.groups.borrow().members(group);
        {
            let mut q = self.messages.borrow_mut();
            for member in members {
                let Some(member_rc) = self.nodes.get(member) else {
                    continue;
                };
                let member_id = member_rc.borrow().id.clone();
                q.push_back(create_message(
                    r#type.clone(),
                    from.clone(),
                    member_id,
                    payload.clone(),
                    None,
                ));
            }
        }
        // M14.6: fire the broadcast sink for cross-node fan-out.
        // Qualify `from` with our node name (when set + not
        // already qualified) so peers see an authoritative
        // `from_local_id@our_name` source.
        let our_node = self.node_name.read().clone();
        let from_qualified = qualify_from(&from, our_node.as_deref());
        if let Some(sink) = self.remote_broadcast_sink.read().clone() {
            sink(group, &from_qualified, &r#type, &payload);
        }
    }

    /// Number of active monitors. For tests and introspection.
    #[must_use]
    pub fn monitor_count(&self) -> usize {
        self.monitors.borrow().len()
    }

    // ---- Supervisor live execution (M5 close-out) ----

    /// Install a [`Supervisor`] on the scheduler. The supervisor stays
    /// inert until at least one child is registered via [`supervise`].
    pub fn install_supervisor(&mut self, supervisor: Supervisor) -> SupervisorKey {
        self.supervisors.insert(supervisor)
    }

    /// Register a [`ChildSpec`] under the given supervisor: stores the
    /// spec on the supervisor, mounts the child via
    /// [`add_node_at_runtime`], and records the reverse index so future
    /// abnormal exits trigger the supervisor's restart strategy.
    ///
    /// [`add_node_at_runtime`]: Self::add_node_at_runtime
    pub fn supervise(
        &mut self,
        supervisor: SupervisorKey,
        spec: ChildSpec,
    ) -> Result<NodeKey, RuntimeError> {
        if !self.supervisors.contains_key(supervisor) {
            return Err(RuntimeError::NodeNotRegistered {
                id: format!("supervisor:{supervisor:?}"),
            });
        }
        // Stash a copy on the supervisor for later restarts.
        self.supervisors[supervisor].add_child(spec.clone());
        // Mount the child node — the runtime form of the supervisor's
        // declared spec.
        let key =
            self.add_node_at_runtime(spec.definition, spec.id, spec.props, spec.connect_to)?;
        self.node_to_supervisor.insert(key, supervisor);
        Ok(key)
    }

    /// Look up the supervisor governing `node`, if any. For tests.
    #[must_use]
    pub fn supervisor_of(&self, node: NodeKey) -> Option<SupervisorKey> {
        self.node_to_supervisor.get(&node).copied()
    }

    /// Number of installed supervisors.
    #[must_use]
    pub fn supervisor_count(&self) -> usize {
        self.supervisors.len()
    }

    /// **M16-Backend P4.** Snapshot every registered node for the
    /// management API's `GET /v1/nodes`. Walks the
    /// [`crate::runtime::directory::GraphDirectory`] +
    /// `node_to_supervisor` reverse index in one pass; allocates one
    /// [`NodeIntrospection`] per entry.
    ///
    /// Cost is O(n) where n = number of registered nodes; called
    /// once per scrape from the management API drain loop in
    /// `commands.rs::drive_handle_until`. NOT a hot path.
    #[must_use]
    pub fn introspect_nodes(&self) -> Vec<crate::telemetry::NodeIntrospection> {
        let directory = self.directory.borrow();
        // Build node-id → supervisor-key string map. Keyed by id
        // (not NodeKey) because the directory iterates by id.
        let mut node_to_sup: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (node_key, sup_key) in &self.node_to_supervisor {
            if let Some(node_rc) = self.nodes.get(*node_key) {
                let id = node_rc.borrow().id.clone();
                node_to_sup.insert(id, format_supervisor_key(*sup_key));
            }
        }
        directory
            .entries()
            .map(|entry| crate::telemetry::NodeIntrospection {
                id: entry.id.clone(),
                role: entry.role.clone(),
                depth: entry.depth,
                path: entry.path.clone(),
                supervisor: node_to_sup.get(&entry.id).cloned(),
            })
            .collect()
    }

    /// **M16-Backend P4.** Snapshot every installed supervisor +
    /// the node IDs currently mounted under it, for the management
    /// API's `GET /v1/supervisor/tree`.
    #[must_use]
    pub fn introspect_supervisors(&self) -> Vec<crate::telemetry::SupervisorIntrospection> {
        // Build supervisor-key → child-node-id-list reverse index.
        let mut children_by_sup: std::collections::HashMap<SupervisorKey, Vec<String>> =
            std::collections::HashMap::new();
        for (node_key, sup_key) in &self.node_to_supervisor {
            if let Some(node_rc) = self.nodes.get(*node_key) {
                let id = node_rc.borrow().id.clone();
                children_by_sup.entry(*sup_key).or_default().push(id);
            }
        }
        // Stable iteration order: sort each supervisor's children
        // alphabetically so the API output is deterministic across
        // calls.
        for ids in children_by_sup.values_mut() {
            ids.sort();
        }
        self.supervisors
            .iter()
            .map(|(sup_key, sup)| crate::telemetry::SupervisorIntrospection {
                key: format_supervisor_key(sup_key),
                strategy: format!("{:?}", sup.strategy),
                max_restarts: sup.tracker.max_restarts,
                period_secs: sup.tracker.period.as_secs(),
                restart_count: sup.tracker.count(),
                children: children_by_sup.remove(&sup_key).unwrap_or_default(),
            })
            .collect()
    }

    /// **M16-Backend P4.** Look up a single node by ID for
    /// `GET /v1/nodes/{id}`. Returns `None` if no node with the
    /// given ID is currently registered.
    #[must_use]
    pub fn introspect_node(&self, id: &str) -> Option<crate::telemetry::NodeIntrospection> {
        let directory = self.directory.borrow();
        let entry = directory.find_by_id(id)?;
        let supervisor = self
            .id_to_key
            .get(id)
            .and_then(|key| self.node_to_supervisor.get(key))
            .copied()
            .map(format_supervisor_key);
        Some(crate::telemetry::NodeIntrospection {
            id: entry.id.clone(),
            role: entry.role.clone(),
            depth: entry.depth,
            path: entry.path.clone(),
            supervisor,
        })
    }

    /// Run the supervisor restart loop for a child that just exited.
    ///
    /// **M14.10.** Before computing the plan, consult any installed
    /// [`SuperviseDecisionHook`]. On [`SuperviseDecision::Defer`],
    /// queue the decision in
    /// [`Self::pending_supervisor_decisions`] and return `Ok(())`;
    /// the next `process_batch` re-evaluates the hook (or escalates
    /// past `retry_after`).
    ///
    /// 1. Look up the governing supervisor + its strategy.
    /// 2. Compute a [`RestartPlan`] via `plan(failed_id, reason)`.
    /// 3. If the plan is empty, no-op.
    /// 4. Otherwise record the restart in the tracker. If that exceeds
    ///    intensity/period, give up — the supervisor's children stay
    ///    down and a `SupervisorError` is returned to the caller.
    /// 5. Terminate every ID in `plan.terminate` (clean shutdown).
    /// 6. Re-mount every ID in `plan.restart` in the supervisor's
    ///    declaration order, threading the original `ChildSpec` payload.
    fn execute_supervisor_plan(
        &mut self,
        sup_key: SupervisorKey,
        failed_id: &str,
        reason: &ExitReason,
    ) -> Result<(), SupervisorError> {
        // M14.10: hook check. Snapshot the Arc out of the lock so
        // the closure runs with no scheduler locks held — symmetric
        // with the unmount-hook discipline.
        let hook = self.supervise_decision_hook.read().clone();
        if let Some(h) = &hook {
            match h(sup_key, failed_id, reason) {
                SuperviseDecision::Proceed => {}
                SuperviseDecision::Defer {
                    reason: defer_reason,
                    retry_after,
                } => {
                    tracing::debug!(
                        "supervisor decision deferred for child `{failed_id}` under sup {sup_key:?}: {defer_reason}"
                    );
                    self.pending_supervisor_decisions.borrow_mut().push(
                        DeferredSupervisorDecision {
                            sup_key,
                            failed_id: failed_id.to_string(),
                            reason: reason.clone(),
                            deferred_at: std::time::Instant::now(),
                            retry_after,
                        },
                    );
                    return Ok(());
                }
            }
        }
        self.execute_supervisor_plan_inner(sup_key, failed_id, reason)
    }

    /// **M14.10.** Hook-bypassing inner body of
    /// [`Self::execute_supervisor_plan`]. Called either directly
    /// when no hook is installed / the hook returned `Proceed`, or
    /// via [`Self::drain_pending_supervisor_decisions`] when a
    /// previously-deferred decision is unparked or escalated.
    fn execute_supervisor_plan_inner(
        &mut self,
        sup_key: SupervisorKey,
        failed_id: &str,
        reason: &ExitReason,
    ) -> Result<(), SupervisorError> {
        let plan = self.supervisors[sup_key].plan(failed_id, reason)?;
        if plan.terminate.is_empty() && plan.restart.is_empty() {
            return Ok(());
        }
        self.supervisors[sup_key]
            .tracker
            .record(std::time::Instant::now())?;

        // Snapshot the specs we'll need to re-mount, *before* any
        // termination removes nodes that share the supervisor.
        let restart_specs: Vec<ChildSpec> = plan
            .restart
            .iter()
            .filter_map(|id| self.supervisors[sup_key].find_child(id).cloned())
            .collect();

        // Phase 1: terminate listed siblings cleanly. Use Shutdown so
        // they don't recursively trigger more supervisor work.
        for id in &plan.terminate {
            if let Some(&node_key) = self.id_to_key.get(id) {
                self.unmount_with_reason(node_key, ExitReason::Shutdown);
            }
        }

        // Phase 2: re-mount in the supervisor's declared order.
        for spec in restart_specs {
            // Skip if a sibling unmount above already cleared it; we
            // re-mount fresh either way.
            let _ = self.add_node_at_runtime(
                spec.definition.clone(),
                spec.id.clone(),
                spec.props.clone(),
                spec.connect_to.clone(),
            );
            // Re-establish the supervision link.
            if let Some(&new_key) = self.id_to_key.get(&spec.id) {
                self.node_to_supervisor.insert(new_key, sup_key);
            }
        }
        Ok(())
    }

    /// Whether a child has the given restart policy. For tests.
    #[must_use]
    pub fn child_policy(&self, sup_key: SupervisorKey, child_id: &str) -> Option<RestartPolicy> {
        self.supervisors
            .get(sup_key)
            .and_then(|s| s.find_child(child_id).map(|c| c.policy))
    }

    /// Dispatch a single topology event to all registered topology
    /// handlers, honoring per-handler filters (event kind + neighbor
    /// proximity).
    fn dispatch_topology_event(&self, event: &TopologyEvent) {
        // M16-Backend Phase 1: fire `on_topology_event` once per
        // dispatched event, regardless of fan-out count. Stable
        // string `kind` lets consumers (Phase-2 Prometheus) use it
        // as a label value directly.
        if let Some(sink) = self.telemetry.load().as_ref() {
            let occurred = match event {
                TopologyEvent::NodeAdded { node_id } => crate::telemetry::TopologyEventOccurred {
                    kind: "node_added",
                    node_id: Some(node_id.as_str()),
                    edge_u: None,
                    edge_v: None,
                    edge_key: None,
                },
                TopologyEvent::NodeRemoved { node_id } => crate::telemetry::TopologyEventOccurred {
                    kind: "node_removed",
                    node_id: Some(node_id.as_str()),
                    edge_u: None,
                    edge_v: None,
                    edge_key: None,
                },
                TopologyEvent::EdgeAdded { u, v, key } => crate::telemetry::TopologyEventOccurred {
                    kind: "edge_added",
                    node_id: None,
                    edge_u: Some(u.as_str()),
                    edge_v: Some(v.as_str()),
                    edge_key: Some(key.as_str()),
                },
                TopologyEvent::EdgeRemoved { u, v, key } => {
                    crate::telemetry::TopologyEventOccurred {
                        kind: "edge_removed",
                        node_id: None,
                        edge_u: Some(u.as_str()),
                        edge_v: Some(v.as_str()),
                        edge_key: Some(key.as_str()),
                    }
                }
                TopologyEvent::EdgeUpdated { u, v, key } => {
                    crate::telemetry::TopologyEventOccurred {
                        kind: "edge_updated",
                        node_id: None,
                        edge_u: Some(u.as_str()),
                        edge_v: Some(v.as_str()),
                        edge_key: Some(key.as_str()),
                    }
                }
            };
            sink.on_topology_event(&occurred);
        }
        // Snapshot keys first so we don't hold a borrow on `self.nodes`
        // while iterating; handlers invoked below may mutate scheduler state.
        let receivers: Vec<NodeKey> = self.nodes.keys().collect();
        for receiving_key in receivers {
            let Some(node_rc) = self.nodes.get(receiving_key) else {
                continue;
            };
            if node_rc.borrow().is_unmounted() {
                continue;
            }
            let receiving_id = node_rc.borrow().id.clone();
            let handlers = node_rc.borrow().topology_handlers.clone();
            // Install HookCtx for the receiver so handlers can call
            // `use_emit`, ctx mutations, etc., the same as inside a body.
            let neighbors = self
                .graph
                .borrow()
                .neighbors(&receiving_id)
                .unwrap_or_default();
            let hook_ctx = self.build_hook_ctx(receiving_key, receiving_id.clone(), neighbors);
            set_hook_ctx(Some(hook_ctx));
            for (handler, options) in handlers {
                if !matches_filter_for(&self.graph.borrow(), &receiving_id, event, options.as_ref())
                {
                    continue;
                }
                handler(event);
            }
            set_hook_ctx(None);
        }
    }

    /// Public entry point for tests / hosts that want to manually dispatch
    /// a synthetic topology event without going through the graph.
    pub fn dispatch_topology(&self, event: &TopologyEvent) {
        self.dispatch_topology_event(event);
    }

    // ---- Internal: evaluation + delivery ----

    fn evaluate(&self, key: NodeKey) {
        let Some(node_rc) = self.nodes.get(key) else {
            return;
        };
        if node_rc.borrow().is_unmounted() {
            return;
        }

        let node_id = node_rc.borrow().id.clone();
        let neighbors = self.graph.borrow().neighbors(&node_id).unwrap_or_default();
        let (depth, path) = self
            .directory
            .borrow()
            .find_by_id(&node_id)
            .map(|e| (e.depth, e.path.clone()))
            .unwrap_or((0, vec![node_id.clone()]));
        let props = node_rc.borrow().props.clone();
        let children = node_rc.borrow().external_children.clone();

        let ctx = NodeContext {
            id: node_id.clone(),
            props,
            children,
            my_depth: depth,
            my_path: path,
            neighbors: neighbors.clone(),
        };

        let hook_ctx = self.build_hook_ctx(key, node_id.clone(), neighbors);

        // M16-Backend Phase 1: per-node evaluate start/end. Hold an
        // `arc_swap::Guard` across the call so the start/end pair
        // sees the same sink instance — same `ArcSwap`-snapshot
        // discipline as `process_batch`. No Arc clone on the hot
        // path.
        let telemetry_guard = self.telemetry.load();
        let telemetry: Option<&Arc<crate::telemetry::TelemetrySinkRef>> = telemetry_guard.as_ref();
        if let Some(sink) = telemetry {
            sink.on_node_evaluate_start(&crate::telemetry::NodeEvaluateStart { node_id: &node_id });
        }

        evaluate_node(node_rc, ctx, hook_ctx);

        if let Some(sink) = telemetry {
            sink.on_node_evaluate_end(&crate::telemetry::NodeEvaluateEnd { node_id: &node_id });
        }

        // M12.3a-followon-E: sync the freshly-stored manifest into
        // the runtime-shared `manifests` index so Tier 2 / Tier 3
        // queries (over policy-derived reachability) reflect the
        // latest body output.
        if let Some(m) = node_rc.borrow().manifest.clone() {
            self.manifests.borrow_mut().insert(node_id.clone(), m);
        }
        // M12.3a-followon-F: same sync contract for the edge-accept
        // predicate. Bodies that don't call `use_edge_accept_handler`
        // leave the slot `None` (cleared by `reset_handlers`); we drop
        // the shared index entry in that case so default-accept
        // resumes for that node.
        match node_rc.borrow().edge_accept_predicate.clone() {
            Some(p) => {
                self.edge_accept_predicates
                    .borrow_mut()
                    .insert(node_id.clone(), p);
            }
            None => {
                self.edge_accept_predicates.borrow_mut().remove(&node_id);
            }
        }

        // gen_statem-style replay: any messages this node postponed in
        // earlier deliveries get re-injected at the head of the main
        // queue, ahead of any pending arrivals.
        let drained = self
            .postponed
            .borrow_mut()
            .get_mut(&key)
            .map(PostponeQueue::drain)
            .unwrap_or_default();
        if !drained.is_empty() {
            let mut q = self.messages.borrow_mut();
            for msg in drained.into_iter().rev() {
                q.push_front(msg);
            }
        }
    }

    fn deliver(&self, mut msg: Message) -> Result<(), RuntimeError> {
        let retry_message = msg.clone();
        if !decrement_ttl(&mut msg) {
            return Ok(());
        }
        let Some(&target_key) = self.id_to_key.get(&msg.to) else {
            return Ok(());
        };
        let Some(target_rc) = self.nodes.get(target_key) else {
            return Ok(());
        };
        if target_rc.borrow().is_unmounted() {
            return Ok(());
        }

        // Track the active TTL so `emit` calls inside message handlers
        // inherit it (cycle protection).
        let prev_ttl = self.current_delivery_ttl.get();
        self.current_delivery_ttl.set(Some(msg.ttl));
        // Record the per-node TTL so emits from this target's *next*
        // re-render (after the handler returns and current_delivery_ttl
        // has been reset) still inherit the cycle's decaying TTL.
        // M4-followon (2026-05-02). Outlives the synchronous handler
        // window, unlike `current_delivery_ttl`.
        self.last_received_ttl
            .borrow_mut()
            .insert(target_key, msg.ttl);

        let mut incoming = IncomingMessage::from_message(&msg);

        // **M-Ingress.** Three origin classes for non-policy-checked paths:
        // - `__ingress__` — HTTP-injected via `POST /v1/nodes/:id/messages`
        //   (`SchedulerHandle::send_ingress`). Default-deny: type must be
        //   declared in `manifest.accepts_external`. New in M-Ingress.
        // - `__external__` — in-process inject (`SchedulerHandle::send`,
        //   used by tests, benches, dist remote dispatch glue, source
        //   bridges). Legacy bypass preserved for backward compatibility.
        // - `__source__` / `__system__` — synthetic origins from
        //   `EventSource` and supervision. Legacy bypass.
        let is_ingress = msg.from == "__ingress__";
        let is_legacy_external = matches!(
            msg.from.as_str(),
            "__external__" | "__source__" | "__system__"
        );

        // Resolve disposition first so the M16-Backend telemetry sink
        // sees a single `MessageDelivered { accepted }` event with the
        // post-policy outcome regardless of which short-circuit path
        // we took. Direct messages (emit with `EmitOptions::direct`)
        // bypass full channel-policy resolution; the target's flat
        // `manifest.accepts` is still enforced — direct doesn't grant
        // a capability the target hasn't declared. Mirrors the TS
        // scheduler's `_direct` semantics introduced in Phase 9.
        let accepted = if !is_ingress && !is_legacy_external && msg.direct {
            match target_rc.borrow().manifest.clone() {
                Some(manifest) => manifest.accepts.iter().any(|t| t == &msg.r#type),
                None => false,
            }
        } else {
            let policy =
                resolve_channel_policy(&target_rc.borrow(), &msg.from, &self.graph.borrow(), None);
            let accept_listed = if is_ingress {
                // **M-Ingress.** HTTP-injected messages: default-deny
                // unless the manifest opts in. Phase 1 uses
                // `accepts_external` (single-shot). Phase 2 adds
                // call-shape: when `correlation_id.is_some()`, the
                // `request_type` must appear in
                // `accepts_external_call`. Two separate trust
                // boundaries — declaring a call-shape doesn't grant
                // single-shot inject and vice versa. `request_type`
                // alone is checked here; the (request_type,
                // reply_type) pair is gated server-side by the
                // call drain handler before this delivery runs.
                target_rc
                    .borrow()
                    .manifest
                    .as_ref()
                    .map(|m| {
                        if msg.correlation_id.is_some() {
                            m.accepts_external_call
                                .iter()
                                .any(|c| c.request_type == msg.r#type)
                        } else {
                            m.accepts_external.iter().any(|t| t == &msg.r#type)
                        }
                    })
                    .unwrap_or(false)
            } else if is_legacy_external {
                // Legacy bypass for in-process injection paths.
                true
            } else {
                policy.accepts.iter().any(|t| t == &msg.r#type)
            };
            if accept_listed {
                if let Some(transform) = &policy.transform {
                    match transform(&incoming, &Attrs::new()) {
                        Some(modified) => {
                            incoming = modified;
                            true
                        }
                        None => false,
                    }
                } else {
                    true
                }
            } else {
                false
            }
        };

        // M16-Backend Phase 1: fire `on_message_delivered` exactly
        // once, capturing the post-policy disposition. Telemetry
        // fires before handler execution so the event order matches
        // wall-clock causality (delivery decided → handlers run).
        if let Some(sink) = self.telemetry.load().as_ref() {
            sink.on_message_delivered(&crate::telemetry::MessageDelivered {
                from: msg.from.as_str(),
                to: msg.to.as_str(),
                message_type: msg.r#type.as_str(),
                ttl: msg.ttl,
                direct: msg.direct,
                accepted,
                // **M16-Backend P10.** Forward the remote OTel span
                // context (set by `graphreact_dist::dispatch_inbound`
                // for remote-origin messages) into the event so
                // `OtelSink` can attach it as a Link attribute on the
                // receiver-side `message.deliver` span.
                trace_context: msg.trace_context,
            });
        }

        if !accepted {
            self.current_delivery_ttl.set(prev_ttl);
            return Ok(());
        }

        let is_durable_activation = msg.run_id.is_some()
            && msg.attempt_id.is_some()
            && self.durable_activation_committer.is_some();
        let state_before = is_durable_activation.then(|| self.snapshot_durable_state());
        if is_durable_activation {
            *self.durable_output_buffer.borrow_mut() = Some(DurableActivationBuffer::default());
        }

        let handlers: Vec<crate::runtime::types::MessageHandler> = target_rc
            .borrow()
            .message_handlers
            .get(&msg.r#type)
            .cloned()
            .unwrap_or_default();

        // Install per-target HookCtx for the duration of handler invocation
        // so handlers can call `use_emit`, `ctx_spawn_child`, etc. — same
        // surface available from inside the component body.
        let target_id = target_rc.borrow().id.clone();
        let target_neighbors = self
            .graph
            .borrow()
            .neighbors(&target_id)
            .unwrap_or_default();
        let hook_ctx = self.build_hook_ctx(target_key, target_id, target_neighbors);
        set_hook_ctx(Some(hook_ctx));

        // **M-Ingress phase 2.** Auto-propagation context capture.
        // If this inbound message carries a `correlation_id` (i.e.
        // it came in via `POST /v1/nodes/:id/call`) AND the target
        // node's manifest declares a matching
        // `accepts_external_call` entry for this `request_type`,
        // capture `(correlation_id, reply_type)` so `make_emit`
        // can auto-tag the first matching outgoing reply. We
        // save/restore the prior value so nested deliveries
        // (handlers triggering immediate inbox push that gets
        // delivered before the outer handler returns) don't
        // cross-contaminate. First-match-only is enforced via the
        // `tagged` flag — flipped in `make_emit` after the first
        // match so subsequent matching emits in the same
        // invocation are NOT tagged.
        let prev_call_context = self.current_call_context.borrow_mut().take();
        let prev_run_context = self.current_durable_run_context.borrow_mut().take();
        *self.current_durable_run_context.borrow_mut() = msg
            .run_id
            .as_ref()
            .zip(msg.attempt_id.as_ref())
            .map(|(run_id, attempt_id)| DurableRunContext {
                run_id: run_id.clone(),
                attempt_id: attempt_id.clone(),
            });
        let new_call_context = msg.correlation_id.as_ref().and_then(|cid| {
            target_rc.borrow().manifest.as_ref().and_then(|m| {
                m.accepts_external_call
                    .iter()
                    .find(|c| c.request_type == msg.r#type)
                    .map(|c| CallContext {
                        correlation_id: cid.clone(),
                        reply_type: c.reply_type.clone(),
                        tagged: false,
                    })
            })
        });
        *self.current_call_context.borrow_mut() = new_call_context;

        for handler in handlers {
            handler(&incoming);
        }
        set_hook_ctx(None);

        // Restore the prior call context (supports nested deliveries).
        *self.current_call_context.borrow_mut() = prev_call_context;
        *self.current_durable_run_context.borrow_mut() = prev_run_context;

        if let Some(state_before) = state_before {
            let buffer = self
                .durable_output_buffer
                .borrow_mut()
                .take()
                .unwrap_or_default();
            let activation = DurableActivation {
                input: msg.clone(),
                state_before: state_before.clone(),
                state_after: self.snapshot_durable_state(),
                emitted_messages: buffer.emitted_messages,
                effect_intents: buffer.effect_intents,
            };
            let commit_result = self
                .durable_activation_committer
                .as_ref()
                .expect("durable committer checked above")
                .as_ref()(&activation);
            if let Err(error) = commit_result {
                self.restore_durable_state(state_before)?;
                self.messages.borrow_mut().push_front(retry_message);
                self.current_delivery_ttl.set(prev_ttl);
                return Err(error);
            }
            self.messages
                .borrow_mut()
                .extend(activation.emitted_messages);
        }

        // If any handler called `postpone()`, defer this message into the
        // target's postpone queue. It re-enters the main queue at the
        // next re-evaluation of this node.
        if take_postpone_flag() {
            self.postponed
                .borrow_mut()
                .entry(target_key)
                .or_default()
                .push(msg);
        }

        // Restore previous TTL context (supports nested deliveries).
        self.current_delivery_ttl.set(prev_ttl);
        Ok(())
    }

    /// Build the per-node `HookCtx` used by `evaluate_node`,
    /// `deliver`, and `dispatch_topology_event` so handlers invoked in
    /// any of those phases can read the active context.
    fn build_hook_ctx(&self, key: NodeKey, node_id: String, neighbors: Vec<String>) -> HookCtx {
        HookCtx {
            node_id: node_id.clone(),
            neighbors,
            enqueue_self: self.make_enqueue_self(key),
            emit: self.make_emit(node_id.clone()),
            enqueue_durable_effect: self.make_enqueue_durable_effect(node_id.clone()),
            register_source: self.make_register_source(key, node_id.clone()),
            unregister_source: self.make_unregister_source(),
            register_monitor: self.make_register_monitor(key, node_id.clone()),
            cancel_monitor: self.make_cancel_monitor(),
            register_link: self.make_register_link(key, node_id.clone()),
            unregister_link: self.make_unregister_link(key, node_id.clone()),
            group_join: self.make_group_join(key, node_id.clone()),
            group_leave: self.make_group_leave(key, node_id.clone()),
            spawn_child: self.make_spawn_child(node_id.clone()),
            unmount_self: self.make_unmount_self(node_id.clone()),
            compose: self.make_compose(node_id.clone()),
            accept_edge: self.make_accept_edge(node_id.clone()),
            set_edge_accept_handler: self.make_set_edge_accept_handler(key),
        }
    }

    // ---- Closure factories that hooks consume via HookCtx ----

    fn make_enqueue_self(&self, key: NodeKey) -> Rc<dyn Fn()> {
        let dirty = Rc::clone(&self.dirty);
        let height = self.heights.get(&key).copied().unwrap_or(0);
        Rc::new(move || {
            dirty.borrow_mut().insert((height, key));
        })
    }

    fn make_emit(&self, from: String) -> Rc<dyn Fn(&str, Value, EmitOptions)> {
        let messages = Rc::clone(&self.messages);
        let id_to_key = self.id_to_key.clone();
        let current_ttl = Rc::clone(&self.current_delivery_ttl);
        let last_received_ttl = Rc::clone(&self.last_received_ttl);
        let call_context = Rc::clone(&self.current_call_context);
        let durable_run_context = Rc::clone(&self.current_durable_run_context);
        let durable_output_buffer = Rc::clone(&self.durable_output_buffer);
        // **M-Ingress phase 2.** Snapshot the telemetry sink at
        // make_emit time so the auto-propagation `on_call_reply`
        // path can fire without re-acquiring the ArcSwap on every
        // emit. Same shape as the other snapshot-at-build-time
        // captures (`remote_sink`, `our_node_name`); install /
        // replace lands on the next hook-ctx rebuild.
        let telemetry_snapshot: Option<Arc<crate::telemetry::TelemetrySinkRef>> =
            self.telemetry.load_full();
        let from_key = self.id_to_key.get(&from).copied();
        // Snapshot neighbor lookup table for emission targeting.
        let graph = self.graph.borrow();
        let neighbors_table: FxHashMap<String, Vec<String>> = self
            .id_to_key
            .keys()
            .map(|id| (id.clone(), graph.neighbors(id).unwrap_or_default()))
            .collect();
        // Snapshot per-edge TTL annotations so `<edge ttl={N}/>` caps
        // the emission TTL on that edge. Stored under both `(u, v)` and
        // `(v, u)` keys to handle the graph's undirected-edge model.
        // M4-followon (2026-05-02): closes the gap where `make_emit`
        // ignored edge attrs entirely. Snapshot at construction time —
        // dynamic edge mutations (compose / accept_edge) won't reflect
        // until the next hook-ctx rebuild.
        let edge_ttl_table: FxHashMap<(String, String), u32> = {
            let mut t: FxHashMap<(String, String), u32> = FxHashMap::default();
            for (u, v, key) in graph.edges() {
                let Ok(attrs) = graph.get_edge_data(&u, &v, Some(&key)) else {
                    continue;
                };
                let Some(ttl) = attrs
                    .get("ttl")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                else {
                    continue;
                };
                t.insert((u.clone(), v.clone()), ttl);
                t.insert((v, u), ttl);
            }
            t
        };
        // M14 distribution hook: snapshot the sink + node_name once
        // at make_emit time so the closure doesn't re-acquire locks
        // on every call. Subsequent install_remote_sink / set_node_name
        // reach the next hook-ctx rebuild.
        let remote_sink = self.remote_sink.read().clone();
        let our_node_name = self.node_name.read().clone();

        Rc::new(move |r#type: &str, payload: Value, opts: EmitOptions| {
            let neighbors = neighbors_table.get(&from).cloned().unwrap_or_default();
            let is_direct = opts.direct.is_some();
            let targets: Vec<String> = if let Some(direct) = opts.direct.clone() {
                vec![direct]
            } else if let Some(to) = opts.to.clone() {
                to
            } else {
                neighbors
            };
            // TTL resolution priority (highest first):
            //   1. Explicit `EmitOptions::ttl` from the caller.
            //   2. The currently-active delivery's TTL (inherited when
            //      emitting from inside a synchronous message handler).
            //   3. **The from-node's last-received TTL** — survives
            //      across the dirty → re-evaluate → flush_effects boundary
            //      so emits issued from a useEffect that fires *after*
            //      the handler returns still inherit the cycle's TTL.
            //   4. `DEFAULT_TTL`.
            // Then capped to the per-edge `<edge ttl={N}/>` annotation
            // (per target) so explicit edge bounds always hold.
            let inherited_ttl_opt = opts
                .ttl
                .or_else(|| current_ttl.get())
                .or_else(|| from_key.and_then(|k| last_received_ttl.borrow().get(&k).copied()));
            let proposed_ttl = inherited_ttl_opt.unwrap_or(DEFAULT_TTL);

            // **M-Ingress phase 2.** Decide once per emit whether to
            // auto-tag this outgoing message with the inbound
            // call's correlation_id. Tagging happens IFF:
            //   1. There's an active call context (set by `deliver`
            //      when an inbound `correlation_id` met the manifest
            //      gate).
            //   2. The outgoing `type` matches the call's declared
            //      `reply_type`.
            //   3. We haven't already tagged a prior outgoing message
            //      in this handler invocation (first-match-only).
            // We compute the id once and apply to every constructed
            // Message in this loop iteration's broadcast (multi-
            // neighbor emit produces one outgoing per neighbor; for
            // a typical single-neighbor reply, only one is created).
            // Then flip `tagged` so subsequent emit() calls in the
            // same handler don't tag.
            let auto_correlation_id: Option<String> = {
                let mut slot = call_context.borrow_mut();
                match slot.as_mut() {
                    Some(ctx) if !ctx.tagged && ctx.reply_type == r#type => {
                        ctx.tagged = true;
                        Some(ctx.correlation_id.clone())
                    }
                    _ => None,
                }
            };
            let run_context = durable_run_context.borrow().clone();

            // **M-Ingress phase 2.** When auto-tagging fires, surface
            // the reply payload to any installed `TelemetrySink` so the
            // `PendingCalls` registry (the consumer in
            // `graphreact-observer`) can resolve the awaiting
            // `tokio::sync::oneshot::Sender` IMMEDIATELY at emit-time.
            // This decouples call resolution from actual scheduler
            // delivery — leaf-node call handlers can reply without
            // needing graph connectivity to a reply-accepting neighbor.
            // The reply Message still flows through the normal queue
            // for any listening graph consumers (e.g. monitoring nodes
            // that also accept the reply type).
            if let Some(cid) = auto_correlation_id.as_deref() {
                if let Some(sink_arc) = telemetry_snapshot.as_ref() {
                    sink_arc.0.on_call_reply(&crate::telemetry::CallReply {
                        correlation_id: cid,
                        payload: &payload,
                        from: from.as_str(),
                        reply_type: r#type,
                    });
                }
            }

            for target in targets {
                // M14 distribution: classify the target into Local /
                // LocalStripped (target@<our_name>) / Remote (@<other>).
                let (lookup_target, is_remote_addr) =
                    classify_target(&target, our_node_name.as_deref());

                if id_to_key.contains_key(&lookup_target) {
                    // Local enqueue. Use the stripped target for
                    // the lookup AND the message's `to` field so
                    // existing single-node handlers continue to see
                    // bare local IDs.
                    let final_ttl = match edge_ttl_table.get(&(from.clone(), lookup_target.clone()))
                    {
                        Some(&edge_cap) => proposed_ttl.min(edge_cap),
                        None => proposed_ttl,
                    };
                    let mut msg = create_message(
                        r#type,
                        from.clone(),
                        lookup_target,
                        payload.clone(),
                        Some(final_ttl),
                    );
                    msg.direct = is_direct;
                    msg.correlation_id.clone_from(&auto_correlation_id);
                    if let Some(context) = &run_context {
                        msg.id = uuid::Uuid::now_v7().to_string();
                        msg.run_id = Some(context.run_id.clone());
                        msg.attempt_id = Some(context.attempt_id.clone());
                    }
                    if let Some(buffer) = durable_output_buffer.borrow_mut().as_mut() {
                        buffer.emitted_messages.push(msg);
                    } else {
                        messages.borrow_mut().push_back(msg);
                    }
                } else if is_remote_addr {
                    // Phase A durable runs are single-deployment and may not
                    // cross simultaneously active GraphReact peers.
                    if run_context.is_some() {
                        continue;
                    }
                    // Cross-node target. Build the message with the
                    // ORIGINAL (unstripped) target + a qualified
                    // `from` ("local@<our_name>") so the remote
                    // peer's scheduler can route replies back to
                    // us. No edge-TTL cap — `<edge ttl={N}/>`
                    // annotations live on the local graph only.
                    let qualified_from = qualify_from(&from, our_node_name.as_deref());
                    let mut msg = create_message(
                        r#type,
                        qualified_from,
                        target,
                        payload.clone(),
                        Some(proposed_ttl),
                    );
                    msg.direct = is_direct;
                    msg.correlation_id.clone_from(&auto_correlation_id);
                    if let Some(sink) = &remote_sink {
                        // Sink is responsible for delivery; we don't
                        // enqueue locally. Sink's bool return is
                        // advisory — `false` means "I declined, fall
                        // through" but we have no fallback today, so
                        // both shapes drop the message symmetrically
                        // (matches OTP's "send to disconnected node"
                        // semantics).
                        let _ = sink(&msg);
                    }
                    // No sink installed → message is silently dropped,
                    // mirroring today's behavior for unknown locals.
                }
                // else: bare local target not in id_to_key → drop
                // (today's behavior, unchanged).
            }
        })
    }

    fn make_enqueue_durable_effect(
        &self,
        node_id: String,
    ) -> Rc<dyn Fn(DurableEffectRequest) -> Result<String, RuntimeError>> {
        let run_context = Rc::clone(&self.current_durable_run_context);
        let activation_buffer = Rc::clone(&self.durable_output_buffer);
        Rc::new(move |request: DurableEffectRequest| {
            if request.key.trim().is_empty() {
                return Err(RuntimeError::EmptyDurableEffectKey);
            }
            let context = run_context
                .borrow()
                .clone()
                .ok_or(RuntimeError::DurableEffectOutsideRun)?;
            let mut slot = activation_buffer.borrow_mut();
            let buffer = slot.as_mut().ok_or(RuntimeError::DurableEffectOutsideRun)?;
            if !buffer.effect_keys.insert(request.key.clone()) {
                return Err(RuntimeError::DuplicateDurableEffectKey { key: request.key });
            }
            let id = uuid::Uuid::now_v7().to_string();
            buffer.effect_intents.push(DurableEffectIntent {
                id: id.clone(),
                key: request.key,
                run_id: context.run_id,
                attempt_id: context.attempt_id,
                node_id: node_id.clone(),
                adapter: request.adapter,
                operation: request.operation,
                input: request.input,
                result_type: request.result_type,
                error_type: request.error_type,
                replay_policy: request.replay_policy,
            });
            Ok(id)
        })
    }

    fn make_register_source(
        &self,
        key: NodeKey,
        node_id: String,
    ) -> Rc<dyn Fn(std::sync::Arc<dyn EventSource>)> {
        let sources = Rc::clone(&self.sources);
        let inbox = Arc::clone(&self.external_inbox);
        let notify = Arc::clone(&self.notify);
        // Snapshot the wake closure now (Arc clone). If the host
        // installs / replaces wake later, sources registered after
        // that pick up the new one — current registrations keep the
        // snapshot.
        let wake = self.external_wake.lock().clone();
        Rc::new(move |source: std::sync::Arc<dyn EventSource>| {
            let id = source.id().to_string();
            sources.borrow_mut().insert(id, key);

            // Sources push events directly into the cross-thread
            // external inbox; the next `process_batch` (or daemon
            // wake-up) drains them. This avoids requiring a LocalSet
            // for every scheduler entry that registers a source.
            let bridge = SourceBridge::new(
                Arc::clone(&inbox),
                Arc::clone(&notify),
                node_id.clone(),
                wake.clone(),
            );
            source.start(bridge.into_push());
        })
    }

    fn make_unregister_source(&self) -> Rc<dyn Fn(&str)> {
        let sources = Rc::clone(&self.sources);
        Rc::new(move |id: &str| {
            sources.borrow_mut().remove(id);
        })
    }

    // ---- Supervision callbacks consumed by HookCtx ----

    /// **M14.5.** String-taking monitor registration. Routes local
    /// vs remote based on whether the target string contains an
    /// `@<other_node>` suffix that doesn't match `our_node_name`.
    /// For local targets the closure mirrors the pre-M14.5 path
    /// (lookup `id_to_key`, register against the resolved key).
    /// For remote targets it allocates a stub ref against the
    /// shared sentinel and fires the installed
    /// [`RemoteMonitorSink`]. Returns `None` if the target can't
    /// be resolved (unknown local id) or if the remote path is
    /// requested but the sentinel / sink isn't installed.
    fn make_register_monitor(
        &self,
        observer: NodeKey,
        observer_id: String,
    ) -> Rc<dyn Fn(&str) -> Option<MonitorRef>> {
        let monitors = Rc::clone(&self.monitors);
        let id_to_key = self.id_to_key.clone();
        let our_node = self.node_name.read().clone();
        let sentinel = *self.remote_sentinel.read();
        let sink = self.remote_monitor_sink.read().clone();
        Rc::new(move |target_id: &str| -> Option<MonitorRef> {
            let (lookup_target, is_remote) = classify_target(target_id, our_node.as_deref());
            if !is_remote {
                let target_key = id_to_key.get(&lookup_target).copied()?;
                return Some(monitors.borrow_mut().add(observer, target_key));
            }
            let sentinel = sentinel?;
            let sink = sink.as_ref()?;
            let mref = monitors.borrow_mut().add(observer, sentinel);
            sink(observer, &observer_id, target_id, mref);
            Some(mref)
        })
    }

    fn make_cancel_monitor(&self) -> Rc<dyn Fn(MonitorRef)> {
        let monitors = Rc::clone(&self.monitors);
        Rc::new(move |mref: MonitorRef| monitors.borrow_mut().remove(mref))
    }

    /// **M14.5.** String-taking link registration. Same local-vs-
    /// remote routing as [`Self::make_register_monitor`]. Returns
    /// `true` if the (observer, target) link was established —
    /// locally if the target resolves, or via the dist sink for
    /// remote targets.
    fn make_register_link(&self, this: NodeKey, this_id: String) -> Rc<dyn Fn(&str) -> bool> {
        let links = Rc::clone(&self.links);
        let id_to_key = self.id_to_key.clone();
        let our_node = self.node_name.read().clone();
        let sentinel = *self.remote_sentinel.read();
        let sink = self.remote_link_sink.read().clone();
        Rc::new(move |target_id: &str| -> bool {
            let (lookup_target, is_remote) = classify_target(target_id, our_node.as_deref());
            if !is_remote {
                let Some(&target_key) = id_to_key.get(&lookup_target) else {
                    return false;
                };
                links.borrow_mut().add(this, target_key);
                return true;
            }
            let Some(sentinel) = sentinel else {
                return false;
            };
            let Some(sink) = sink.as_ref() else {
                return false;
            };
            // Idempotent at the LinkRegistry level — `add` is a
            // set insertion. The dist sink is responsible for
            // bookkeeping per-peer link counts so the sentinel
            // entry is only torn down when ALL remote links from
            // this observer have unwound.
            links.borrow_mut().add(this, sentinel);
            sink(this, &this_id, target_id)
        })
    }

    fn make_unregister_link(&self, this: NodeKey, this_id: String) -> Rc<dyn Fn(&str)> {
        let links = Rc::clone(&self.links);
        let id_to_key = self.id_to_key.clone();
        let our_node = self.node_name.read().clone();
        let sink = self.remote_unlink_sink.read().clone();
        Rc::new(move |target_id: &str| {
            let (lookup_target, is_remote) = classify_target(target_id, our_node.as_deref());
            if !is_remote {
                if let Some(&target_key) = id_to_key.get(&lookup_target) {
                    links.borrow_mut().remove(this, target_key);
                }
                return;
            }
            // Remote path: the dist sink owns the per-peer link
            // counter and the (observer, sentinel) tear-down via
            // `SchedulerHandle::unlink_remote`. We do NOT touch
            // `LinkRegistry` here — letting the dist crate decide
            // when the last remote link unwinds.
            if let Some(sink) = sink.as_ref() {
                sink(this, &this_id, target_id);
            }
        })
    }

    /// **M14.5.** Drain `pending_remote_demonitors` and
    /// `pending_remote_unlinks` — both pushed from tokio worker
    /// threads via [`SchedulerHandle::demonitor_remote`] /
    /// [`SchedulerHandle::unlink_remote`]. Runs at the head of
    /// every `process_batch` round so the local
    /// `MonitorRegistry` / `LinkRegistry` shed entries pointing
    /// at the shared sentinel before the next evaluation pass.
    fn drain_pending_remote_supervision(&mut self) {
        let demonitors: Vec<MonitorRef> =
            std::mem::take(&mut *self.pending_remote_demonitors.lock());
        if !demonitors.is_empty() {
            let mut monitors = self.monitors.borrow_mut();
            for mref in demonitors {
                monitors.remove(mref);
            }
        }
        let unlinks: Vec<NodeKey> = std::mem::take(&mut *self.pending_remote_unlinks.lock());
        if !unlinks.is_empty() {
            if let Some(sentinel) = *self.remote_sentinel.read() {
                let mut links = self.links.borrow_mut();
                for observer in unlinks {
                    links.remove(observer, sentinel);
                }
            }
        }
    }

    /// **M-Prod sub-track #4 Phase γ (M14.11).** Drain
    /// [`Self::pending_delegated_restarts`] — cross-node
    /// delegated-restart requests pushed by the dist crate's
    /// `dispatch_inbound` via
    /// [`SchedulerHandle::request_delegated_restart`].
    ///
    /// Per entry: look up the child via `id_to_key`. If absent →
    /// outcome `Failed { error_kind = "unknown_child" }`. Look up
    /// the governing supervisor via `node_to_supervisor`. If absent
    /// → outcome `Failed { error_kind = "not_supervised" }`.
    /// Otherwise synthesize `ExitReason::Crashed { detail }` (the
    /// origin peer + correlation id + reason code are folded into
    /// `detail` for human visibility) and call
    /// `unmount_with_reason`. The existing path drives
    /// `execute_supervisor_plan` automatically (preserving the
    /// `RestartPolicy` / intensity tracker contract). Post-call,
    /// `id_to_key.contains_key(child_id)` is the success
    /// indicator — re-mounted children re-populate the id at the
    /// same key.
    ///
    /// Outcomes are pushed onto
    /// [`Self::pending_delegated_restart_outcomes`] for the dist
    /// crate to drain via
    /// [`SchedulerHandle::poll_delegated_restart_outcomes`] and
    /// ship as `RestartDelegationResult` packets.
    fn drain_pending_delegated_restarts(&mut self) {
        let requests: Vec<DelegatedRestartRequest> =
            std::mem::take(&mut *self.pending_delegated_restarts.lock());
        if requests.is_empty() {
            return;
        }
        let mut outcomes: Vec<DelegatedRestartOutcome> = Vec::with_capacity(requests.len());
        for req in requests {
            let started = std::time::Instant::now();
            let key_opt = self.id_to_key.get(&req.child_node_id).copied();
            let Some(node_key) = key_opt else {
                outcomes.push(DelegatedRestartOutcome {
                    origin_peer: req.origin_peer,
                    correlation_id: req.correlation_id,
                    child_node_id: req.child_node_id,
                    completed: false,
                    error_kind: Some("unknown_child"),
                    reason: Some("child id not found in scheduler".to_string()),
                    duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                });
                continue;
            };
            if !self.node_to_supervisor.contains_key(&node_key) {
                outcomes.push(DelegatedRestartOutcome {
                    origin_peer: req.origin_peer,
                    correlation_id: req.correlation_id,
                    child_node_id: req.child_node_id,
                    completed: false,
                    error_kind: Some("not_supervised"),
                    reason: Some("child is not governed by any supervisor".to_string()),
                    duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                });
                continue;
            }
            let detail = format!(
                "delegated_restart from `{}` (corr={}, reason={})",
                req.origin_peer, req.correlation_id, req.reason_code
            );
            self.unmount_with_reason(
                node_key,
                crate::runtime::supervision::ExitReason::Crashed { detail },
            );
            // Success classification: the supervisor's plan
            // re-mounts the child via `add_node_at_runtime`, which
            // re-populates `id_to_key` with the same id (potentially
            // a fresh `NodeKey`). Children whose policy is
            // `Temporary` OR whose intensity tracker refused the
            // restart leave `id_to_key` empty.
            let completed = self.id_to_key.contains_key(&req.child_node_id);
            let (error_kind, reason) = if completed {
                (None, None)
            } else {
                (
                    Some("policy_refused_or_intensity_exceeded"),
                    Some(
                        "supervisor policy or intensity tracker did not re-mount the child"
                            .to_string(),
                    ),
                )
            };
            outcomes.push(DelegatedRestartOutcome {
                origin_peer: req.origin_peer,
                correlation_id: req.correlation_id,
                child_node_id: req.child_node_id,
                completed,
                error_kind,
                reason,
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        }
        if !outcomes.is_empty() {
            self.pending_delegated_restart_outcomes
                .lock()
                .extend(outcomes);
        }
    }

    /// **M14.10.** Walk
    /// [`Self::pending_supervisor_decisions`]; for each entry,
    /// either execute the plan (hook returned `Proceed` OR the
    /// `retry_after` deadline elapsed) or keep it parked. Drops
    /// decisions whose supervisor was removed in the meantime
    /// (defensive — prevents a stale `sup_key` from indexing into
    /// a freed slot).
    fn drain_pending_supervisor_decisions(&mut self) {
        let pending: Vec<DeferredSupervisorDecision> =
            std::mem::take(&mut *self.pending_supervisor_decisions.borrow_mut());
        if pending.is_empty() {
            return;
        }
        // Snapshot the hook so the closure runs without holding
        // the RwLock — same discipline as `execute_supervisor_plan`.
        let hook = self.supervise_decision_hook.read().clone();
        let now = std::time::Instant::now();
        let mut keep = Vec::new();
        for entry in pending {
            // Defensive: supervisor unmounted while parked → drop
            // the decision rather than indexing into a freed slot.
            if !self.supervisors.contains_key(entry.sup_key) {
                tracing::debug!(
                    "supervisor decision dropped for child `{}` — supervisor {:?} no longer exists",
                    entry.failed_id,
                    entry.sup_key
                );
                continue;
            }
            let elapsed = now.duration_since(entry.deferred_at);
            let escalated = entry.retry_after.is_some_and(|d| elapsed >= d);
            let decision = if escalated {
                SuperviseDecision::Proceed
            } else if let Some(h) = &hook {
                h(entry.sup_key, &entry.failed_id, &entry.reason)
            } else {
                // Hook removed since the decision was parked: just
                // proceed; we have no reason to keep deferring.
                SuperviseDecision::Proceed
            };
            match decision {
                SuperviseDecision::Proceed => {
                    if escalated {
                        tracing::warn!(
                            "supervisor decision escalated for child `{}` (deferred {}ms ago)",
                            entry.failed_id,
                            elapsed.as_millis()
                        );
                    }
                    let _ = self.execute_supervisor_plan_inner(
                        entry.sup_key,
                        &entry.failed_id,
                        &entry.reason,
                    );
                    // M16-Backend Phase 1: fire `on_supervisor_decision`
                    // AFTER the plan runs so consumers see the action +
                    // its outcome at a single boundary (Proceeded if
                    // the hook released the deferral; Escalated if the
                    // retry_after deadline elapsed).
                    if let Some(sink) = self.telemetry.load().as_ref() {
                        sink.on_supervisor_decision(&crate::telemetry::SupervisorDecisionMade {
                            failed_id: &entry.failed_id,
                            reason: entry.reason.as_str(),
                            action: if escalated {
                                crate::telemetry::SupervisorAction::Escalated
                            } else {
                                crate::telemetry::SupervisorAction::Proceeded
                            },
                        });
                    }
                }
                SuperviseDecision::Defer {
                    reason: _,
                    retry_after,
                } => {
                    if let Some(sink) = self.telemetry.load().as_ref() {
                        sink.on_supervisor_decision(&crate::telemetry::SupervisorDecisionMade {
                            failed_id: &entry.failed_id,
                            reason: entry.reason.as_str(),
                            action: crate::telemetry::SupervisorAction::Deferred,
                        });
                    }
                    keep.push(DeferredSupervisorDecision {
                        // A later Defer can shorten/lengthen the
                        // deadline; if unspecified, retain the
                        // original.
                        retry_after: retry_after.or(entry.retry_after),
                        ..entry
                    });
                }
            }
        }
        if !keep.is_empty() {
            self.pending_supervisor_decisions.borrow_mut().extend(keep);
        }
    }

    fn make_group_join(&self, key: NodeKey, node_id: String) -> Rc<dyn Fn(&str)> {
        let groups = Rc::clone(&self.groups);
        let sink = self.remote_group_sink.read().clone();
        Rc::new(move |group: &str| {
            // Idempotent at the registry — re-joins are
            // collapsed. Fire the sink only on a true state
            // transition (was-not-member, now-is-member) so
            // peers don't see a stream of redundant `PgJoin`
            // packets every render.
            let mut reg = groups.borrow_mut();
            let was_member = reg.members(group).contains(&key);
            reg.join(group.to_string(), key);
            drop(reg);
            if !was_member {
                if let Some(sink) = sink.as_ref() {
                    sink(GroupOp::Join, group, &node_id);
                }
            }
        })
    }

    fn make_group_leave(&self, key: NodeKey, node_id: String) -> Rc<dyn Fn(&str)> {
        let groups = Rc::clone(&self.groups);
        let sink = self.remote_group_sink.read().clone();
        Rc::new(move |group: &str| {
            let mut reg = groups.borrow_mut();
            let was_member = reg.members(group).contains(&key);
            reg.leave(group, key);
            drop(reg);
            if was_member {
                if let Some(sink) = sink.as_ref() {
                    sink(GroupOp::Leave, group, &node_id);
                }
            }
        })
    }

    // ---- ctx mutation factories (push to pending_mutations) ----

    fn make_spawn_child(&self, parent_id: String) -> Rc<dyn Fn(ComposeChildSpec)> {
        let pending = Rc::clone(&self.pending_mutations);
        Rc::new(move |spec: ComposeChildSpec| {
            pending.borrow_mut().push(PendingMutation::SpawnChild {
                spec,
                parent_id: parent_id.clone(),
            });
        })
    }

    fn make_unmount_self(&self, id: String) -> Rc<dyn Fn()> {
        let pending = Rc::clone(&self.pending_mutations);
        Rc::new(move || {
            pending
                .borrow_mut()
                .push(PendingMutation::UnmountNode { id: id.clone() });
        })
    }

    fn make_compose(&self, parent_id: String) -> Rc<dyn Fn(Vec<ComposeChildSpec>)> {
        let pending = Rc::clone(&self.pending_mutations);
        Rc::new(move |specs: Vec<ComposeChildSpec>| {
            pending.borrow_mut().push(PendingMutation::Compose {
                parent_id: parent_id.clone(),
                specs,
            });
        })
    }

    /// Phase-9 edge negotiation factory. Defers the edge addition to the
    /// next `drain_pending_mutations` so callers from inside an
    /// evaluation pass don't try to mutate the graph while it's borrowed.
    fn make_accept_edge(&self, from_id: String) -> Rc<dyn Fn(&str)> {
        let pending = Rc::clone(&self.pending_mutations);
        Rc::new(move |target_id: &str| {
            pending.borrow_mut().push(PendingMutation::AcceptEdge {
                from: from_id.clone(),
                to: target_id.to_string(),
            });
        })
    }

    /// M12.3a-followon-F: install-the-active-node's edge-accept predicate
    /// factory. Captures the active `NodeInstance` Rc so the hook can
    /// write the predicate into its slot during body evaluation. The Rc
    /// is held for the lifetime of the `HookCtx`, which is dropped at
    /// the end of `evaluate_node`/`evaluate_node_async` — no scheduler-
    /// scoped cycle.
    fn make_set_edge_accept_handler(
        &self,
        key: NodeKey,
    ) -> Rc<dyn Fn(crate::runtime::node_instance::EdgeAcceptPredicate)> {
        let node_rc = self.nodes.get(key).cloned();
        Rc::new(
            move |predicate: crate::runtime::node_instance::EdgeAcceptPredicate| {
                if let Some(node_rc) = &node_rc {
                    node_rc.borrow_mut().edge_accept_predicate = Some(predicate);
                }
            },
        )
    }

    /// Drain pending mutations and apply them. Called per cascade round
    /// in `process_batch` and `process_batch_async` so deferred
    /// `ctx.spawnChild` / `ctx.unmount` / `ctx.compose` calls land before
    /// the next pass observes them.
    fn drain_pending_mutations(&mut self) {
        loop {
            let mutations: Vec<PendingMutation> =
                std::mem::take(&mut *self.pending_mutations.borrow_mut());
            if mutations.is_empty() {
                break;
            }
            for mutation in mutations {
                match mutation {
                    PendingMutation::SpawnChild { spec, parent_id: _ } => {
                        if !self.has_node(&spec.id) {
                            let _ = self.add_node_at_runtime(
                                spec.definition,
                                spec.id,
                                spec.props,
                                spec.connect_to,
                            );
                        }
                    }
                    PendingMutation::UnmountNode { id } => {
                        let _ = self.remove_node_at_runtime(&id);
                    }
                    PendingMutation::Compose { parent_id, specs } => {
                        // Stage 3: replace `composed_children` and spawn
                        // any missing IDs. Diffing (unmount removed
                        // children) is a stage-4 follow-up.
                        if let Some(&parent_key) = self.id_to_key.get(&parent_id) {
                            if let Some(parent_node) = self.nodes.get(parent_key) {
                                let refs: Vec<crate::runtime::types::ChildRef> = specs
                                    .iter()
                                    .map(|s| crate::runtime::types::ChildRef {
                                        id: s.id.clone(),
                                        component: s.definition.name.clone(),
                                    })
                                    .collect();
                                parent_node.borrow_mut().composed_children = refs;
                            }
                        }
                        for spec in specs {
                            if self.has_node(&spec.id) {
                                continue;
                            }
                            let mut connect_to = spec.connect_to.clone();
                            if !connect_to.iter().any(|p| p == &parent_id) {
                                connect_to.push(parent_id.clone());
                            }
                            let _ = self.add_node_at_runtime(
                                spec.definition,
                                spec.id,
                                spec.props,
                                connect_to,
                            );
                        }
                    }
                    PendingMutation::AcceptEdge { from, to } => {
                        // Phase-9 edge negotiation. No-op if either
                        // endpoint is missing or the edge already exists
                        // — matches the TS reference's `addEdge`
                        // tolerance for re-acceptance.
                        //
                        // M12.3a-followon-F: if the target has installed an
                        // edge-accept predicate via `use_edge_accept_handler`,
                        // consult it before committing. Phase-9 is channel-
                        // shaped (no msgType on the mutation), so the
                        // predicate is invoked with the empty string for
                        // msgType — predicates that care about msgType-
                        // specific decisions should rely on Tier 3 query-
                        // time evaluation instead. Predicates not
                        // registered fall through to default-accept (the
                        // pre-followon-F behavior).
                        let predicate = self.edge_accept_predicates.borrow().get(&to).cloned();
                        if let Some(p) = predicate {
                            if !p(&from, "") {
                                continue;
                            }
                        }
                        let mut g = self.graph.borrow_mut();
                        if g.has_node(&from) && g.has_node(&to) {
                            let already = g.has_edge(&from, &to, None);
                            if !already {
                                let _ = g.add_edge(
                                    from.clone(),
                                    to.clone(),
                                    None,
                                    crate::graph::Attrs::new(),
                                );
                                drop(g);
                                // Re-evaluate both endpoints — their
                                // `useNeighbors` snapshots are now stale.
                                if let Some(&from_key) = self.id_to_key.get(&from) {
                                    self.mark_dirty(from_key);
                                }
                                if let Some(&to_key) = self.id_to_key.get(&to) {
                                    self.mark_dirty(to_key);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Whether `event` should fire on a handler attached to `receiver_id`.
fn matches_filter_for(
    graph: &CommunicationGraph,
    receiver_id: &str,
    event: &TopologyEvent,
    options: Option<&crate::runtime::types::TopologyFilterOptions>,
) -> bool {
    let Some(opts) = options else {
        return true;
    };
    if let Some(kinds) = &opts.kinds {
        if !kinds.iter().any(|k| *k == TopologyEventKind::of(event)) {
            return false;
        }
    }
    if opts.neighbors_only {
        let receiver_neighbors = graph.neighbors(receiver_id).unwrap_or_default();
        let touched_ids = touched_node_ids(event);
        // Event is "near" the receiver if any of its referenced node IDs
        // is the receiver itself or one of its current neighbors.
        let near = touched_ids
            .iter()
            .any(|id| id == receiver_id || receiver_neighbors.iter().any(|n| n == id));
        if !near {
            return false;
        }
    }
    true
}

fn touched_node_ids(event: &TopologyEvent) -> Vec<String> {
    match event {
        TopologyEvent::NodeAdded { node_id } | TopologyEvent::NodeRemoved { node_id } => {
            vec![node_id.clone()]
        }
        TopologyEvent::EdgeAdded { u, v, .. }
        | TopologyEvent::EdgeRemoved { u, v, .. }
        | TopologyEvent::EdgeUpdated { u, v, .. } => vec![u.clone(), v.clone()],
    }
}

/// Cross-thread bridge from source push to the scheduler.
///
/// The source's `start(push)` runs on whatever runtime the source
/// chose (tokio task for timer/delay/http, fs-watch thread for watch).
/// Push events go directly into the scheduler's `external_inbox` —
/// the same `Arc<Mutex<Vec<Message>>>` cross-thread queue that
/// `SchedulerHandle::send_external_message` uses. The next scheduler
/// round drains the inbox into the local message queue.
///
/// Compared to the prior LocalSet-based design, this removes the
/// `spawn_local` requirement and lets sources work in any scheduler
/// entry (sync `processBatch`, async `runBatchRound`, daemon mode)
/// without a runtime-context dance. The trade-off is one mutex
/// acquisition per source push instead of an mpsc channel send;
/// negligible compared to typical source rates.
struct SourceBridge {
    inbox: Arc<Mutex<Vec<Message>>>,
    notify: Arc<tokio::sync::Notify>,
    target: String,
    /// Cross-thread wake-up — invoked after every push so the host
    /// (e.g., the napi bridge running on V8) gets a tick to drain the
    /// external inbox. `None` if no host installed one (pure-Rust
    /// daemon hosts rely on `notify` instead).
    wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl SourceBridge {
    fn new(
        inbox: Arc<Mutex<Vec<Message>>>,
        notify: Arc<tokio::sync::Notify>,
        target: String,
        wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self {
            inbox,
            notify,
            target,
            wake,
        }
    }

    fn into_push(self) -> std::sync::Arc<dyn Fn(&str, Value) + Send + Sync> {
        let inbox = Arc::clone(&self.inbox);
        let notify = Arc::clone(&self.notify);
        let target = self.target;
        let wake = self.wake;
        std::sync::Arc::new(move |r#type: &str, payload: Value| {
            let msg = create_message(r#type, "__source__", target.clone(), payload, None);
            inbox.lock().push(msg);
            notify.notify_one();
            if let Some(w) = wake.as_ref() {
                (w)();
            }
        })
    }
}

/// `Send + Sync` cross-thread handle. Lets external tasks push
/// messages into the scheduler and signal it to stop without holding a
/// borrow on the (`!Send`) [`Scheduler`] itself.
///
/// Acquired via [`Scheduler::handle`].
#[derive(Clone)]
pub struct SchedulerHandle {
    inbox: Arc<Mutex<Vec<Message>>>,
    notify: Arc<tokio::sync::Notify>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    /// **M14.5.** Remote-demonitor cleanup queue. Pushed to from
    /// dist worker threads on peer disconnect / inbound `Demonitor`
    /// packet; drained on the scheduler thread at the head of
    /// `process_batch`.
    pending_remote_demonitors: Arc<Mutex<Vec<MonitorRef>>>,
    /// **M14.5.** Remote-unlink cleanup queue — observer `NodeKey`s
    /// whose link to the shared sentinel should be removed once
    /// their per-peer remote-link count drops to zero.
    pending_remote_unlinks: Arc<Mutex<Vec<NodeKey>>>,
    /// **Phase γ (M14.11).** Cross-thread queue of inbound
    /// delegated-restart requests. Pushed by the dist crate's
    /// `dispatch_inbound`; drained at the head of each
    /// `process_batch` via
    /// [`Scheduler::drain_pending_delegated_restarts`].
    pending_delegated_restarts: Arc<Mutex<Vec<DelegatedRestartRequest>>>,
    /// **Phase γ (M14.11).** Cross-thread queue of outcomes from
    /// completed delegated-restart drains. Drained from the dist
    /// crate's per-connection task via
    /// [`Self::poll_delegated_restart_outcomes`].
    pending_delegated_restart_outcomes: Arc<Mutex<Vec<DelegatedRestartOutcome>>>,
}

impl SchedulerHandle {
    /// Inject a message destined for `target_id`. The scheduler picks
    /// it up at the start of its next batch.
    pub fn send(&self, target_id: impl Into<String>, r#type: impl Into<String>, payload: Value) {
        let msg = create_message(r#type, "__external__", target_id, payload, None);
        self.inbox.lock().push(msg);
        self.notify.notify_one();
    }

    /// Thread-safe durable-run injection counterpart to
    /// [`Scheduler::send_durable`].
    pub fn send_durable(
        &self,
        target_id: impl Into<String>,
        r#type: impl Into<String>,
        payload: Value,
        run_id: impl Into<String>,
        attempt_id: impl Into<String>,
    ) {
        let mut msg = create_message(r#type, "__external__", target_id, payload, None);
        msg.id = uuid::Uuid::now_v7().to_string();
        msg.run_id = Some(run_id.into());
        msg.attempt_id = Some(attempt_id.into());
        self.inbox.lock().push(msg);
        self.notify.notify_one();
    }

    /// **M-Ingress phase 1.** Inject a message via the HTTP-ingress path.
    /// The constructed message carries `from = "__ingress__"`, which
    /// triggers default-deny enforcement against the target node's
    /// `manifest.accepts_external` allowlist in `Scheduler::deliver`.
    ///
    /// Use this when the message is logically "external to the cluster"
    /// — e.g., `POST /v1/nodes/:id/messages` ingress dispatch from the
    /// management API. For in-process / dist / source / test injection
    /// that should bypass the allowlist, use [`Self::send`].
    pub fn send_ingress(
        &self,
        target_id: impl Into<String>,
        r#type: impl Into<String>,
        payload: Value,
    ) {
        let msg = create_message(r#type, "__ingress__", target_id, payload, None);
        self.inbox.lock().push(msg);
        self.notify.notify_one();
    }

    /// **M-Ingress phase 2.** Inject an HTTP-call message via the
    /// HTTP-ingress path with an attached `correlation_id`. The
    /// constructed message carries `from = "__ingress__"` (so
    /// `Scheduler::deliver` enforces the manifest's
    /// `accepts_external_call` gate, falling back to
    /// `accepts_external` for the legacy single-shot path) AND a
    /// `correlation_id: Some(id)` field that the scheduler's
    /// auto-propagation logic copies onto the first matching outgoing
    /// reply (per the manifest's `accepts_external_call` table).
    ///
    /// Used by the `POST /v1/nodes/:id/call` axum handler in the
    /// observer crate after generating a UUIDv4 server-side and
    /// registering the awaiting `oneshot::Sender` in the
    /// `PendingCalls` registry.
    pub fn send_ingress_with_correlation(
        &self,
        target_id: impl Into<String>,
        r#type: impl Into<String>,
        payload: Value,
        correlation_id: impl Into<String>,
    ) {
        let mut msg = create_message(r#type, "__ingress__", target_id, payload, None);
        msg.correlation_id = Some(correlation_id.into());
        self.inbox.lock().push(msg);
        self.notify.notify_one();
    }

    /// **M14 distribution.** Inject a fully-formed [`Message`] —
    /// preserves `from`, `ttl`, `direct`, and the timestamp. The
    /// dist crate's per-connection reader uses this to push remote-
    /// arriving messages so the receiving scheduler's normal
    /// delivery path (TTL decrement, channel-policy resolution,
    /// handler invocation) treats them identically to local emits.
    pub fn send_raw(&self, msg: Message) {
        self.inbox.lock().push(msg);
        self.notify.notify_one();
    }

    /// **M14.5 distribution.** Queue a remote-monitor's stub
    /// `MonitorRef` for removal from the local `MonitorRegistry`.
    /// Drained on the scheduler thread at the next
    /// `process_batch`. Safe to call from any thread.
    pub fn demonitor_remote(&self, mref: MonitorRef) {
        self.pending_remote_demonitors.lock().push(mref);
        self.notify.notify_one();
    }

    /// **M14.5 distribution.** Queue an observer `NodeKey` whose
    /// link to the shared remote sentinel should be torn down at
    /// the next `process_batch`. Called by the dist crate's
    /// `RemoteSupervision` once the observer's per-peer link
    /// count drops to zero. Safe to call from any thread.
    pub fn unlink_remote(&self, observer: NodeKey) {
        self.pending_remote_unlinks.lock().push(observer);
        self.notify.notify_one();
    }

    /// **M-Prod sub-track #4 Phase γ (M14.11).** Push a cross-node
    /// delegated-restart request onto the scheduler's drain queue.
    /// Drained at the head of the next `process_batch`; outcome is
    /// pushed onto the outcome queue (polled by the dist crate via
    /// [`Self::poll_delegated_restart_outcomes`]).
    ///
    /// Safe to call from any thread. Infallible — outcome is
    /// asynchronous.
    ///
    /// The drain logic preserves the existing
    /// `Supervisor::Strategy` / `RestartPolicy::should_restart`
    /// contract by going through `unmount_with_reason` →
    /// `execute_supervisor_plan` with a synthesized
    /// `ExitReason::Crashed { detail }`. Operator-driven restarts
    /// are treated as the same abnormal exit shape any local fault
    /// would produce.
    pub fn request_delegated_restart(
        &self,
        child_node_id: impl Into<String>,
        reason_code: &'static str,
        origin_peer: impl Into<String>,
        correlation_id: u64,
    ) {
        let req = DelegatedRestartRequest {
            origin_peer: origin_peer.into(),
            correlation_id,
            child_node_id: child_node_id.into(),
            reason_code,
        };
        self.pending_delegated_restarts.lock().push(req);
        self.notify.notify_one();
    }

    /// **Phase γ (M14.11).** Drain the scheduler's
    /// delegated-restart outcome queue. Called by the dist crate's
    /// per-connection task after each `process_batch` to ship
    /// `RestartDelegationResult` packets back to originators.
    pub fn poll_delegated_restart_outcomes(&self) -> Vec<DelegatedRestartOutcome> {
        std::mem::take(&mut *self.pending_delegated_restart_outcomes.lock())
    }

    /// Signal the daemon loop to stop. No-op outside daemon mode.
    pub fn stop_daemon(&self) {
        self.stop_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.notify.notify_one();
    }

    /// Direct access to the wake-up signal. Useful for sources that
    /// want to fire a wake without injecting a message (e.g. they
    /// pushed via the source bridge channel).
    #[must_use]
    pub fn notify_handle(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }
}

/// **M14 distribution helper.** Given a target string (the bare
/// local id `responder` OR the cross-node form `responder@bob`)
/// and our own cluster-unique node name (e.g. `bob`), return:
/// - `(stripped, false)` if the target's node-suffix matches our
///   own name — strip it, treat as local.
/// - `(stripped, false)` if the target has no `@` — treat as local
///   verbatim.
/// - `(target, true)` if the target's node-suffix is `<some-other>`
///   — preserve it as-is, classify as remote so `make_emit`
///   forwards to the dist crate's sink.
/// - `(target, true)` if the target has `@<anything>` and we have
///   no `node_name` set — assume the dist crate is expected to
///   handle it.
///
/// Pure function — no scheduler state. Lives near the top of the
/// scheduler module so the dist crate's tests can mirror its
/// classification rules without re-implementing them.
pub(crate) fn classify_target(target: &str, our_node: Option<&str>) -> (String, bool) {
    match target.rsplit_once('@') {
        None => (target.to_string(), false),
        Some((local, suffix)) => {
            if let Some(name) = our_node {
                if suffix == name {
                    return (local.to_string(), false);
                }
                (target.to_string(), true)
            } else {
                (target.to_string(), true)
            }
        }
    }
}

/// **M14 distribution helper.** Append `@<our_node_name>` to a bare
/// local sender id so cross-node receivers see a fully-qualified
/// `from` they can reply to. If `from` is already qualified or we
/// don't know our own name, returns it unchanged.
pub(crate) fn qualify_from(from: &str, our_node: Option<&str>) -> String {
    if from.contains('@') {
        return from.to_string();
    }
    match our_node {
        Some(name) => format!("{from}@{name}"),
        None => from.to_string(),
    }
}

fn union_find_find(parent: &mut FxHashMap<NodeKey, NodeKey>, x: NodeKey) -> NodeKey {
    let p = *parent.get(&x).unwrap_or(&x);
    if p == x {
        return x;
    }
    let root = union_find_find(parent, p);
    parent.insert(x, root);
    root
}

fn union_find_union(parent: &mut FxHashMap<NodeKey, NodeKey>, a: NodeKey, b: NodeKey) {
    let ra = union_find_find(parent, a);
    let rb = union_find_find(parent, b);
    if ra != rb {
        parent.insert(ra, rb);
    }
}

/// **M16-Backend P4.** Format a [`SupervisorKey`] as a stable string
/// (`"sup:0"`, `"sup:1"`, etc.) for management API output. Uses the
/// SlotMap key's `Debug` representation as the source of truth +
/// strips the `KeyData(...)` wrapper.
fn format_supervisor_key(key: SupervisorKey) -> String {
    // SlotMap keys impl `Debug` as `SupervisorKey(KeyData(N))`. We
    // want `"sup:N"` for management API consumers. Extract the
    // numeric portion via the underlying `as_ffi()` accessor —
    // returns a u64 with version + index packed; mask the low 32
    // bits to get the slot index.
    use slotmap::Key;
    let ffi = key.data().as_ffi();
    // Low 32 bits = slot index; high 32 bits = version. Use the
    // raw u64 so two keys that differ only in version still get
    // distinct strings (slotmap may reuse a slot index after
    // remove + re-insert; the version disambiguates).
    format!("sup:{ffi}")
}
