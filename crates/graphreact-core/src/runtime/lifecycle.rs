//! Node lifecycle — mount, update, unmount.
//!
//! Wraps each call to a node's component function with the thread-local
//! setup hooks rely on. After the component returns, runs pending effects
//! whose deps changed in this evaluation.
//!
//! Two flush paths exist:
//!
//! * [`flush_effects`] — synchronous. Runs `EffectCallback::Sync` slots and
//!   their sync cleanups. Async slots stay pending (their deps don't commit)
//!   until an async flush picks them up.
//! * [`flush_effects_async`] — runs both kinds, awaiting Async ones. Mirrors
//!   the TS Phase 6 design: async is opt-in via `processBatchAsync`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::runtime::constants::flags;
use crate::runtime::hooks::{set_current_node, set_hook_ctx, HookCtx};
use crate::runtime::node_instance::{
    AsyncEffectCleanup, CleanupCallback, EffectCallback, EffectCleanup, HookSlot, NodeInstance,
};
use crate::runtime::types::NodeContext;

/// Evaluate (mount or update) a node. Sets up thread-locals, runs the
/// component function, applies the returned manifest, then flushes the
/// sync effects.
///
/// `HOOK_CTX` and `CURRENT_NODE` stay populated through `flush_effects`
/// so hooks that read the active context (`use_emit`, `ctx.spawnChild`
/// via `ctx_*` helpers) can be called from inside effect callbacks.
/// They're cleared once the entire evaluation pass completes.
pub fn evaluate_node(node_rc: &Rc<RefCell<NodeInstance>>, ctx: NodeContext, hook_ctx: HookCtx) {
    install_ctx(node_rc, hook_ctx);
    let _context_guard = HookContextGuard;
    run_component_body(node_rc, ctx);
    flush_effects(node_rc);
}

/// Evaluate a node and await its async effects. Async slots that resolved
/// during the await commit their deps; sync slots are flushed inline.
pub async fn evaluate_node_async(
    node_rc: &Rc<RefCell<NodeInstance>>,
    ctx: NodeContext,
    hook_ctx: HookCtx,
) {
    install_ctx(node_rc, hook_ctx);
    let _context_guard = HookContextGuard;
    run_component_body(node_rc, ctx);
    flush_effects_async(node_rc).await;
}

fn install_ctx(node_rc: &Rc<RefCell<NodeInstance>>, hook_ctx: HookCtx) {
    set_current_node(Some(Rc::clone(node_rc)));
    set_hook_ctx(Some(hook_ctx));
}

fn clear_ctx() {
    set_hook_ctx(None);
    set_current_node(None);
}

/// Ensures validation errors or component panics cannot leak one node's hook
/// context into a later activation on the same scheduler thread.
struct HookContextGuard;

impl Drop for HookContextGuard {
    fn drop(&mut self) {
        clear_ctx();
    }
}

fn run_component_body(node_rc: &Rc<RefCell<NodeInstance>>, ctx: NodeContext) {
    {
        let mut node = node_rc.borrow_mut();
        node.clear_flag(flags::DIRTY);
        node.set_flag(flags::PROCESSING);
        node.reset_handlers();
        node.pending_effects.clear();
    }

    // Snapshot the component closure outside any borrow so the user code
    // can re-borrow the cell freely (hooks need mutable access).
    let component = node_rc.borrow().definition.component.clone();
    let manifest = component(&ctx);

    let mut node = node_rc.borrow_mut();
    node.manifest = Some(manifest);
    node.clear_flag(flags::PROCESSING);
    if !node.has_flag(flags::MOUNTED) {
        node.set_flag(flags::MOUNTED);
    }
}

/// Run sync effects whose deps changed in the most recent evaluation.
/// Async effect slots are deferred — their deps don't commit until
/// [`flush_effects_async`] runs.
pub(crate) fn flush_effects(node_rc: &Rc<RefCell<NodeInstance>>) {
    // Drain pending effects so the borrow is released between callbacks.
    let pending = {
        let mut node = node_rc.borrow_mut();
        std::mem::take(&mut node.pending_effects)
    };

    let mut deferred: Vec<usize> = Vec::new();

    for index in pending {
        let (callback, prev_cleanup, deps) = {
            let mut node = node_rc.borrow_mut();
            let HookSlot::Effect(state) = &mut node.hooks[index] else {
                continue;
            };
            let cb = state.callback.take();
            let cleanup = state.cleanup.take();
            let pending_deps = state.pending_deps.take();
            (cb, cleanup, pending_deps)
        };

        let Some(callback) = callback else { continue };

        match callback {
            EffectCallback::Sync(sync_cb) => {
                run_sync_cleanup(prev_cleanup);
                let new_cleanup = sync_cb().map(CleanupCallback::Sync);
                commit_effect_slot(
                    node_rc,
                    index,
                    deps,
                    Some(EffectCallback::Sync(sync_cb)),
                    new_cleanup,
                );
            }
            EffectCallback::Async(async_cb) => {
                // Async effects don't run on the sync path. Restore the
                // callback + previous cleanup + pending deps so a later
                // async flush can pick them up.
                let mut node = node_rc.borrow_mut();
                if let HookSlot::Effect(state) = &mut node.hooks[index] {
                    state.callback = Some(EffectCallback::Async(async_cb));
                    state.cleanup = prev_cleanup;
                    state.pending_deps = deps;
                }
                deferred.push(index);
            }
        }
    }

    if !deferred.is_empty() {
        let mut node = node_rc.borrow_mut();
        node.pending_effects.extend(deferred);
    }
}

