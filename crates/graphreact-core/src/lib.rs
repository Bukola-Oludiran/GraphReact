//! GraphReact core runtime.
//!
//! Provides the [`graph::CommunicationGraph`] data structure, the
//! message-driven scheduler, React-style hooks, OTP-inspired supervision
//! (M5), and a Leptos-inspired reactive derivation layer (M7).
//!
//! M2 shipped the graph layer; M3 shipped the algorithms; M4 ships the
//! reactive runtime — node instances, hooks, scheduler, lifecycle.

#![allow(clippy::module_inception)]

pub mod error;
pub mod graph;
pub mod reactive;
pub mod runtime;
pub mod telemetry;

pub use reactive::{Memo, ReactiveNode, ReactiveNodeState, ReactiveSignal};

pub use telemetry::{
    CallReply, HotReload, MessageDelivered, NodeEvaluateEnd, NodeEvaluateStart, NodeMounted,
    NodeUnmounted, QueueDepths, QueueDepthsSnapshot, RemoteDirection, RemoteMessage,
    SchedulerTickEnd, SchedulerTickStart, SupervisorAction, SupervisorDecisionMade, TelemetrySink,
    TelemetrySinkRef, TopologyEventOccurred,
};

pub use error::{GraphError, RuntimeError};
pub use graph::{
    accept_all_edges, accept_all_nodes, attrs_from, from_json, subgraph_by_nodes, subgraph_view,
    to_graphml, to_json, Attrs, CommunicationGraph, EdgeFilter, EdgeSlot, EdgeTuple, JsonEdge,
    JsonGraph, JsonNode, NodeFilter, NodeKey as GraphNodeKey, SubgraphView, Subscription,
    TopologyEvent,
};
pub use runtime::{
    build_directory, create_message, ctx_compose, ctx_spawn_child, ctx_unmount_self, decrement_ttl,
    delay_source, evaluate_node, evaluate_node_async, flags, get_transform, list_transforms,
    node_accepts, node_can_emit, postpone, register_transform, reset_message_counter,
    resolve_channel_policy, timer_source, unmount_node, unmount_node_async, use_accept_edge,
    use_callback, use_compose, use_demonitor, use_durable_artifact, use_durable_cancellation,
    use_durable_effect, use_durable_state, use_durable_timer, use_edge_accept_handler, use_effect,
    use_effect_async, use_emit, use_group_join, use_group_leave, use_link, use_memo, use_message,
    use_monitor, use_neighbors, use_reducer, use_source, use_spawn_child, use_state, use_topology,
    use_unlink, use_unmount_self, AsyncEffectCleanup, ChannelPolicy, ChildId, ChildRef, ChildSpec,
    ComposeChildSpec, DelegatedRestartOutcome, DelegatedRestartRequest, DirectoryEntry,
    DurabilityMode, DurableActivation, DurableActivationCommitter, DurableArtifactIntent,
    DurableArtifactRequest, DurableEffectIntent, DurableEffectRequest, DurableQueueSnapshot,
    DurableReplayPolicy, DurableSignal, DurableStateSnapshot, DurableTimerCommand,
    DurableTimerIntent, DurableTimerRequest, EdgeAcceptPredicate, EffectCleanup, EmitOptions,
    EventSource, EventSourceFactory, ExitReason, ExternalCall, GraphDirectory, GroupOp,
    GroupRegistry, IncomingMessage, LinkRegistry, Message, MessageHandler, MonitorRef,
    MonitorRegistry, NodeContext, NodeDefinition, NodeInstance, NodeKey, PermissionManifest,
    PostponeQueue, RemoteBroadcastSink, RemoteGroupSink, RemoteLinkSink, RemoteMonitorSink,
    RemoteSink, RemoteUnlinkSink, ResolvedPolicy, RestartPlan, RestartPolicy, RestartStrategy,
    RestartTracker, Scheduler, SchedulerHandle, Signal, SuperviseDecision, SuperviseDecisionHook,
    Supervisor, SupervisorError, SupervisorKey, TopologyEventKind, TopologyFilterOptions,
    TopologyHandler, TransformFn, UnmountHook, DEFAULT_TTL, MAX_PROCESS_DEPTH, SYSTEM_DOWN,
    SYSTEM_EXIT,
};
