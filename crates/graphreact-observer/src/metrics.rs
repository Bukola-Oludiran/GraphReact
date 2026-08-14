//! **M16-Backend Phase 2.** Hand-rolled metric primitives — `Counter`,
//! `Gauge`, sharded `Histogram` — plus the [`MetricsInner`] aggregate
//! that the [`crate::PrometheusSink`] writes into and the
//! [`crate::prometheus`] text-format encoder reads from.
//!
//! ## Why hand-rolled, not the `prometheus` crate
//!
//! The `prometheus` crate brings `protobuf` + `lazy_static` we don't
//! otherwise use, and its label-tuple counter lookup adds a `BTreeMap`
//! probe per emission. Our hot path is the scheduler's
//! `Scheduler::deliver` (309k msg/sec on the M14 ring fixture); a
//! locked map probe per delivery would burn the bench-(b) ≤1% gate.
//! Hand-rolled counters target a `DashMap<u64, ...>` shard read +
//! `AtomicU64::fetch_add(Relaxed)` — ~25-40ns per emission after
//! warmup vs ~150ns through the `prometheus` crate's
//! `Counter.with_label_values` path.
//!
//! ## Cardinality risk (read before scaling labels)
//!
//! [`MetricsInner::messages_delivered_inc`] keys on `(from, to, type)`
//! per the M16-Backend P2 plan. For a `.grx` system with `N` bodies
//! and `T` message types the upper bound is `N × N × T` distinct
//! series. Production Prometheus deployments treat node_id as
//! high-cardinality; >10⁵ unique series per scrape stresses TSDB
//! ingestion. **Operators scraping into a cardinality-sensitive
//! Prometheus deployment SHOULD cap their `.grx` system size or
//! aggregate at the recording-rule layer.** M-Prod #6 will land
//! per-label-cardinality caps + sharded label registries; this
//! module ships the substrate-not-MVP version that exposes the
//! full label set so the cap can be added without a breaking
//! metric-name change.
//!
//! ## Hot-path discipline
//!
//! Every public increment / observation method is `#[inline]` and
//! takes only `&self`. The DashMap fast path is `get(&hash)` ⇒
//! `fetch_add(1, Relaxed)`; the `or_insert_with` path runs once per
//! unique label tuple and allocates the label `String`s + the inner
//! `Arc<Counter>` exactly once.
//!
//! The histogram is sharded across `HISTOGRAM_SHARDS` (16) replicas;
//! observers index by a thread-local `SHARD_IDX` initialised lazily
//! from a global `AtomicUsize`. Sum-over-shards happens at scrape
//! time only.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use rustc_hash::FxHasher;
use std::hash::Hasher;

/// 14-bucket powers-of-2 histogram covering 4µs..32.7ms (`4µs * 2^k`
/// for `k in 0..14`). `Histogram::observe` walks the array linearly
/// to find the smallest upper bound ≥ `duration_ns` and increments
/// that bucket. The plan budgets "4µs..65ms"; we stop one bucket
/// short of 65ms because Prometheus's `+Inf` bucket (count_total)
/// implicitly covers everything above the last finite bucket.
pub const HISTOGRAM_BUCKETS_NS: [u64; 14] = [
    4_000,      // 4µs
    8_000,      // 8µs
    16_000,     // 16µs
    32_000,     // 32µs
    64_000,     // 64µs
    128_000,    // 128µs
    256_000,    // 256µs
    512_000,    // 512µs
    1_024_000,  // ~1ms
    2_048_000,  // ~2ms
    4_096_000,  // ~4ms
    8_192_000,  // ~8ms
    16_384_000, // ~16ms
    32_768_000, // ~32ms
];

/// Number of histogram shards. Trade-off: more shards = less write
/// contention, more work at scrape. 16 covers a typical 8-16 core
/// box without aliasing two CPUs to the same shard under a uniform
/// hash. Power-of-2 so `% HISTOGRAM_SHARDS` lowers to a `&` mask.
pub const HISTOGRAM_SHARDS: usize = 16;

/// Lock-free monotonic counter. Writers `fetch_add(1, Relaxed)`;
/// the encoder reads via `load(Relaxed)`. Relaxed ordering is
/// correct: Prometheus scrapes are eventually-consistent, with no
/// happens-before requirement between counters.
#[derive(Default)]
pub struct Counter(AtomicU64);

impl Counter {
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    #[inline]
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Lock-free `u64` gauge. Writers `store`; the encoder reads `load`.
/// All operations Relaxed — gauges are last-write-wins under scrape.
#[derive(Default)]
pub struct Gauge(AtomicU64);

impl Gauge {
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    #[inline]
    pub fn set(&self, v: u64) {
        self.0.store(v, Ordering::Relaxed);
    }

