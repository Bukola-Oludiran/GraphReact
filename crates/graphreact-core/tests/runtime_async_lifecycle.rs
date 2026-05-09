//! Async lifecycle (M4.5) — `use_effect_async`, `flush_effects_async`,
//! `evaluate_node_async`, `unmount_node_async`, `process_batch_async`.
//!
//! Mirrors TS Phase 6: async is opt-in, not a sync replacement.

use graphreact_core::{
    use_effect, use_effect_async, Attrs, CommunicationGraph, NodeContext, NodeDefinition,
    PermissionManifest, Scheduler,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::LocalSet;

#[test]
fn async_effect_awaited_during_async_evaluation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let local = LocalSet::new();

    let ran = Arc::new(AtomicU32::new(0));
    let ran_clone = Arc::clone(&ran);

    local.block_on(&runtime, async move {
        let component = move |_ctx: &NodeContext| {
            let ran = Arc::clone(&ran_clone);
            use_effect_async(
                move || {
                    let ran = Arc::clone(&ran);
                    async move {
                        // Yield once so we can confirm the runtime awaits.
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        ran.fetch_add(1, Ordering::SeqCst);
                        None
                    }
                },
                vec![],
            );
            PermissionManifest::default()
        };

        let mut graph = CommunicationGraph::new();
        graph.add_node("n", Attrs::new());
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
            .unwrap();
        scheduler.start_async().await.unwrap();

        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "async effect must run during process_batch_async"
        );
    });
}

#[test]
fn async_effect_skipped_on_pure_sync_path() {
    // `start()` + `process_batch()` won't run async effects. They're
    // strictly opt-in via the async path. Mixing modes (sync start, async
    // batch later) is intentionally undefined; users commit to one path.
    let ran = Arc::new(AtomicU32::new(0));
    let ran_clone = Arc::clone(&ran);

    let component = move |_ctx: &NodeContext| {
        let ran = Arc::clone(&ran_clone);
        use_effect_async(
            move || {
                let ran = Arc::clone(&ran);
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    None
                }
            },
            vec![],
        );
        PermissionManifest::default()
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("n", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();
    scheduler.process_batch().unwrap();

    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "async effects must not run on the pure sync path"
    );
}

#[test]
fn async_cleanup_awaited_on_unmount_async() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let local = LocalSet::new();

    let cleaned = Arc::new(AtomicU32::new(0));
    let cleaned_clone = Arc::clone(&cleaned);

    local.block_on(&runtime, async move {
        let component = move |_ctx: &NodeContext| {
            let cleaned = Arc::clone(&cleaned_clone);
            use_effect_async(
                move || {
                    let cleaned = Arc::clone(&cleaned);
                    async move {
                        let cleanup: graphreact_core::AsyncEffectCleanup = Box::new(move || {
                            let cleaned = Arc::clone(&cleaned);
                            Box::pin(async move {
                                tokio::time::sleep(Duration::from_millis(5)).await;
                                cleaned.fetch_add(1, Ordering::SeqCst);
                            })
                                as std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>
                        });
                        Some(cleanup)
                    }
                },
                vec![],
            );
            PermissionManifest::default()
        };

        let mut graph = CommunicationGraph::new();
        graph.add_node("n", Attrs::new());
        let mut scheduler = Scheduler::new(graph);
        let key = scheduler
            .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
            .unwrap();
        scheduler.start_async().await.unwrap();

        scheduler.unmount_async(key).await;
        assert_eq!(
            cleaned.load(Ordering::SeqCst),
            1,
            "async cleanup must run during unmount_async"
        );
    });
}

#[test]
fn process_batch_async_runs_independent_groups_in_parallel() {
    // Three leaves with no shared edges and a 50ms async effect each.
    // Sequential would be ≥150ms; parallel-via-join_all should be ~50ms.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let local = LocalSet::new();

    let started: Arc<parking_lot::Mutex<Vec<(String, Instant)>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));

    local.block_on(&runtime, async move {
        let make_leaf =
            |id: &'static str, started: Arc<parking_lot::Mutex<Vec<(String, Instant)>>>| {
                let started = Arc::clone(&started);
                move |_ctx: &NodeContext| {
                    let started = Arc::clone(&started);
                    let id = id.to_string();
                    use_effect_async(
                        move || {
                            let started = Arc::clone(&started);
                            let id = id.clone();
                            async move {
                                started.lock().push((id, Instant::now()));
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                None
                            }
                        },
                        vec![],
                    );
                    PermissionManifest::default()
                }
            };

        let mut graph = CommunicationGraph::new();
        graph.add_node("a", Attrs::new());
        graph.add_node("b", Attrs::new());
        graph.add_node("c", Attrs::new());
        // No edges — all three are independent groups.
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(
                NodeDefinition::new("A", make_leaf("a", Arc::clone(&started))),
                "a",
                Attrs::new(),
                0,
            )
            .unwrap();
        scheduler
            .register_node(
                NodeDefinition::new("B", make_leaf("b", Arc::clone(&started))),
                "b",
                Attrs::new(),
                0,
            )
            .unwrap();
        scheduler
            .register_node(
                NodeDefinition::new("C", make_leaf("c", Arc::clone(&started))),
                "c",
                Attrs::new(),
                0,
            )
            .unwrap();

        let begin = Instant::now();
        scheduler.start_async().await.unwrap();
        let elapsed = begin.elapsed();

        // Sequential would be ≥150ms; parallel should be well under 100ms
        // even with scheduler overhead. Generous margin.
        assert!(
            elapsed < Duration::from_millis(120),
            "process_batch_async should run independent groups in parallel \
             (elapsed = {elapsed:?}, sequential would be ≥150ms)"
        );

        // All three started before any of them finished — confirms
        // concurrent kickoff, not staggered sequential.
        let starts = started.lock().clone();
        assert_eq!(starts.len(), 3);
        let last_start = starts.iter().map(|(_, t)| *t).max().unwrap();
        let first_start = starts.iter().map(|(_, t)| *t).min().unwrap();
        let kickoff_spread = last_start.duration_since(first_start);
        assert!(
            kickoff_spread < Duration::from_millis(20),
            "all three leaves should start within ~20ms of each other \
             (spread = {kickoff_spread:?})"
        );
    });
}

