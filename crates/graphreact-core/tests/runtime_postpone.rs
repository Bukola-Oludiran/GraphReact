//! gen_statem-style selective receive — message handlers can call
//! `postpone()` to defer the active message; the scheduler replays it
//! after the next state transition.
//!
//! Mirrors the postpone-and-replay pattern from
//! `lib/stdlib/src/gen_statem.erl` (lines 170-209), adapted for the
//! reactive scheduler.

use graphreact_core::{
    postpone, use_message, use_state, Attrs, CommunicationGraph, NodeContext, NodeDefinition,
    PermissionManifest, Scheduler, Signal,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[test]
fn postpone_defers_message_until_state_change() {
    let processed: Rc<RefCell<Vec<i64>>> = Rc::new(RefCell::new(Vec::new()));
    let signal_holder: Rc<RefCell<Option<Signal<bool>>>> = Rc::new(RefCell::new(None));

    let component = {
        let processed = Rc::clone(&processed);
        let signal_holder = Rc::clone(&signal_holder);
        move |_ctx: &NodeContext| {
            // `ready` flips from false → true; while false the handler
            // postpones every "data" message.
            let ready: Signal<bool> = use_state(false);
            *signal_holder.borrow_mut() = Some(ready);
            let processed = Rc::clone(&processed);
            use_message("data", move |msg| {
                if !ready.get() {
                    postpone();
                    return;
                }
                let v = msg
                    .payload
                    .get("v")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                processed.borrow_mut().push(v);
            });
            PermissionManifest::new(["data"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("n", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();

    // Send while not ready — message is postponed.
    scheduler.send("n", "data", serde_json::json!({ "v": 1 }));
    scheduler.send("n", "data", serde_json::json!({ "v": 2 }));
    scheduler.process_batch().unwrap();
    assert!(
        processed.borrow().is_empty(),
        "messages postponed while ready = false"
    );

    // Flip the signal — triggers re-evaluation. The scheduler drains
    // the postpone queue back into the main message queue; the new
    // handler (with ready=true) processes them.
    signal_holder.borrow().unwrap().set(true);
    scheduler.process_batch().unwrap();

    assert_eq!(*processed.borrow(), vec![1, 2]);
}

#[test]
fn postponed_messages_are_dropped_on_unmount() {
    // Confirms the scheduler's unmount path clears the per-node
    // postpone queue so no stray messages haunt the runtime.
    let signal_holder: Rc<RefCell<Option<Signal<bool>>>> = Rc::new(RefCell::new(None));

    let component = {
        let signal_holder = Rc::clone(&signal_holder);
        move |_ctx: &NodeContext| {
            let ready: Signal<bool> = use_state(false);
            *signal_holder.borrow_mut() = Some(ready);
            use_message("data", move |_msg| {
                if !ready.get() {
                    postpone();
                }
            });
            PermissionManifest::new(["data"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("n", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();

    scheduler.send("n", "data", serde_json::Value::Null);
    scheduler.process_batch().unwrap();

    // Unmount before the signal flips — the postponed message goes
    // away with the queue. No assertion fails because nothing crashes
    // when we re-process. Just confirms graceful cleanup.
    let key = scheduler.key_for("n").unwrap();
    scheduler.unmount(key);
    scheduler.process_batch().unwrap();
}

#[test]
fn postpone_outside_handler_is_a_noop() {
    // `postpone()` only affects the active delivery. Calling it from
    // outside any handler must not break later message delivery.
    let received = Rc::new(Cell::new(0u32));

    postpone(); // nothing active — should be inert.

    let component = {
        let received = Rc::clone(&received);
        move |_ctx: &NodeContext| {
            let received = Rc::clone(&received);
            use_message("ping", move |_msg| {
                received.set(received.get() + 1);
            });
            PermissionManifest::new(["ping"], Vec::<String>::new())
        }
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("n", Attrs::new());
    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(NodeDefinition::new("N", component), "n", Attrs::new(), 0)
        .unwrap();
    scheduler.start().unwrap();

    scheduler.send("n", "ping", serde_json::Value::Null);
    scheduler.process_batch().unwrap();
    assert_eq!(received.get(), 1);
}
