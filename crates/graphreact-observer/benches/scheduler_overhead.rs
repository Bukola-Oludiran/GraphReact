//! **M16-Backend Phase 1 acceptance gate.** Per-phase regression
//! gate from P2 onward — every phase that adds a hot-path callsite OR
//! lands a new consumer re-runs this bench to verify the trait-not-
//! attached → trait-attached overhead stays within the locked-in
//! budgets.
//!
//! ## The three cases
//!
//! - **(a) trait NOT attached** — baseline. Scheduler hot path runs
//!   exactly as it did before M16-Backend Phase 1.
//! - **(b) trait attached + `NoopSink`** — measures the cost of the
//!   `Option::is_some` branch + read-lock-clone on the snapshot. Gate:
//!   within 1% of (a).
//! - **(c) trait attached + `JsonLogTelemetry` to `io::sink()`** —
//!   measures the cost of full event encoding + `serde_json::to_writer`
//!   plus line write to the equivalent of `/dev/null`. Gate: within
//!   5% of (b).
//!
//! ## Scheduler shape
//!
//! Each iteration:
//!
//! 1. Mount a small ring of `N` nodes (configurable via the
//!    [`RING_NODE_COUNT`] constant).
//! 2. Drive `K` rounds of message dispatch around the ring (each
//!    message hops from node `i` to node `(i + 1) % N`).
//! 3. Tear down.
//!
//! Mounts + messages are deterministic so the bench is stable across
//! runs. The scheduler instance is fresh per iteration to avoid
//! amortizing setup cost across measurements.

use std::io;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};

use graphreact_core::{
    runtime::types::{NodeContext, PermissionManifest},
    Attrs, CommunicationGraph, NodeDefinition, Scheduler, TelemetrySink,
};
use graphreact_observer::{JsonLogTelemetry, MultiplexSink, PrometheusSink, TracingJsonLayer};

/// 1k-message ring fixture per the M16-Backend Phase-1 plan
/// verification gate. Per-event JSON encoding cost amortizes across
/// ~1000 message_delivered events so Phase-1's ONE consumer
/// (`JsonLogTelemetry`) has room for the overhead the plan budgets.
const RING_NODE_COUNT: usize = 8;
const MESSAGE_ROUNDS: usize = 1000;

/// `NoopSink` — every method is the trait's default no-op. Equivalent
/// to "trait attached but consumer cares about nothing". The
/// `Option::is_some` branch + Arc clone-on-snapshot still runs.
struct NoopSink;
impl TelemetrySink for NoopSink {}

/// Build a fresh scheduler with a ring of `RING_NODE_COUNT` nodes.
/// Each node accepts `"hop"` messages and re-emits to its right
/// neighbor with a TTL of 1 (so each round dies out before infinite
/// loops can build up).
fn build_ring_scheduler() -> Scheduler {
    let mut graph = CommunicationGraph::new();
    for i in 0..RING_NODE_COUNT {
        graph.add_node(format!("n{i}"), Attrs::new());
    }
    for i in 0..RING_NODE_COUNT {
        let from = format!("n{i}");
        let to = format!("n{}", (i + 1) % RING_NODE_COUNT);
        graph.add_edge(from, to, None, Attrs::new());
    }
    let mut scheduler = Scheduler::new(graph);
    for i in 0..RING_NODE_COUNT {
        let node_id = format!("n{i}");
        let definition = NodeDefinition::new(&node_id, |_ctx: &NodeContext| {
            PermissionManifest::new(vec!["hop".to_string()], vec!["hop".to_string()])
        });
        scheduler
            .register_node(definition, node_id, Attrs::new(), 0)
            .expect("register");
    }
    scheduler.start().expect("start");
    scheduler
}

/// Drive `MESSAGE_ROUNDS` rounds of `"hop"` traffic around the ring.
/// Each round mounts a single message at `n0` → `n1`; the scheduler
/// drives one cascade. With per-edge TTL=1 the message dies after one
/// hop (no handlers re-emit; we just want the deliver+evaluate path
/// exercised against a steady cadence of messages).
fn drive_rounds(scheduler: &mut Scheduler) {
    for r in 0..MESSAGE_ROUNDS {
        let to = format!("n{}", (r + 1) % RING_NODE_COUNT);
        scheduler.send(to, "hop", serde_json::json!({ "r": r }));
        scheduler.process_batch().expect("batch");
    }
}

fn bench_case_a(c: &mut Criterion) {
    c.bench_function("scheduler_overhead/case_a_trait_not_attached", |b| {
        b.iter(|| {
            let mut scheduler = build_ring_scheduler();
            drive_rounds(&mut scheduler);
        });
    });
}

fn bench_case_b(c: &mut Criterion) {
    c.bench_function("scheduler_overhead/case_b_trait_attached_noop", |b| {
        b.iter(|| {
            let mut scheduler = build_ring_scheduler();
            scheduler.install_telemetry_sink(Arc::new(NoopSink) as Arc<dyn TelemetrySink>);
            drive_rounds(&mut scheduler);
        });
    });
}

fn bench_case_c(c: &mut Criterion) {
    c.bench_function("scheduler_overhead/case_c_trait_attached_json_logs", |b| {
        b.iter(|| {
            let mut scheduler = build_ring_scheduler();
            // Point the JSON sink at io::sink() so we measure the
            // encode + write_all path WITHOUT real I/O variance.
            let sink = JsonLogTelemetry::with_writer(Box::new(io::sink()));
            scheduler.install_telemetry_sink(sink.into_arc());
            drive_rounds(&mut scheduler);
        });
    });
}

