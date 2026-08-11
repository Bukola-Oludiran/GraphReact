//! Reactive runtime — message scheduler, hook system, lifecycle.

pub mod constants;
pub mod directory;
pub mod hooks;
pub mod lifecycle;
pub mod message;
pub mod node_instance;
pub mod permissions;
pub mod scheduler;
pub mod sources;
pub mod supervision;
pub mod transforms;
pub mod types;

pub use constants::{flags, DEFAULT_TTL, MAX_PROCESS_DEPTH};
pub use directory::{build_directory, DirectoryEntry, GraphDirectory};
pub use hooks::{
    ctx_accept_edge, ctx_compose, ctx_spawn_child, ctx_unmount_self, use_accept_edge, use_callback,
    use_compose, use_demonitor, use_edge_accept_handler, use_effect, use_effect_async, use_emit,
    use_group_join, use_group_leave, use_link, use_memo, use_message, use_monitor, use_neighbors,
    use_reducer, use_source, use_spawn_child, use_state, use_topology, use_unlink,
    use_unmount_self, Signal,
};
pub use lifecycle::{evaluate_node, evaluate_node_async, unmount_node, unmount_node_async};
pub use message::{create_message, decrement_ttl, reset_message_counter};
pub use node_instance::{
    AsyncEffectCleanup, EdgeAcceptPredicate, EffectCleanup, NodeDefinition, NodeInstance,
};
pub use permissions::{node_accepts, node_can_emit, resolve_channel_policy, ResolvedPolicy};
pub use scheduler::{
    DelegatedRestartOutcome, DelegatedRestartRequest, GroupOp, NodeKey, RemoteBroadcastSink,
    RemoteGroupSink, RemoteLinkSink, RemoteMonitorSink, RemoteSink, RemoteUnlinkSink, Scheduler,
    SchedulerHandle, SuperviseDecision, SuperviseDecisionHook, SupervisorKey, UnmountHook,
};
pub use sources::{delay_source, timer_source, EventSource, EventSourceFactory};
pub use supervision::{
    postpone, ChildId, ChildSpec, ExitReason, GroupRegistry, LinkRegistry, MonitorRef,
    MonitorRegistry, PostponeQueue, RestartPlan, RestartPolicy, RestartStrategy, RestartTracker,
    Supervisor, SupervisorError, SYSTEM_DOWN, SYSTEM_EXIT,
};
pub use transforms::{get_transform, list_transforms, register_transform};
pub use types::{
    ChannelPolicy, ChildRef, ComposeChildSpec, EmitOptions, ExternalCall, IncomingMessage, Message,
    MessageHandler, NodeContext, PermissionManifest, TopologyEventKind, TopologyFilterOptions,
    TopologyHandler, TransformFn,
};
