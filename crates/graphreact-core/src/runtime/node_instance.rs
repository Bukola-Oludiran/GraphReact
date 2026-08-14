//! `NodeInstance` — the runtime representation of a mounted node.
//!
//! Each instance owns its hook list, message handler map, topology
//! handlers, and active event-source registrations. Dropped on unmount —
//! the [`generational_box::Owner`] inside ensures any user-held [`Signal`]
//! handles become safely-stale rather than dangling.

use generational_box::{AnyStorage, GenerationalBox, Owner, UnsyncStorage};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::graph::Attrs;
use crate::runtime::constants::flags;
use crate::runtime::hooks::StateCellInner;
use crate::runtime::sources::EventSource;
use crate::runtime::types::{
    ChildRef, MessageHandler, PermissionManifest, TopologyFilterOptions, TopologyHandler,
};

/// Per-node decision predicate: given `(requesting_source_id, msg_type)`,
/// return whether THIS node would accept an incoming edge / Tier 3 reach.
/// Installed via [`crate::runtime::hooks::use_edge_accept_handler`]. When
/// no predicate is installed, default behavior is "accept" (preserves
/// Phase-9 status quo). M12.3a-followon-F.
pub type EdgeAcceptPredicate = std::rc::Rc<dyn Fn(&str, &str) -> bool>;

/// User-supplied component function — the body of a node.
pub type NodeComponent =
    std::rc::Rc<dyn Fn(&crate::runtime::types::NodeContext) -> PermissionManifest>;

/// Definition of a node component — name + body.
#[derive(Clone)]
pub struct NodeDefinition {
    pub name: String,
    pub component: NodeComponent,
}

impl std::fmt::Debug for NodeDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeDefinition")
            .field("name", &self.name)
            .field("component", &"<fn>")
            .finish()
    }
}

impl NodeDefinition {
    pub fn new<S, F>(name: S, component: F) -> Self
    where
        S: Into<String>,
        F: Fn(&crate::runtime::types::NodeContext) -> PermissionManifest + 'static,
    {
        Self {
            name: name.into(),
            component: std::rc::Rc::new(component),
        }
    }
}

/// State hook slot — wraps the generational-box cell holding the value.
pub struct StateHookState {
    pub(crate) value: GenerationalBox<StateCellInner, UnsyncStorage>,
}

/// JSON-compatible state cell used by the explicitly named durable-state
/// hook. Durable cells live outside the positional hook list: their stable
/// key, rather than render order, is their recovery identity.
pub struct DurableStateCellInner {
    pub(crate) value: serde_json::Value,
    pub(crate) enqueue: std::rc::Rc<dyn Fn()>,
}

/// What kind of effect callback is held in this slot. Sync effects are
/// invoked from the synchronous flush path; async effects only fire on the
/// async flush path (Phase 6 in the TS port — async is opt-in, not a sync
/// replacement).
pub enum EffectCallback {
    Sync(std::rc::Rc<dyn Fn() -> Option<EffectCleanup>>),
    Async(
        std::rc::Rc<
            dyn Fn() -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Option<AsyncEffectCleanup>>>,
            >,
        >,
    ),
}

/// Effect hook slot.
pub struct EffectHookState {
    pub(crate) callback: Option<EffectCallback>,
    pub(crate) committed_deps: Option<Vec<serde_json::Value>>,
    pub(crate) pending_deps: Option<Vec<serde_json::Value>>,
    pub(crate) cleanup: Option<CleanupCallback>,
}

/// What kind of cleanup is held — runs at the matching flush path.
pub enum CleanupCallback {
    Sync(EffectCleanup),
    Async(AsyncEffectCleanup),
}

/// Memo hook slot.
pub struct MemoHookState {
    pub(crate) value: Option<Box<dyn std::any::Any>>,
    pub(crate) deps: Option<Vec<serde_json::Value>>,
}

/// Message hook slot — declarative record of the latest handler binding.
///
/// Fields are kept for introspection and M5 hot-reload diffing; the live
/// handler list lives on `NodeInstance::message_handlers`.
#[allow(dead_code)]
pub struct MessageHookState {
    pub(crate) message_type: String,
    pub(crate) handler: MessageHandler,
}

/// Source hook slot — keeps the current source instance alive across evals.
pub struct SourceHookState {
    pub(crate) source: Option<std::sync::Arc<dyn EventSource>>,
    pub(crate) deps: Option<Vec<serde_json::Value>>,
}

/// Topology hook slot. Fields kept for introspection / M5 hot-reload.
#[allow(dead_code)]
pub struct TopologyHookState {
    pub(crate) handler: TopologyHandler,
    pub(crate) options: Option<TopologyFilterOptions>,
}

/// One slot in a node's hook list.
pub enum HookSlot {
    /// Uninitialized placeholder — used during slot pre-allocation.
    Empty,
    State(StateHookState),
    Effect(EffectHookState),
    Memo(MemoHookState),
    Message(MessageHookState),
    Source(SourceHookState),
    Topology(TopologyHookState),
}

impl HookSlot {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        matches!(self, HookSlot::Empty)
    }
}

