//! Daemon mode + tokio source integration — closes out M4.
//!
//! `run_daemon` parks on `tokio::sync::Notify` between bursts of work.
//! Cross-thread wake-ups (e.g. a tokio task pushing source events) must
//! reliably restart the scheduler loop. The scheduler itself is `!Send`,
//! so the daemon runs inside a `LocalSet`; external pushers go through
//! a [`SchedulerHandle`] (`Send + Sync`) so they don't double-borrow
//! the scheduler.

use graphreact_core::{
    timer_source, use_message, use_source, Attrs, CommunicationGraph, NodeContext, NodeDefinition,
    PermissionManifest, Scheduler,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::LocalSet;

#[test]
fn run_daemon_processes_external_pushes_then_stops_on_signal() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let local = LocalSet::new();

    let received = Rc::new(Cell::new(0u32));
    let received_clone = Rc::clone(&received);

    local.block_on(&runtime, async move {
        let component = move |_ctx: &NodeContext| {
            let received = Rc::clone(&received_clone);
            use_message("ping", move |_msg| {
                received.set(received.get() + 1);
            });
            PermissionManifest::new(["ping"], Vec::<String>::new())
        };

        let mut graph = CommunicationGraph::new();
        graph.add_node("n", Attrs::new());
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
            .unwrap();

        // Cross-thread handle — `Send + Sync`, captures Arc-shared state.
        let handle = scheduler.handle();

        tokio::task::spawn_local(async move {
            for i in 0..3u32 {
                tokio::time::sleep(Duration::from_millis(5)).await;
                handle.send("n", "ping", serde_json::json!({ "i": i }));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            handle.stop_daemon();
        });

        scheduler.run_daemon().await.unwrap();
    });

    assert_eq!(received.get(), 3);
}

#[test]
fn use_source_event_through_daemon_lands_as_message() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let local = LocalSet::new();

    let ticks = Arc::new(AtomicU32::new(0));
    let ticks_for_component = Arc::clone(&ticks);

    local.block_on(&runtime, async move {
        let component = move |_ctx: &NodeContext| {
            // Subscribe to a 5ms timer source. The bridge converts each
            // push into a `tick` message routed to this node.
            use_source(|| timer_source(5), vec![]);
            let ticks = Arc::clone(&ticks_for_component);
            use_message("tick", move |_msg| {
                ticks.fetch_add(1, Ordering::Relaxed);
            });
            PermissionManifest::new(["tick"], Vec::<String>::new())
        };

        let mut graph = CommunicationGraph::new();
        graph.add_node("ticker", Attrs::new());
        let mut scheduler = Scheduler::new(graph);
        scheduler
            .register_node(
                NodeDefinition::new("Ticker", component),
                "ticker",
                Attrs::new(),
                0,
            )
            .unwrap();

        let handle = scheduler.handle();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            handle.stop_daemon();
        });

        scheduler.run_daemon().await.unwrap();
    });

    let count = ticks.load(Ordering::Relaxed);
    assert!(
        count >= 3,
        "expected at least 3 ticks within ~60ms (got {count})"
    );
}