    #[inline]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Sharded histogram with 14 powers-of-2 buckets. Reads sum across
/// every shard at scrape time; writes pick a thread-local shard so
/// concurrent `observe` calls on different threads land in different
/// cache lines.
pub struct Histogram {
    /// `[shards][buckets]`. `Box` so `MetricsInner: !Sized` doesn't
    /// hold (`AtomicU64` is `Sized` but the array is large enough
    /// that we want it heap-allocated to keep `MetricsInner` slim).
    shards: Box<[[AtomicU64; 14]; HISTOGRAM_SHARDS]>,
    /// Sum of all observed values (nanoseconds). Sharded same way.
    sum_shards: Box<[AtomicU64; HISTOGRAM_SHARDS]>,
    /// Total observation count. Sharded same way.
    count_shards: Box<[AtomicU64; HISTOGRAM_SHARDS]>,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// Lazily-assigned per-thread shard index. Each thread gets a
    /// distinct value modulo `HISTOGRAM_SHARDS` from a global
    /// counter, distributing writes across shards.
    static SHARD_IDX: usize = {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed) % HISTOGRAM_SHARDS
    };
}

impl Histogram {
    #[must_use]
    pub fn new() -> Self {
        // `AtomicU64` is `Default` so we can construct via array
        // initialisation. The verbose form is unavoidable on stable
        // because `[AtomicU64::new(0); 14]` requires `Copy`, which
        // `AtomicU64` doesn't impl.
        let shards = Box::new(std::array::from_fn(|_| {
            std::array::from_fn(|_| AtomicU64::new(0))
        }));
        let sum_shards = Box::new(std::array::from_fn(|_| AtomicU64::new(0)));
        let count_shards = Box::new(std::array::from_fn(|_| AtomicU64::new(0)));
        Self {
            shards,
            sum_shards,
            count_shards,
        }
    }

    /// Record one observation. `duration_ns` is the elapsed
    /// nanoseconds for the operation being measured (e.g.,
    /// `process_batch` end-to-end time).
    #[inline]
    pub fn observe(&self, duration_ns: u64) {
        let shard_idx = SHARD_IDX.with(|i| *i);
        let bucket_idx = bucket_for(duration_ns);
        if let Some(bucket) = self.shards[shard_idx].get(bucket_idx) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_shards[shard_idx].fetch_add(duration_ns, Ordering::Relaxed);
        self.count_shards[shard_idx].fetch_add(1, Ordering::Relaxed);
    }

    /// Read aggregate state across every shard. Returns
    /// `(cumulative_buckets, sum_ns, count)` — cumulative because the
    /// Prometheus text format emits cumulative bucket counts (each
    /// `bucket{le="X"}` value is the count of observations ≤ X).
    #[must_use]
    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut buckets = [0u64; 14];
        for shard in self.shards.iter() {
            for (i, bucket) in shard.iter().enumerate() {
                buckets[i] += bucket.load(Ordering::Relaxed);
            }
        }
        // Convert from per-bucket counts to cumulative counts.
        for i in 1..buckets.len() {
            buckets[i] += buckets[i - 1];
        }
        let sum: u64 = self
            .sum_shards
            .iter()
            .map(|s| s.load(Ordering::Relaxed))
            .sum();
        let count: u64 = self
            .count_shards
            .iter()
            .map(|s| s.load(Ordering::Relaxed))
            .sum();
        HistogramSnapshot {
            cumulative_buckets: buckets,
            sum_ns: sum,
            count,
        }
    }
}

/// Returns the index in `HISTOGRAM_BUCKETS_NS` of the smallest bucket
/// whose upper bound is ≥ `duration_ns`. Saturates at the last
/// bucket — observations larger than the last finite bucket are
/// folded into it (Prometheus emits a `+Inf` bucket separately
/// equal to `count_total`, recovered by the encoder).
#[inline]
fn bucket_for(duration_ns: u64) -> usize {
    // Linear walk: 14 buckets fit in one cache line; branch
    // prediction saturates after warmup. `binary_search_by` adds
    // overhead at this size.
    for (i, upper) in HISTOGRAM_BUCKETS_NS.iter().enumerate() {
        if duration_ns <= *upper {
            return i;
        }
    }
    HISTOGRAM_BUCKETS_NS.len() - 1
}

/// Read-only snapshot of histogram state. Encoder turns this into
/// the Prometheus text format's `_bucket{le="X"} N`, `_sum N`, and
/// `_count N` lines.
pub struct HistogramSnapshot {
    /// One entry per finite bucket; values are CUMULATIVE counts.
    pub cumulative_buckets: [u64; 14],
    /// Sum of all observed values, in nanoseconds.
    pub sum_ns: u64,
    /// Total observation count (= the `+Inf` bucket).
    pub count: u64,
}