#[test]
fn start_async_runs_fan_in_siblings_in_parallel_despite_shared_parent() {
    // Fan-in tree: root with three children, all connected by edges to
    // root (the typical project-DAG shape). At initial mount the older
    // union-find grouping put all four nodes in one transitive group
    // (because root connects them), serializing the leaves. The fix puts
    // each node in its own group at start_async — so the three leaves'
    // 50ms async effects run concurrently, not sequentially.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let local = LocalSet::new();

    local.block_on(&runtime, async move {
        let leaf = move |_ctx: &NodeContext| {
            use_effect_async(
                move || async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    None
                },
                vec![],
            );
            PermissionManifest::default()
        };
        // Root has no work — just sits there as a topology anchor.
        let root_def = move |_ctx: &NodeContext| PermissionManifest::default();

        let mut graph = CommunicationGraph::new();
        graph.add_node("root", Attrs::new());
        graph.add_node("a", Attrs::new());
        graph.add_node("b", Attrs::new());
        graph.add_node("c", Attrs::new());
        graph.add_edge("root", "a", None, Attrs::new());
        graph.add_edge("root", "b", None, Attrs::new());
        graph.add_edge("root", "c", None, Attrs::new());

        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(
                NodeDefinition::new("Root", root_def),
                "root",
                Attrs::new(),
                0,
            )
            .unwrap();
        scheduler
            .register_node(NodeDefinition::new("Leaf", leaf), "a", Attrs::new(), 1)
            .unwrap();
        scheduler
            .register_node(NodeDefinition::new("Leaf", leaf), "b", Attrs::new(), 1)
            .unwrap();
        scheduler
            .register_node(NodeDefinition::new("Leaf", leaf), "c", Attrs::new(), 1)
            .unwrap();

        let begin = Instant::now();
        scheduler.start_async().await.unwrap();
        let elapsed = begin.elapsed();

        // Sequential (old behavior): 3 × 50ms = 150ms.
        // Parallel siblings: ~50ms + scheduler overhead.
        // Generous bound: should be well under 100ms.
        assert!(
            elapsed < Duration::from_millis(100),
            "fan-in siblings must parallelize at initial mount despite \
             sharing edges with a common parent (elapsed = {elapsed:?}, \
             sequential would be ≥150ms)"
        );
    });
}

// NOTE: the old "connected dirty nodes are sequential within a group" test
// was removed because start_async now uses per-node grouping (initial-mount
// parallelism fix) — connected nodes parallelize at mount. The cascade-
// round behavior in process_batch_async still uses edge-aware union-find
// grouping, but verifying that requires a more elaborate setup that
// re-dirties already-mounted nodes via state changes (TODO: add a cascade-
// sequential test once the M6 JS bridge is wired and we can drive setState
// from the test harness more cleanly).

#[test]
fn sync_effects_still_work_on_async_path() {
    // Mixed sync + async effects on the same node — async path runs both.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let local = LocalSet::new();

    let sync_ran = Rc::new(Cell::new(0u32));
    let async_ran = Arc::new(AtomicU32::new(0));

    let sync_clone = Rc::clone(&sync_ran);
    let async_clone = Arc::clone(&async_ran);

    local.block_on(&runtime, async move {
        let component = move |_ctx: &NodeContext| {
            let sync_ran = Rc::clone(&sync_clone);
            use_effect(
                move || {
                    sync_ran.set(sync_ran.get() + 1);
                    None
                },
                vec![],
            );
            let async_ran = Arc::clone(&async_clone);
            use_effect_async(
                move || {
                    let async_ran = Arc::clone(&async_ran);
                    async move {
                        async_ran.fetch_add(1, Ordering::SeqCst);
                        None
                    }
                },
                vec![],
            );
            PermissionManifest::default()
        };

        let mut graph = CommunicationGraph::new();
        graph.add_node("n", Attrs::new());
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
            .unwrap();
        scheduler.start_async().await.unwrap();

        assert_eq!(sync_ran.get(), 1, "sync effect runs on async path");
        assert_eq!(
            async_ran.load(Ordering::SeqCst),
            1,
            "async effect runs on async path"
        );
    });
}