/// Optional case: MultiplexSink with two NoopSinks. Validates that
/// the fan-out combinator doesn't dominate the hot path. Gate is
/// looser (within 3% of (b)) because we deliberately doubled the
/// per-event cost.
fn bench_case_mux(c: &mut Criterion) {
    c.bench_function("scheduler_overhead/case_mux_two_noops", |b| {
        b.iter(|| {
            let mut scheduler = build_ring_scheduler();
            let mux = MultiplexSink::builder()
                .push(Arc::new(NoopSink) as Arc<dyn TelemetrySink>)
                .push(Arc::new(NoopSink) as Arc<dyn TelemetrySink>)
                .build();
            scheduler.install_telemetry_sink(Arc::new(mux) as Arc<dyn TelemetrySink>);
            drive_rounds(&mut scheduler);
        });
    });
}

/// **M16-Backend P2 gate.** Trait attached + [`PrometheusSink`].
/// Counter increments hit `DashMap<u64, _>` shard reads + atomic
/// `fetch_add(Relaxed)`. Gate: within 1% of (a). Fails when the
/// `(from,to,type)` hot path materially exceeds the noop branch's
/// cost — first remediation is to drop the `{from,to}` labels per
/// the M-Prod #6 cardinality-cap fallback (see
/// `crates/graphreact-observer/src/metrics.rs` cardinality note).
fn bench_case_d(c: &mut Criterion) {
    c.bench_function("scheduler_overhead/case_d_prometheus_sink", |b| {
        b.iter(|| {
            let mut scheduler = build_ring_scheduler();
            scheduler.install_telemetry_sink(PrometheusSink::new().into_arc());
            drive_rounds(&mut scheduler);
        });
    });
}

/// **M16-Backend P3 gate.** Trait + PrometheusSink + tracing-subscriber's
/// Registry holding our [`TracingJsonLayer`]. Verifies the subscriber's
/// per-event branch doesn't dominate when the scheduler hot path
/// fires no `tracing::*` events (the only firing site, supervisor
/// escalation at scheduler.rs:2561, runs only on retry-deadline
/// escalation; the bench fixture stays in steady-state delivery).
/// Gate: within 1% of (a). Re-runs at every subsequent phase.
fn bench_case_e(c: &mut Criterion) {
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    c.bench_function("scheduler_overhead/case_e_with_subscriber", |b| {
        b.iter(|| {
            let subscriber = Registry::default()
                .with(TracingJsonLayer::new(Box::new(io::sink())));
            with_default(subscriber, || {
                let mut scheduler = build_ring_scheduler();
                scheduler.install_telemetry_sink(PrometheusSink::new().into_arc());
                drive_rounds(&mut scheduler);
            });
        });
    });
}

/// **M16-Backend P11.1 gate.** Trait + `OtelSink` + an
/// `InMemorySpanExporter` via `SimpleSpanProcessor` (synchronous
/// in-process export — chosen because `BatchSpanProcessor` requires
/// a tokio runtime context that criterion benches don't host).
///
/// **Measured 2026-05-08 on macOS dev machine:**
/// `case_g/case_a ≈ +39%` (3.60 ms vs 2.59 ms baseline). This is
/// substantially higher than the P10 doc's optimistic <5% estimate
/// because the bench fires ~9000 spans per iteration (1 tick span +
/// ~8000 node.evaluate child spans + ~1000 message.deliver one-shot
/// spans) AND the synchronous SimpleSpanProcessor exports each span
/// inline. Production uses `BatchSpanProcessor` with async tokio
/// background task drain — the per-event cost there is dominated by
/// the queue push (~50ns), NOT the synchronous export here. A
/// `case_g_batch` variant requiring tokio integration is M-Prod
/// follow-up; this case puts a measured ceiling on the synchronous
/// path.
///
/// Gated behind `otel-test` because `InMemorySpanExporter` requires
/// `opentelemetry_sdk/testing`. Run with:
/// `cargo bench -p graphreact-observer --features otel-test --bench scheduler_overhead`
#[cfg(feature = "otel-test")]
fn bench_case_g_otel_sink(c: &mut Criterion) {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

    c.bench_function("scheduler_overhead/case_g_otel_sink", |b| {
        b.iter(|| {
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter)
                .build();
            let tracer = provider.tracer("graphreact-observer-bench");
            let sink = std::sync::Arc::new(graphreact_observer::OtelSink::with_tracer(tracer));
            let mut scheduler = build_ring_scheduler();
            scheduler.install_telemetry_sink(sink as Arc<dyn TelemetrySink>);
            drive_rounds(&mut scheduler);
            // Provider drops here → exporter drops → SDK background
            // tasks cleanly terminate.
            let _ = provider;
        });
    });
}

#[cfg(not(feature = "otel-test"))]
criterion_group!(
    benches,
    bench_case_a,
    bench_case_b,
    bench_case_c,
    bench_case_mux,
    bench_case_d,
    bench_case_e
);
#[cfg(feature = "otel-test")]
criterion_group!(
    benches,
    bench_case_a,
    bench_case_b,
    bench_case_c,
    bench_case_mux,
    bench_case_d,
    bench_case_e,
    bench_case_g_otel_sink
);
criterion_main!(benches);