/// Counter with attached label values. Lives behind an `Arc` inside
/// the `DashMap` so the encoder can iterate values + read counters
/// without holding a shard lock for the duration of encoding.
pub struct LabeledCounter {
    pub labels: Vec<(&'static str, String)>,
    pub counter: Counter,
}

impl LabeledCounter {
    #[must_use]
    pub fn new(labels: Vec<(&'static str, String)>) -> Self {
        Self {
            labels,
            counter: Counter::new(),
        }
    }
}

/// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Gauge with
/// attached label values. Mirror of [`LabeledCounter`] for the
/// per-forwarder `audit_forwarder_queue_depth{forwarder}` series.
/// Registered ONCE per forwarder at hub-construction time so the
/// worker's `gauge.set()` is pure `AtomicU64::store(Relaxed)` with
/// zero DashMap traffic after warm.
pub struct LabeledGauge {
    pub labels: Vec<(&'static str, String)>,
    pub gauge: Gauge,
}

/// Histogram with attached label values. The label vocabulary is
/// bounded by the caller; the histogram itself retains the same
/// sharded, lock-free write path as [`Histogram`].
pub struct LabeledHistogram {
    pub labels: Vec<(&'static str, String)>,
    pub histogram: Histogram,
}

impl LabeledHistogram {
    #[must_use]
    pub fn new(labels: Vec<(&'static str, String)>) -> Self {
        Self {
            labels,
            histogram: Histogram::new(),
        }
    }
}

impl LabeledGauge {
    #[must_use]
    pub fn new(labels: Vec<(&'static str, String)>) -> Self {
        Self {
            labels,
            gauge: Gauge::new(),
        }
    }
}

/// Aggregate metric storage shared between the `PrometheusSink`
/// (writer) and the `prometheus.rs` text-format encoder (reader).
/// Held behind `Arc` so the HTTP handler and the sink see the same
/// counters.
pub struct MetricsInner {
    /// Phase A durability metrics are deliberately unlabeled. A process owns
    /// at most one active deployment, so deployment/run identifiers would add
    /// cardinality without improving operator decisions.
    pub durability_commit_duration: Histogram,
    pub durability_commits_total: Counter,
    pub durability_commit_failures_total: Counter,
    pub durability_log_bytes: Gauge,
    pub durability_snapshot_bytes: Gauge,
    pub durability_recovery_duration: Histogram,
    pub durability_replay_records_total: Counter,
    pub durability_effect_retries_total: Counter,
    pub durability_indeterminate_effects_total: Counter,
    pub durability_lease_renewals_total: Counter,
    pub durability_lease_losses_total: Counter,
    /// 1 for the active lease holder, 0 for standby/off/local mode.
    pub durability_active: Gauge,

    /// `grx_messages_delivered_total{from,to,type}` — high-cardinality.
    /// See module-level cardinality risk note.
    pub messages_delivered: DashMap<u64, Arc<LabeledCounter>>,

    /// `grx_messages_dropped_total{reason}`. `reason` is bounded:
    /// `"policy_rejected"` (delivered.accepted=false),
    /// `"max_depth"` (tick_end.terminated_by_depth_cap=true).
    pub messages_dropped: DashMap<u64, Arc<LabeledCounter>>,

    /// `grx_node_renders_total{node_id}` — one counter per body.
    /// Cardinality scales with `.grx` system size.
    pub node_renders: DashMap<u64, Arc<LabeledCounter>>,

    /// `grx_node_mount_total` — no labels.
    pub node_mount_total: Counter,

    /// `grx_node_unmount_total{reason}` — bounded ExitReason variants.
    pub node_unmount: DashMap<u64, Arc<LabeledCounter>>,

    /// `grx_supervisor_restarts_total{action}` — SupervisorAction
    /// (Proceeded / Deferred / Escalated) is a bounded enum. The
    /// plan calls this `{policy}` but the underlying event carries
    /// `action`, not the policy that produced it; documented as a
    /// reframing in the metric's `# HELP` line.
    pub supervisor_restarts: DashMap<u64, Arc<LabeledCounter>>,

    /// `grx_remote_messages_routed_total{peer,direction}`.
    /// Direction is bounded (`inbound`/`outbound`); peer is per-peer
    /// in the cluster.
    pub remote_messages_routed: DashMap<u64, Arc<LabeledCounter>>,

    /// `grx_hot_reload_total{outcome}` — `"applied"` or `"rolled_back"`.
    pub hot_reload: DashMap<u64, Arc<LabeledCounter>>,