/// Cleanup closure for a synchronous effect.
pub type EffectCleanup = Box<dyn FnOnce()>;

/// Cleanup closure for an async effect — returns a future that resolves
/// when cleanup completes.
pub type AsyncEffectCleanup =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>>;

/// Per-node runtime state.
pub struct NodeInstance {
    pub definition: NodeDefinition,
    pub id: String,
    pub props: Attrs,
    pub bits: u32,
    pub(crate) hooks: Vec<HookSlot>,
    pub(crate) durable_state:
        FxHashMap<String, GenerationalBox<DurableStateCellInner, UnsyncStorage>>,
    /// Values restored before component bodies are rebuilt. The first
    /// matching `use_durable_state` call moves the value into a live cell.
    pub(crate) restored_durable_state: FxHashMap<String, serde_json::Value>,
    /// Keys used in the current evaluation, reset with the handler maps.
    pub(crate) durable_keys_seen: FxHashSet<String>,
    pub(crate) pending_effects: Vec<usize>,
    pub(crate) message_handlers: FxHashMap<String, Vec<MessageHandler>>,
    pub(crate) topology_handlers: Vec<(TopologyHandler, Option<TopologyFilterOptions>)>,
    pub(crate) active_sources: FxHashMap<String, std::sync::Arc<dyn EventSource>>,
    pub manifest: Option<PermissionManifest>,
    /// Per-node edge-accept predicate, installed by
    /// [`crate::runtime::hooks::use_edge_accept_handler`]. Cleared at the
    /// start of every render via [`reset_handlers`]; nodes that don't call
    /// the hook leave this `None` and inherit the default-accept behavior.
    /// M12.3a-followon-F.
    pub edge_accept_predicate: Option<EdgeAcceptPredicate>,
    pub external_children: Vec<ChildRef>,
    /// Children declared via `ctx.compose(...)` — distinct from
    /// `external_children` (the static AST nesting). Diffed across
    /// evaluations so the runtime can spawn newly-listed children and
    /// unmount removed ones.
    pub composed_children: Vec<ChildRef>,
    pub(crate) owner: Owner<UnsyncStorage>,
}

impl std::fmt::Debug for NodeInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeInstance")
            .field("id", &self.id)
            .field("role", &self.definition.name)
            .field("bits", &self.bits)
            .field("hook_count", &self.hooks.len())
            .field("manifest", &self.manifest)
            .finish()
    }
}

impl NodeInstance {
    #[must_use]
    pub fn new(definition: NodeDefinition, id: String, props: Attrs) -> Self {
        Self {
            definition,
            id,
            props,
            bits: 0,
            hooks: Vec::new(),
            durable_state: FxHashMap::default(),
            restored_durable_state: FxHashMap::default(),
            durable_keys_seen: FxHashSet::default(),
            pending_effects: Vec::new(),
            message_handlers: FxHashMap::default(),
            topology_handlers: Vec::new(),
            active_sources: FxHashMap::default(),
            manifest: None,
            edge_accept_predicate: None,
            external_children: Vec::new(),
            composed_children: Vec::new(),
            owner: UnsyncStorage::owner(),
        }
    }

    #[must_use]
    pub fn has_flag(&self, flag: u32) -> bool {
        self.bits & flag != 0
    }

    pub fn set_flag(&mut self, flag: u32) {
        self.bits |= flag;
    }

    pub fn clear_flag(&mut self, flag: u32) {
        self.bits &= !flag;
    }

    #[must_use]
    pub fn is_unmounted(&self) -> bool {
        self.has_flag(flags::UNMOUNTED)
    }

    /// Reset per-evaluation handler maps. Called by the lifecycle before
    /// the component function runs.
    pub fn reset_handlers(&mut self) {
        self.message_handlers.clear();
        self.topology_handlers.clear();
        self.durable_keys_seen.clear();
        // M12.3a-followon-F: drop any predicate installed in a prior
        // render so a body that conditionally stops calling
        // `use_edge_accept_handler` reverts to default-accept.
        self.edge_accept_predicate = None;
    }

    #[must_use]
    pub fn hook_count(&self) -> usize {
        self.hooks.len()
    }

    /// Snapshot only explicitly durable JSON state. Positional hooks and
    /// callbacks are deliberately excluded from recovery data.
    #[must_use]
    pub fn durable_state_snapshot(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        let mut values: std::collections::BTreeMap<_, _> = self
            .restored_durable_state
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        values.extend(
            self.durable_state
                .iter()
                .map(|(key, cell)| (key.clone(), cell.read().value.clone())),
        );
        values
    }

    /// Install recovered durable values. Existing live cells are updated;
    /// values whose hooks have not been rebuilt yet remain staged by key.
    pub fn restore_durable_state(
        &mut self,
        values: impl IntoIterator<Item = (String, serde_json::Value)>,
    ) {
        for (key, value) in values {
            if let Some(cell) = self.durable_state.get(&key) {
                cell.write().value = value;
            } else {
                self.restored_durable_state.insert(key, value);
            }
        }
    }
}