/// Run all pending effects, awaiting async ones.
pub async fn flush_effects_async(node_rc: &Rc<RefCell<NodeInstance>>) {
    let pending = {
        let mut node = node_rc.borrow_mut();
        std::mem::take(&mut node.pending_effects)
    };

    for index in pending {
        let (callback, prev_cleanup, deps) = {
            let mut node = node_rc.borrow_mut();
            let HookSlot::Effect(state) = &mut node.hooks[index] else {
                continue;
            };
            let cb = state.callback.take();
            let cleanup = state.cleanup.take();
            let pending_deps = state.pending_deps.take();
            (cb, cleanup, pending_deps)
        };

        let Some(callback) = callback else { continue };

        match callback {
            EffectCallback::Sync(sync_cb) => {
                run_sync_cleanup(prev_cleanup);
                let new_cleanup = sync_cb().map(CleanupCallback::Sync);
                commit_effect_slot(
                    node_rc,
                    index,
                    deps,
                    Some(EffectCallback::Sync(sync_cb)),
                    new_cleanup,
                );
            }
            EffectCallback::Async(async_cb) => {
                run_cleanup_awaiting_async(prev_cleanup).await;
                let fut = async_cb();
                let new_cleanup = fut.await.map(CleanupCallback::Async);
                commit_effect_slot(
                    node_rc,
                    index,
                    deps,
                    Some(EffectCallback::Async(async_cb)),
                    new_cleanup,
                );
            }
        }
    }
}

fn commit_effect_slot(
    node_rc: &Rc<RefCell<NodeInstance>>,
    index: usize,
    deps: Option<Vec<serde_json::Value>>,
    callback: Option<EffectCallback>,
    cleanup: Option<CleanupCallback>,
) {
    let mut node = node_rc.borrow_mut();
    if let HookSlot::Effect(state) = &mut node.hooks[index] {
        state.callback = callback;
        state.committed_deps = deps;
        state.cleanup = cleanup;
    }
}

fn run_sync_cleanup(cleanup: Option<CleanupCallback>) {
    match cleanup {
        Some(CleanupCallback::Sync(c)) => c(),
        // Async cleanup lingers across a sync flush — it'll run on the next
        // async flush or on async unmount.
        Some(CleanupCallback::Async(_)) | None => {}
    }
}

async fn run_cleanup_awaiting_async(cleanup: Option<CleanupCallback>) {
    match cleanup {
        Some(CleanupCallback::Sync(c)) => c(),
        Some(CleanupCallback::Async(c)) => {
            let fut = c();
            fut.await;
        }
        None => {}
    }
}

/// Unmount a node. Stops sources, runs all sync effect cleanups, sets the
/// `UNMOUNTED` bit. Async cleanups linger until [`unmount_node_async`] runs.
pub fn unmount_node(node_rc: &Rc<RefCell<NodeInstance>>) {
    stop_sources(node_rc);

    // Drain effect cleanups; async ones get dropped (best-effort fire-and-forget
    // matching TS sync `cleanupEffects` behavior — async needs `unmount_node_async`).
    let cleanups: Vec<EffectCleanup> = {
        let mut node = node_rc.borrow_mut();
        node.hooks
            .iter_mut()
            .filter_map(|slot| {
                if let HookSlot::Effect(state) = slot {
                    match state.cleanup.take() {
                        Some(CleanupCallback::Sync(c)) => Some(c),
                        Some(CleanupCallback::Async(_)) | None => None,
                    }
                } else {
                    None
                }
            })
            .collect()
    };
    for cleanup in cleanups {
        cleanup();
    }

    let mut node = node_rc.borrow_mut();
    node.set_flag(flags::UNMOUNTED);
}

enum AnyCleanup {
    Sync(EffectCleanup),
    Async(AsyncEffectCleanup),
}

/// Async unmount — awaits async cleanups in addition to running sync ones.
pub async fn unmount_node_async(node_rc: &Rc<RefCell<NodeInstance>>) {
    stop_sources(node_rc);

    let cleanups: Vec<AnyCleanup> = {
        let mut node = node_rc.borrow_mut();
        node.hooks
            .iter_mut()
            .filter_map(|slot| {
                if let HookSlot::Effect(state) = slot {
                    state.cleanup.take().map(|c| match c {
                        CleanupCallback::Sync(c) => AnyCleanup::Sync(c),
                        CleanupCallback::Async(c) => AnyCleanup::Async(c),
                    })
                } else {
                    None
                }
            })
            .collect()
    };
    for cleanup in cleanups {
        match cleanup {
            AnyCleanup::Sync(c) => c(),
            AnyCleanup::Async(c) => {
                let fut = c();
                fut.await;
            }
        }
    }

    let mut node = node_rc.borrow_mut();
    node.set_flag(flags::UNMOUNTED);
}

fn stop_sources(node_rc: &Rc<RefCell<NodeInstance>>) {
    let sources: Vec<std::sync::Arc<dyn crate::runtime::sources::EventSource>> = {
        let node = node_rc.borrow();
        node.active_sources.values().cloned().collect()
    };
    for source in sources {
        source.stop();
    }
    let mut node = node_rc.borrow_mut();
    node.active_sources.clear();
}
