//! Regression coverage for `EventSource::stop()` firing on the unmount
//! path. The behavior was already correct
//! ([`crate::runtime::lifecycle::stop_sources`] iterates
//! `node.active_sources.values().stop()` from both `unmount_node` and
//! `unmount_node_async`, and `use_source` populates `active_sources` at
//! `crates/graphreact-core/src/runtime/hooks.rs:468`), but no test
//! pinned it. These do.
//!
//! Surfaced as a "tracked-but-non-blocking observation" during the
//! M12.3a-followon-A planning pass; an actual code-trace showed the
//! lifecycle path was already correct, so the slice degenerated to
//! adding the regression coverage that should have existed all along.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use graphreact_core::{
    Attrs, CommunicationGraph, EventSource, NodeContext, NodeDefinition, PermissionManifest,
    Scheduler,
};
use serde_json::Value;

/// Test-only source: counts `start` and `stop` invocations through
/// shared atomics so the tests can assert the lifecycle.
struct CountingSource {
    id: String,
    starts: Arc<AtomicU32>,
    stops: Arc<AtomicU32>,
}

impl EventSource for CountingSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self, _push: Arc<dyn Fn(&str, Value) + Send + Sync>) {
        self.starts.fetch_add(1, Ordering::SeqCst);
    }

    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn source_stop_fires_when_owning_node_unmounts() {
    let starts = Arc::new(AtomicU32::new(0));
    let stops = Arc::new(AtomicU32::new(0));
    let starts_for_factory = Arc::clone(&starts);
    let stops_for_factory = Arc::clone(&stops);

    let component = move |_ctx: &NodeContext| {
        let starts = Arc::clone(&starts_for_factory);
        let stops = Arc::clone(&stops_for_factory);
        graphreact_core::use_source(
            move || -> Arc<dyn EventSource> {
                Arc::new(CountingSource {
                    id: "counting-1".to_string(),
                    starts,
                    stops,
                })
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), Vec::<String>::new())
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("watcher", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    let key = scheduler
        .register_node(
            NodeDefinition::new("Watcher", component),
            "watcher",
            Attrs::new(),
            0,
        )
        .expect("register");

    scheduler.process_batch().expect("first batch");
    assert_eq!(starts.load(Ordering::SeqCst), 1, "source.start fired once on initial mount");
    assert_eq!(stops.load(Ordering::SeqCst), 0, "no stop yet");

    scheduler.unmount(key);
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "source.stop() fires exactly once on sync unmount"
    );
}

#[test]
fn source_stop_fires_when_use_source_deps_change_within_a_node() {
    // Re-render with new deps replaces the previous source. The prior
    // source must `stop()` before the new one is `start()`-ed.
    use std::cell::Cell;
    use std::rc::Rc;

    let starts = Arc::new(AtomicU32::new(0));
    let stops = Arc::new(AtomicU32::new(0));
    let starts_for_factory = Arc::clone(&starts);
    let stops_for_factory = Arc::clone(&stops);
    let dep = Rc::new(Cell::new(0u32));
    let dep_for_component = Rc::clone(&dep);
    let next_id = Rc::new(Cell::new(0u32));
    let next_id_for_component = Rc::clone(&next_id);

    let component = move |_ctx: &NodeContext| {
        let starts = Arc::clone(&starts_for_factory);
        let stops = Arc::clone(&stops_for_factory);
        let id_n = next_id_for_component.get();
        next_id_for_component.set(id_n + 1);
        graphreact_core::use_source(
            move || -> Arc<dyn EventSource> {
                Arc::new(CountingSource {
                    id: format!("counting-{id_n}"),
                    starts,
                    stops,
                })
            },
            vec![Value::from(dep_for_component.get())],
        );
        PermissionManifest::new(Vec::<String>::new(), Vec::<String>::new())
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("watcher", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    let key = scheduler
        .register_node(
            NodeDefinition::new("Watcher", component),
            "watcher",
            Attrs::new(),
            0,
        )
        .expect("register");

    scheduler.process_batch().expect("first batch");
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 0);

    // Bump the dep + force a re-render via mark_dirty.
    dep.set(1);
    scheduler.mark_dirty(key);
    scheduler.process_batch().expect("second batch");

    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "prior source stopped when deps changed"
    );
    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "new source started after the prior one stopped"
    );

    // Final unmount stops the surviving source.
    scheduler.unmount(key);
    assert_eq!(
        stops.load(Ordering::SeqCst),
        2,
        "current source stopped on unmount"
    );
}

#[tokio::test]
async fn source_stop_fires_on_async_unmount() {
    let starts = Arc::new(AtomicU32::new(0));
    let stops = Arc::new(AtomicU32::new(0));
    let starts_for_factory = Arc::clone(&starts);
    let stops_for_factory = Arc::clone(&stops);

    let component = move |_ctx: &NodeContext| {
        let starts = Arc::clone(&starts_for_factory);
        let stops = Arc::clone(&stops_for_factory);
        graphreact_core::use_source(
            move || -> Arc<dyn EventSource> {
                Arc::new(CountingSource {
                    id: "counting-async".to_string(),
                    starts,
                    stops,
                })
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), Vec::<String>::new())
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut graph = CommunicationGraph::new();
            graph.add_node("watcher", Attrs::new());
            let mut scheduler = Scheduler::new(graph);
            let key = scheduler
                .register_node(
                    NodeDefinition::new("Watcher", component),
                    "watcher",
                    Attrs::new(),
                    0,
                )
                .expect("register");

            scheduler.process_batch().expect("first batch");
            assert_eq!(starts.load(Ordering::SeqCst), 1);
            assert_eq!(stops.load(Ordering::SeqCst), 0);

            scheduler.unmount_async(key).await;
            assert_eq!(
                stops.load(Ordering::SeqCst),
                1,
                "source.stop() fires on async unmount"
            );
        })
        .await;
}