    /// **M22 phase 4.** `grx_engine_mount_count{engine}` — total
    /// mounts (= calls to `register_node` on each per-engine
    /// scheduler) per engine slot. Bounded cardinality: one entry
    /// per active engine `("jsc"|"cpython"|"go"|"null"|"echo")`.
    pub engine_mount_count: DashMap<u64, Arc<LabeledCounter>>,

    /// **M22 phase 4.** `grx_engine_eval_count{engine}` — total body
    /// evaluations across `process_batch` rounds, per engine slot.
    /// Increments once per call to `evaluate_node`-equivalent in
    /// each per-engine runtime. Bounded cardinality identical to
    /// `engine_mount_count`.
    pub engine_eval_count: DashMap<u64, Arc<LabeledCounter>>,

    /// `grx_engine_process_batch_duration_seconds{engine}` — wall-clock
    /// duration of each active engine slot's `process_batch` call. The
    /// `engine` label uses the same closed vocabulary as the engine
    /// mount/evaluation counters.
    pub engine_process_batch_duration: DashMap<u64, Arc<LabeledHistogram>>,

    /// Queue-depth gauges — set by the `on_queue_depths` event.
    pub queue_depth_messages: Gauge,
    pub queue_depth_dirty: Gauge,
    pub queue_depth_pending_topology: Gauge,
    pub queue_depth_external_inbox: Gauge,
    pub queue_depth_pending_mutations: Gauge,

    /// `grx_process_batch_duration_seconds` — histogram of cascade
    /// round duration (tick_start → tick_end delta in nanoseconds).
    pub process_batch_duration: Histogram,

    /// **M-Prod sub-track #6 Phase A → Phase B Part 1.**
    /// `grx_rate_limit_blocked_total{route,bucket}` — per-(route,bucket)
    /// count of HTTP requests rejected by `tower_governor` with
    /// `429 Too Many Requests`. `route` is bounded to the 12 protected
    /// route patterns the management API registers (no `:id`
    /// substitution — the rate-limit observer middleware classifies the
    /// raw URI path back to its pattern), and `bucket` is bounded to a
    /// 3-entry vocabulary `{"per_ip", "per_token", "per_token_no_match"}`
    /// stamped by the per-layer error_handler closures. Total cardinality
    /// is bounded by `(protected_routes + 1) × 3 = 13 × 3 = 39` series.
    /// See [`crate::management_api::build_router`] for the observer
    /// middleware that increments this counter.
    pub rate_limit_blocked: DashMap<u64, Arc<LabeledCounter>>,

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).**
    /// `grx_audit_sink_write_failures_total{kind}` — per-kind count
    /// of audit-sink I/O failures, where `kind` is one of
    /// `{"open", "write", "fsync", "rotate"}` (closed 4-entry
    /// vocabulary). Cardinality is bounded at 4 series.
    /// Family is silent in steady state — only emitted on
    /// `/metrics` once at least one failure has fired. Read by
    /// operator dashboards alongside the existing
    /// `audit_sink_write_failed` tracing line.
    pub audit_sink_write_failures: DashMap<u64, Arc<LabeledCounter>>,

