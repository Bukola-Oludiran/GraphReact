//! Pure-Rust message-dispatch benchmark — paired with the NAPI bench in
//! `crates/graphreact-napi/test-ts/bench/message_dispatch.bench.ts`.
//!
//! Same workload across both: inject `MESSAGES` external `tick` messages
//! at a producer node; producer forwards each to a consumer (one-hop
//! delivery). Total observed deliveries = `MESSAGES * 2`. The ratio
//! (NAPI / Rust) is the M6 perf gate — the plan requires ≤2×.
//!
//! Run via: `cargo test -p graphreact-core --release --test bench_message_dispatch -- --nocapture`
//!
//! Plain `Instant` timing (no criterion) is enough to validate the gate.
//! Statistical-significance criterion benches land later if needed.

use graphreact_core::{
    use_emit, use_message, Attrs, CommunicationGraph, EmitOptions, NodeContext, NodeDefinition,
    PermissionManifest, Scheduler,
};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

const MESSAGES: u64 = 250_000;
const WARMUP_MESSAGES: u64 = 10_000;

#[test]
fn bench_two_node_message_dispatch() {
    let mut graph = CommunicationGraph::new();
    graph.add_node("producer", Attrs::new());
    graph.add_node("consumer", Attrs::new());
    graph.add_edge("producer", "consumer", None, Attrs::new());

    let producer_seen = Rc::new(Cell::new(0u64));
    let consumer_seen = Rc::new(Cell::new(0u64));
    let mut scheduler = Scheduler::new(graph);
    scheduler.set_max_process_depth(usize::try_from(MESSAGES).unwrap() + 1024);

    let p_seen = Rc::clone(&producer_seen);
    scheduler
        .register_node(
            NodeDefinition::new("Producer", move |_ctx: &NodeContext| {
                let p_seen = Rc::clone(&p_seen);
                use_message("tick", move |_msg| {
                    p_seen.set(p_seen.get() + 1);
                    let emit = use_emit();
                    // Fresh TTL on the forward so it doesn't decay to 0
                    // mid-bench; cycle protection isn't relevant here
                    // (no cycles in a producer→consumer edge).
                    emit(
                        "tick",
                        serde_json::Value::Null,
                        EmitOptions {
                            to: Some(vec!["consumer".into()]),
                            ttl: Some(8),
                            ..Default::default()
                        },
                    );
                });
                PermissionManifest::new(["tick"], ["tick"])
            }),
            "producer",
            Attrs::new(),
            0,
        )
        .unwrap();

    let c_seen = Rc::clone(&consumer_seen);
    scheduler
        .register_node(
            NodeDefinition::new("Consumer", move |_ctx: &NodeContext| {
                let c_seen = Rc::clone(&c_seen);
                use_message("tick", move |_msg| {
                    c_seen.set(c_seen.get() + 1);
                });
                PermissionManifest::new(["tick"], [] as [&str; 0])
            }),
            "consumer",
            Attrs::new(),
            0,
        )
        .unwrap();

    scheduler.start().unwrap();

    // Warmup so JIT / cache effects don't dominate the first run's
    // measurement. Counters are reset (subtracted out) before timing.
    for _ in 0..WARMUP_MESSAGES {
        scheduler.send("producer", "tick", serde_json::Value::Null);
    }
    scheduler.process_batch().unwrap();
    let warmup_p = producer_seen.get();
    let warmup_c = consumer_seen.get();

    // Inject MESSAGES external ticks at the producer. Each one fires the
    // producer's handler (counter += 1) and forwards to the consumer
    // (counter += 1). Total deliveries = MESSAGES * 2.
    let begin = Instant::now();
    for _ in 0..MESSAGES {
        scheduler.send("producer", "tick", serde_json::Value::Null);
    }
    scheduler.process_batch().unwrap();
    let elapsed = begin.elapsed();

    // Subtract warmup deliveries from the totals — we time only the
    // post-warmup phase.
    let post_p = producer_seen.get() - warmup_p;
    let post_c = consumer_seen.get() - warmup_c;

    let total = post_p + post_c;
    #[allow(clippy::cast_precision_loss)]
    let throughput = if elapsed.as_secs_f64() > 0.0 {
        total as f64 / elapsed.as_secs_f64()
    } else {
        f64::INFINITY
    };
    let per_msg_ns = if total > 0 {
        elapsed.as_nanos() / u128::from(total)
    } else {
        0
    };

    eprintln!(
        "[pure-rust bench] injected={MESSAGES} delivered={total} \
         elapsed={elapsed:?} per_msg={per_msg_ns}ns throughput={throughput:.0} msg/sec"
    );

    assert_eq!(producer_seen.get(), MESSAGES + WARMUP_MESSAGES);
    assert_eq!(consumer_seen.get(), MESSAGES + WARMUP_MESSAGES);
    scheduler.stop();
}
