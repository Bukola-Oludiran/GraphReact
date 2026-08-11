//! M4-followon — scheduler TTL inheritance.
//!
//! Two regression tests for the gap surfaced by M12.3a-phase3d:
//!
//! 1. **Edge-attribute TTL caps emission.** A `<edge ttl={N}/>` annotation
//!    on the `(source, target)` pair must cap the TTL of messages emitted
//!    along that edge — `make_emit` was previously ignoring per-edge
//!    attrs entirely.
//! 2. **TTL carry-across re-render.** A node that emits from a `useEffect`
//!    whose deps changed in response to a delivered message must inherit
//!    that delivery's TTL, even though the emit happens *after* the
//!    synchronous handler returned and `current_delivery_ttl` was reset.
//!    This is the failure mode that made ping-pong loop unbounded on
//!    `grx run --runtime=jsc`.

use std::cell::RefCell;
use std::rc::Rc;

use graphreact_core::{
    use_effect, use_emit, use_message, use_state, Attrs, CommunicationGraph, EmitOptions,
    NodeContext, NodeDefinition, PermissionManifest, Scheduler,
};
use serde_json::json;

fn ttl_attr(ttl: u32) -> Attrs {
    let mut attrs = Attrs::new();
    attrs.insert("ttl".to_string(), json!(ttl));
    attrs
}

/// Edge `<edge ttl={3}/>` caps a message emitted on that edge to TTL=3
/// even if the sender's body emits without specifying `ttl` in
/// `EmitOptions`. The receiver's handler observes the post-decrement
/// TTL (msg.ttl == edge_cap - 1).
#[test]
fn edge_attribute_ttl_caps_emission() {
    let observed_ttls: Rc<RefCell<Vec<u32>>> = Rc::new(RefCell::new(Vec::new()));

    let sender_def = NodeDefinition::new("Sender", move |_ctx: &NodeContext| {
        let emit = use_emit();
        // Emit once on first render, no explicit TTL in options.
        use_effect(
            move || {
                emit("ping", json!({}), EmitOptions::default());
                None
            },
            vec![],
        );
        PermissionManifest::new(Vec::<String>::new(), ["ping"])
    });

    let receiver_def = {
        let observed_ttls = Rc::clone(&observed_ttls);
        NodeDefinition::new("Receiver", move |_ctx: &NodeContext| {
            let observed_ttls = Rc::clone(&observed_ttls);
            use_message("ping", move |msg| {
                observed_ttls.borrow_mut().push(msg.ttl);
            });
            PermissionManifest::new(["ping"], Vec::<String>::new())
        })
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("a".to_string(), Attrs::new());
    graph.add_node("b".to_string(), Attrs::new());
    // Edge with explicit ttl=3 cap — far below the DEFAULT_TTL=10 default.
    graph.add_edge("a".to_string(), "b".to_string(), None, ttl_attr(3));

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(sender_def, "a".to_string(), Attrs::new(), 0_u32)
        .expect("register sender");
    scheduler
        .register_node(receiver_def, "b".to_string(), Attrs::new(), 0_u32)
        .expect("register receiver");

    scheduler.process_batch().expect("batch");

    let observed = observed_ttls.borrow();
    assert_eq!(
        observed.len(),
        1,
        "receiver got exactly one ping; observed ttls: {observed:?}"
    );
    // `decrement_ttl` runs at deliver: outgoing TTL=3 → received as 2.
    // The relevant property is "<= 3" (the edge cap held); the decrement
    // is incidental.
    assert!(
        observed[0] <= 3,
        "edge ttl cap (3) held; received ttl = {} (must be ≤ 3)",
        observed[0]
    );
}

/// Ping-pong shape: two nodes bouncing 'ping'/'pong' messages, each
/// using `useState` for a local counter, `useEffect(..., [count])` to
/// emit on every counter change, and `useMessage(...)` to bump the
/// counter on every received message. Edge annotations cap the TTL;
/// the carry-across machinery propagates it across the dirty →
/// re-evaluate → flush_effects → emit boundary so the cycle decays
/// naturally instead of resetting to `DEFAULT_TTL` on every re-render.
///
/// Acceptance: `process_batch` returns `Ok(())` (no `MaxDepthExceeded`)
/// and both nodes saw multiple bounces before the TTL exhausted.
#[test]
fn ping_pong_ttl_inheritance_terminates_naturally() {
    let observed: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let make_def = |label: &'static str, accept: &'static str, emit_type: &'static str| {
        let observed = Rc::clone(&observed);
        NodeDefinition::new(label, move |_ctx: &NodeContext| {
            let count_signal = use_state(json!(0));
            let count: u32 = count_signal
                .get()
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0);

            let emit_fn = use_emit();
            // Use_message increments local state on every received message.
            {
                let observed = Rc::clone(&observed);
                let count_for_handler = count;
                let setter = count_signal;
                use_message(accept, move |_msg| {
                    observed
                        .borrow_mut()
                        .push(format!("{label}-recv:{count_for_handler}"));
                    setter.set(json!(count_for_handler + 1));
                });
            }
            // Effect emits whenever count changes (initial render
            // counts as a change from "uninitialised" to 0).
            {
                let observed = Rc::clone(&observed);
                let count_for_emit = count;
                use_effect(
                    move || {
                        observed
                            .borrow_mut()
                            .push(format!("{label}-emit:{count_for_emit}"));
                        emit_fn(
                            emit_type,
                            json!({ "n": count_for_emit }),
                            EmitOptions::default(),
                        );
                        None
                    },
                    vec![json!(count)],
                );
            }
            PermissionManifest::new([accept], [emit_type])
        })
    };

    let mut graph = CommunicationGraph::new();
    graph.add_node("ping".to_string(), Attrs::new());
    graph.add_node("pong".to_string(), Attrs::new());
    // Both edges TTL-bounded (graph treats edges as undirected internally
    // — one entry suffices, but adding both for clarity).
    graph.add_edge("ping".to_string(), "pong".to_string(), None, ttl_attr(5));
    graph.add_edge("pong".to_string(), "ping".to_string(), None, ttl_attr(5));

    let ping_def = make_def("ping", "pong", "ping");
    let pong_def = make_def("pong", "ping", "pong");

    let mut scheduler = Scheduler::new(graph);
    scheduler
        .register_node(ping_def, "ping".to_string(), Attrs::new(), 0_u32)
        .expect("register ping");
    scheduler
        .register_node(pong_def, "pong".to_string(), Attrs::new(), 0_u32)
        .expect("register pong");

    let result = scheduler.process_batch();
    assert!(
        result.is_ok(),
        "ping-pong terminated naturally without MaxDepthExceeded; got: {result:?}"
    );

    let log = observed.borrow();
    // Both nodes must have emitted multiple times AND received multiple
    // times — the bounce actually flowed before the TTL exhausted.
    let ping_emits = log.iter().filter(|s| s.starts_with("ping-emit:")).count();
    let ping_recvs = log.iter().filter(|s| s.starts_with("ping-recv:")).count();
    let pong_emits = log.iter().filter(|s| s.starts_with("pong-emit:")).count();
    let pong_recvs = log.iter().filter(|s| s.starts_with("pong-recv:")).count();

    assert!(
        ping_emits >= 2,
        "ping emitted multiple times before TTL exhaust: {log:?}"
    );
    assert!(pong_emits >= 2, "pong emitted multiple times: {log:?}");
    assert!(
        ping_recvs >= 1,
        "ping received from pong at least once: {log:?}"
    );
    assert!(
        pong_recvs >= 1,
        "pong received from ping at least once: {log:?}"
    );
}