    /// `grx_secret_read_failures_total{reason}`. Reasons are the
    /// closed vocabulary `{undeclared,missing,backend_error,non_utf8}`.
    pub secret_read_failures: DashMap<u64, Arc<LabeledCounter>>,

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).**
    /// `grx_audit_sink_pending_records` — current size of the
    /// in-flight batched-fsync buffer. Stays at 0 forever under
    /// the default [`crate::audit_chain::FsyncMode::PerWrite`]
    /// mode (no batching, no pending records). Under
    /// [`crate::audit_chain::FsyncMode::Batched`] this gauge
    /// fluctuates between 0 and `max_pending_records` — operators
    /// alert when it sustains near the cap (worker too slow OR
    /// disk under pressure).
    pub audit_sink_pending_records: Gauge,

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).**
    /// `grx_audit_forwarder_records_total{forwarder,outcome}` —
    /// per-(forwarder, outcome) count of audit-record fan-out
    /// attempts. `forwarder` is one of the 2-entry closed
    /// vocabulary `{"spool_file", "stderr_jsonl"}` (extending
    /// reference impls add labels); `outcome` is one of the
    /// 4-entry closed vocabulary
    /// `{"forwarded", "dropped_queue_full", "dropped_shutdown", "write_failed"}`.
    /// Total cardinality bounded at 8 series. Family suppressed
    /// until the first fan fires.
    pub audit_forwarder_records: DashMap<u64, Arc<LabeledCounter>>,

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).**
    /// `grx_audit_forwarder_queue_depth{forwarder}` — current
    /// backlog of the per-forwarder bounded mpsc worker queue
    /// (default cap 1024, env `GRX_AUDIT_FORWARDER_QUEUE_DEPTH`).
    /// Sustained near cap signals a slow consumer. Cardinality
    /// bounded at 2 series.
    pub audit_forwarder_queue_depth: DashMap<u64, Arc<LabeledGauge>>,

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).**
    /// `grx_audit_forwarder_drain_timeouts_total{forwarder}` —
    /// per-forwarder count of drain operations that hit
    /// `OBSERVER_SHUTDOWN_DEADLINE` before the in-flight queue
    /// fully flushed. Operators alert on non-zero.
    pub audit_forwarder_drain_timeouts: DashMap<u64, Arc<LabeledCounter>>,
}

impl Default for MetricsInner {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsInner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            durability_commit_duration: Histogram::new(),
            durability_commits_total: Counter::new(),
            durability_commit_failures_total: Counter::new(),
            durability_log_bytes: Gauge::new(),
            durability_snapshot_bytes: Gauge::new(),
            durability_recovery_duration: Histogram::new(),
            durability_replay_records_total: Counter::new(),
            durability_effect_retries_total: Counter::new(),
            durability_indeterminate_effects_total: Counter::new(),
            durability_lease_renewals_total: Counter::new(),
            durability_lease_losses_total: Counter::new(),
            durability_active: Gauge::new(),
            messages_delivered: DashMap::new(),
            messages_dropped: DashMap::new(),
            node_renders: DashMap::new(),
            node_mount_total: Counter::new(),
            node_unmount: DashMap::new(),
            supervisor_restarts: DashMap::new(),
            remote_messages_routed: DashMap::new(),
            hot_reload: DashMap::new(),
            engine_mount_count: DashMap::new(),
            engine_eval_count: DashMap::new(),
            engine_process_batch_duration: DashMap::new(),
            queue_depth_messages: Gauge::new(),
            queue_depth_dirty: Gauge::new(),
            queue_depth_pending_topology: Gauge::new(),
            queue_depth_external_inbox: Gauge::new(),
            queue_depth_pending_mutations: Gauge::new(),
            process_batch_duration: Histogram::new(),
            rate_limit_blocked: DashMap::new(),
            audit_sink_write_failures: DashMap::new(),
            secret_read_failures: DashMap::new(),
            audit_sink_pending_records: Gauge::new(),
            audit_forwarder_records: DashMap::new(),
            audit_forwarder_queue_depth: DashMap::new(),
            audit_forwarder_drain_timeouts: DashMap::new(),
        }
    }

    #[inline]
    pub fn durability_commit_observe(&self, duration_ns: u64, success: bool) {
        self.durability_commit_duration.observe(duration_ns);
        if success {
            self.durability_commits_total.inc();
        } else {
            self.durability_commit_failures_total.inc();
        }
    }

    #[inline]
    pub fn durability_recovery_observe(&self, duration_ns: u64, replayed_records: u64) {
        self.durability_recovery_duration.observe(duration_ns);
        self.durability_replay_records_total.add(replayed_records);
    }

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Increment
    /// the per-(forwarder, outcome) audit-forwarder records
    /// counter. Both labels are `&'static str` so the closed
    /// vocabulary (2 forwarders × 4 outcomes = 8 series) is
    /// enforced at the type level.
    #[inline]
    pub fn audit_forwarder_records_inc(&self, forwarder: &'static str, outcome: &'static str) {
        let key = hash2(forwarder, outcome);
        if let Some(c) = self.audit_forwarder_records.get(&key) {
            c.counter.inc();
            return;
        }
        self.audit_forwarder_records
            .entry(key)
            .or_insert_with(|| {
                Arc::new(LabeledCounter::new(vec![
                    ("forwarder", forwarder.to_string()),
                    ("outcome", outcome.to_string()),
                ]))
            })
            .counter
            .inc();
    }

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Register
    /// and return the cached `Arc<LabeledGauge>` for one
    /// forwarder's queue-depth series. Called ONCE per forwarder
    /// at hub-construction time; the worker holds the returned
    /// `Arc` and updates the gauge with pure
    /// `AtomicU64::store(Relaxed)` — zero DashMap traffic on the
    /// fan-out hot path.
    pub fn audit_forwarder_queue_depth_register(
        &self,
        forwarder: &'static str,
    ) -> Arc<LabeledGauge> {
        let key = hash1(forwarder);
        self.audit_forwarder_queue_depth
            .entry(key)
            .or_insert_with(|| {
                Arc::new(LabeledGauge::new(vec![(
                    "forwarder",
                    forwarder.to_string(),
                )]))
            })
            .clone()
    }

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Set the
    /// queue-depth gauge for one forwarder. Cached `Arc` lookup
    /// after warm; falls through to register-on-miss for the
    /// first-event path.
    #[inline]
    pub fn audit_forwarder_queue_depth_set(&self, forwarder: &'static str, depth: u64) {
        let key = hash1(forwarder);
        if let Some(g) = self.audit_forwarder_queue_depth.get(&key) {
            g.gauge.set(depth);
            return;
        }
        self.audit_forwarder_queue_depth
            .entry(key)
            .or_insert_with(|| {
                Arc::new(LabeledGauge::new(vec![(
                    "forwarder",
                    forwarder.to_string(),
                )]))
            })
            .gauge
            .set(depth);
    }

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1d).** Increment
    /// the per-forwarder drain-timeout counter. Bumped when the
    /// hub's drain exceeded `OBSERVER_SHUTDOWN_DEADLINE` for a
    /// specific forwarder.
    #[inline]
    pub fn audit_forwarder_drain_timeouts_inc(&self, forwarder: &'static str) {
        let key = hash1(forwarder);
        if let Some(c) = self.audit_forwarder_drain_timeouts.get(&key) {
            c.counter.inc();
            return;
        }
        self.audit_forwarder_drain_timeouts
            .entry(key)
            .or_insert_with(|| {
                Arc::new(LabeledCounter::new(vec![(
                    "forwarder",
                    forwarder.to_string(),
                )]))
            })
            .counter
            .inc();
    }

    /// **M-Prod sub-track #1 Phase B Part 2 (Group 1ab).** Increment
    /// the per-`kind` audit-sink-write-failures counter. `kind` is
    /// drawn from the closed 4-entry vocabulary
    /// `{"open", "write", "fsync", "rotate"}`; the `&'static str`
    /// signature enforces the cap at the type level.
    ///
    /// Hot-path inline + get-then-insert avoids the DashMap shard
    /// write lock on cache hit — same pattern as
    /// [`Self::rate_limit_blocked_inc`].
    #[inline]
    pub fn audit_sink_write_failures_inc(&self, kind: &'static str) {
        let key = hash1(kind);
        if let Some(c) = self.audit_sink_write_failures.get(&key) {
            c.counter.inc();
            return;
        }
        self.audit_sink_write_failures
            .entry(key)
            .or_insert_with(|| Arc::new(LabeledCounter::new(vec![("kind", kind.to_string())])))
            .counter
            .inc();
    }

    /// Increment the bounded per-reason runtime secret-read failure
    /// counter. Callers pass only the four policy/backend outcomes.
    #[inline]
    pub fn secret_read_failures_inc(&self, reason: &'static str) {
        let key = hash1(reason);
        if let Some(counter) = self.secret_read_failures.get(&key) {
            counter.counter.inc();
            return;
        }
        self.secret_read_failures
            .entry(key)
            .or_insert_with(|| Arc::new(LabeledCounter::new(vec![("reason", reason.to_string())])))
            .counter
            .inc();
    }

    /// **M22 phase 4.** Increment the per-engine mount counter. The
    /// `engine` label is a `&'static str` to keep the cardinality
    /// bounded (one of `"jsc"|"cpython"|"go"|"null"|"echo"`) — the
    /// MixedRuntime + per-engine adapters pass their static slot
    /// label here.
    #[inline]
    pub fn engine_mount_count_inc(&self, engine: &'static str) {
        let key = hash1(engine);
        if let Some(c) = self.engine_mount_count.get(&key) {
            c.counter.inc();
            return;
        }
        self.engine_mount_count
            .entry(key)
            .or_insert_with(|| Arc::new(LabeledCounter::new(vec![("engine", engine.to_string())])))
            .counter
            .inc();
    }

    /// **M22 phase 4.** Increment the per-engine body-evaluation
    /// counter. Same `engine` label vocabulary as
    /// `engine_mount_count_inc`. Hot-path inline so the cost is
    /// roughly identical to `node_renders_inc`.
    #[inline]
    pub fn engine_eval_count_inc(&self, engine: &'static str) {
        let key = hash1(engine);
        if let Some(c) = self.engine_eval_count.get(&key) {
            c.counter.inc();
            return;
        }
        self.engine_eval_count
            .entry(key)
            .or_insert_with(|| Arc::new(LabeledCounter::new(vec![("engine", engine.to_string())])))
            .counter
            .inc();
    }

    /// Observe one engine slot's `process_batch` wall-clock duration.
    /// The first observation creates the labeled histogram; subsequent
    /// observations take the DashMap read path and a sharded atomic write.
    #[inline]
    pub fn engine_process_batch_duration_observe(&self, engine: &'static str, duration_ns: u64) {
        let key = hash1(engine);
        if let Some(h) = self.engine_process_batch_duration.get(&key) {
            h.histogram.observe(duration_ns);
            return;
        }
        self.engine_process_batch_duration
            .entry(key)
            .or_insert_with(|| {
                Arc::new(LabeledHistogram::new(vec![("engine", engine.to_string())]))
            })
            .histogram
            .observe(duration_ns);
    }

    /// Hot-path counter increment for `messages_delivered`.
    /// Allocation-free on cache hit; allocates label strings + the
    /// inner `Arc` exactly once per unique `(from, to, type)` tuple.
    #[inline]
    pub fn messages_delivered_inc(&self, from: &str, to: &str, msg_type: &str) {
        let key = hash3(from, to, msg_type);
        if let Some(c) = self.messages_delivered.get(&key) {
            c.counter.inc();
            return;
        }
        let entry = self
            .messages_delivered
            .entry(key)
            .or_insert_with(|| {
                Arc::new(LabeledCounter::new(vec![
                    ("from", from.to_string()),
                    ("to", to.to_string()),
                    ("type", msg_type.to_string()),
                ]))
            })
            .clone();
        entry.counter.inc();
    }

    #[inline]
    pub fn messages_dropped_inc(&self, reason: &'static str) {
        let key = hash1(reason);
        if let Some(c) = self.messages_dropped.get(&key) {
            c.counter.inc();
            return;
        }
        self.messages_dropped
            .entry(key)
            .or_insert_with(|| Arc::new(LabeledCounter::new(vec![("reason", reason.to_string())])))
            .counter
            .inc();
    }

    #[inline]
    pub fn node_renders_inc(&self, node_id: &str) {
        let key = hash1(node_id);
        if let Some(c) = self.node_renders.get(&key) {
            c.counter.inc();
            return;
        }
        self.node_renders
            .entry(key)
            .or_insert_with(|| {
                Arc::new(LabeledCounter::new(vec![("node_id", node_id.to_string())]))
            })
            .counter
            .inc();
    }

    #[inline]
    pub fn node_unmount_inc(&self, reason: &str) {
        let key = hash1(reason);
        if let Some(c) = self.node_unmount.get(&key) {
            c.counter.inc();
            return;
        }
        self.node_unmount
            .entry(key)
            .or_insert_with(|| Arc::new(LabeledCounter::new(vec![("reason", reason.to_string())])))
            .counter
            .inc();
    }

    #[inline]
    pub fn supervisor_restarts_inc(&self, action: &'static str) {
        let key = hash1(action);
        if let Some(c) = self.supervisor_restarts.get(&key) {
            c.counter.inc();
            return;
        }
        self.supervisor_restarts
            .entry(key)
            .or_insert_with(|| Arc::new(LabeledCounter::new(vec![("action", action.to_string())])))
            .counter
            .inc();
    }

    #[inline]
    pub fn remote_messages_routed_inc(&self, peer: &str, direction: &'static str) {
        let key = hash2(peer, direction);
        if let Some(c) = self.remote_messages_routed.get(&key) {
            c.counter.inc();
            return;
        }
        self.remote_messages_routed
            .entry(key)
            .or_insert_with(|| {
                Arc::new(LabeledCounter::new(vec![
                    ("peer", peer.to_string()),
                    ("direction", direction.to_string()),
                ]))
            })
            .counter
            .inc();
    }

    #[inline]
    pub fn hot_reload_inc(&self, outcome: &'static str) {
        let key = hash1(outcome);
        if let Some(c) = self.hot_reload.get(&key) {
            c.counter.inc();
            return;
        }
        self.hot_reload
            .entry(key)
            .or_insert_with(|| {
                Arc::new(LabeledCounter::new(vec![("outcome", outcome.to_string())]))
            })
            .counter
            .inc();
    }

    /// **M-Prod sub-track #6 Phase A → Phase B Part 1.** Increment the
    /// per-(route, bucket) rate-limit-blocked counter. `route` is the
    /// protected-route pattern (`&'static str`) classified by the
    /// rate-limit observer middleware in
    /// [`crate::management_api::build_router`]; `bucket` is the
    /// closed-over `&'static str` discriminator
    /// `{"per_ip", "per_token", "per_token_no_match"}` stamped by the
    /// per-layer governor `error_handler` closure (defaults to
    /// `"per_ip"` for back-compat if the bucket extension is missing).
    /// Total cardinality is bounded by `(routes + 1) × 3 ≤ 36` series.
    ///
    /// Hot-path inline + get-then-insert avoids the DashMap shard
    /// write lock on cache hit — same pattern as
    /// [`Self::remote_messages_routed_inc`] (the other 2-label family).
    #[inline]
    pub fn rate_limit_blocked_inc(&self, route: &'static str, bucket: &'static str) {
        let key = hash2(route, bucket);
        if let Some(c) = self.rate_limit_blocked.get(&key) {
            c.counter.inc();
            return;
        }
        self.rate_limit_blocked
            .entry(key)
            .or_insert_with(|| {
                Arc::new(LabeledCounter::new(vec![
                    ("route", route.to_string()),
                    ("bucket", bucket.to_string()),
                ]))
            })
            .counter
            .inc();
    }
}

#[inline]
fn hash1(a: &str) -> u64 {
    let mut h = FxHasher::default();
    h.write(a.as_bytes());
    h.finish()
}

#[inline]
fn hash2(a: &str, b: &str) -> u64 {
    let mut h = FxHasher::default();
    h.write(a.as_bytes());
    h.write_u8(0);
    h.write(b.as_bytes());
    h.finish()
}

#[inline]
fn hash3(a: &str, b: &str, c: &str) -> u64 {
    let mut h = FxHasher::default();
    h.write(a.as_bytes());
    h.write_u8(0);
    h.write(b.as_bytes());
    h.write_u8(0);
    h.write(c.as_bytes());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_increments() {
        let c = Counter::new();
        c.inc();
        c.inc();
        c.add(3);
        assert_eq!(c.get(), 5);
    }

    #[test]
    fn gauge_last_write_wins() {
        let g = Gauge::new();
        g.set(42);
        g.set(7);
        assert_eq!(g.get(), 7);
    }

    #[test]
    fn histogram_buckets_into_correct_slot() {
        let h = Histogram::new();
        h.observe(2_000); // 2µs → bucket 0 (4µs)
        h.observe(5_000); // 5µs → bucket 1 (8µs)
        h.observe(33_000); // 33µs → bucket 4 (64µs)
        h.observe(100_000_000); // 100ms → last bucket (saturates)
        let snap = h.snapshot();
        // Cumulative — every bucket ≥ 4µs sees both the 2µs and
        // 5µs observations + everything else above it.
        assert_eq!(snap.cumulative_buckets[0], 1, "≤4µs"); // just 2µs
        assert_eq!(snap.cumulative_buckets[1], 2, "≤8µs"); // 2µs + 5µs
        assert_eq!(snap.cumulative_buckets[4], 3, "≤64µs"); // + 33µs
        assert_eq!(snap.cumulative_buckets[13], 4, "≤32ms"); // saturated
        assert_eq!(snap.count, 4);
        assert_eq!(snap.sum_ns, 2_000 + 5_000 + 33_000 + 100_000_000);
    }

    #[test]
    fn metrics_inner_messages_delivered_distinct_tuples() {
        let m = MetricsInner::new();
        m.messages_delivered_inc("a", "b", "ping");
        m.messages_delivered_inc("a", "b", "ping");
        m.messages_delivered_inc("a", "c", "ping");
        m.messages_delivered_inc("a", "b", "pong");
        assert_eq!(m.messages_delivered.len(), 3);
        let total: u64 = m
            .messages_delivered
            .iter()
            .map(|e| e.value().counter.get())
            .sum();
        assert_eq!(total, 4);
    }

    #[test]
    fn metrics_inner_remote_messages_routed_keys_on_peer_and_direction() {
        let m = MetricsInner::new();
        m.remote_messages_routed_inc("alice", "outbound");
        m.remote_messages_routed_inc("alice", "inbound");
        m.remote_messages_routed_inc("alice", "outbound");
        assert_eq!(m.remote_messages_routed.len(), 2);
    }

    /// Hash function MUST distinguish all label tuples — collisions
    /// cause silent counter aliasing. `FxHasher` is well-distributed
    /// for short ASCII strings; this is a smoke test for typical
    /// usage.
    #[test]
    fn hash_collisions_avoided_for_typical_labels() {
        let pairs = [
            ("alice", "bob", "ping"),
            ("alice", "bob", "pong"),
            ("alice", "carol", "ping"),
            ("bob", "alice", "ping"),
            ("", "", ""),
            ("\0", "", ""),
            ("a", "b\0c", "d"),
            ("a\0b", "c", "d"),
        ];
        let mut hashes = std::collections::HashSet::new();
        for (a, b, c) in pairs {
            assert!(
                hashes.insert(hash3(a, b, c)),
                "collision for {a:?}/{b:?}/{c:?}"
            );
        }
    }
}
