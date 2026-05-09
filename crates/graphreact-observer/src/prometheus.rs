//! **M16-Backend Phase 2.** Prometheus text-exposition-format encoder.
//!
//! Renders [`MetricsInner`] into the
//! [openmetrics-text-0.0.1](https://github.com/OpenObservability/OpenMetrics/blob/main/specification/OpenMetrics.md#text-format)-
//! compatible subset that the standard Prometheus scrape parser
//! accepts:
//!
//! ```text
//! # HELP metric_name help text
//! # TYPE metric_name counter
//! metric_name{label="value"} 42
//! ```
//!
//! Histograms emit `_bucket{le="X"}`, `_sum`, and `_count` lines
//! with cumulative bucket counts (per Prometheus convention).
//!
//! ## Why hand-rolled, not the `prometheus` crate's encoder
//!
//! The `prometheus` crate brings `protobuf` (~700 LOC of generated
//! code) and `lazy_static`, neither of which we use elsewhere. Our
//! exposition surface is small (10 metric families); ~150 LOC of
//! `write!` calls is cheaper to maintain than the `prometheus`
//! crate's coupled API. The output is byte-validated against the
//! `prometheus-parse` crate in the
//! `crates/graphreact-observer/tests/prometheus_scrape.rs`
//! integration test so any drift fails CI immediately.
//!
//! ## Label-value escaping
//!
//! Per the text-format spec, label values must escape `\\`, `"`,
//! and `\n`. [`escape_label_value`] performs exactly those three
//! escapes; any other character (including non-ASCII UTF-8) passes
//! through verbatim. Operators using high-cardinality labels with
//! arbitrary content should sanitise upstream.

use std::fmt::Write;

use crate::metrics::{HistogramSnapshot, LabeledCounter, MetricsInner, HISTOGRAM_BUCKETS_NS};

/// Encode the entire [`MetricsInner`] into the Prometheus text
/// exposition format. Returns a `String` ready to be served as the
/// `/metrics` HTTP response body (Content-Type:
/// `text/plain; version=0.0.4; charset=utf-8`).
#[must_use]
pub fn encode_text(inner: &MetricsInner) -> String {
    let mut s = String::with_capacity(8 * 1024);

    encode_labeled_counter(
        &mut s,
        "grx_messages_delivered_total",
        "Total number of messages delivered (post-policy accepted=true).",
        &inner.messages_delivered,
    );

    encode_labeled_counter(
        &mut s,
        "grx_messages_dropped_total",
        "Total messages dropped. reason=\"policy_rejected\" comes from \
         on_message_delivered{accepted=false}; reason=\"max_depth\" \
         counts ticks terminated by the cascade depth cap (one event \
         per saturated tick, NOT one per pending message).",
        &inner.messages_dropped,
    );

    encode_labeled_counter(
        &mut s,
        "grx_node_renders_total",
        "Total per-body render count (one per evaluate_node call).",
        &inner.node_renders,
    );

    encode_unlabeled_counter(
        &mut s,
        "grx_node_mount_total",
        "Total mounts via Scheduler::add_node_at_runtime.",
        inner.node_mount_total.get(),
    );

    encode_labeled_counter(
        &mut s,
        "grx_node_unmount_total",
        "Total unmounts. reason is one of {normal, shutdown, crashed, killed}.",
        &inner.node_unmount,
    );

    encode_labeled_counter(
        &mut s,
        "grx_supervisor_restarts_total",
        "Total supervisor decisions, labeled by action (proceeded, \
         deferred, escalated). The plan calls this {policy} but the \
         underlying event carries {action}; documented as a P2 \
         reframing pending a richer SupervisorDecisionMade event.",
        &inner.supervisor_restarts,
    );

    encode_labeled_counter(
        &mut s,
        "grx_remote_messages_routed_total",
        "Total messages routed across the cluster, labeled by peer + \
         direction (inbound/outbound).",
        &inner.remote_messages_routed,
    );

    encode_labeled_counter(
        &mut s,
        "grx_hot_reload_total",
        "Total hot-reload applies. outcome=\"applied\" on success; \
         outcome=\"rolled_back\" when the post-swap render errored \
         and the orchestrator restored prior definitions.",
        &inner.hot_reload,
    );

    encode_gauge(
        &mut s,
        "grx_queue_depth_messages",
        "Current depth of the messages queue at the last cascade-round \
         classification step.",
        inner.queue_depth_messages.get(),
    );
    encode_gauge(
        &mut s,
        "grx_queue_depth_dirty",
        "Current depth of the dirty queue.",
        inner.queue_depth_dirty.get(),
    );
    encode_gauge(
        &mut s,
        "grx_queue_depth_pending_topology",
        "Current depth of the pending_topology queue.",
        inner.queue_depth_pending_topology.get(),
    );
    encode_gauge(
        &mut s,
        "grx_queue_depth_external_inbox",
        "Current depth of the external_inbox queue.",
        inner.queue_depth_external_inbox.get(),
    );
    encode_gauge(
        &mut s,
        "grx_queue_depth_pending_mutations",
        "Current depth of the pending_mutations queue.",
        inner.queue_depth_pending_mutations.get(),
    );

    encode_histogram(
        &mut s,
        "grx_process_batch_duration_seconds",
        "Wall-clock duration of one process_batch cascade round, in seconds.",
        &inner.process_batch_duration.snapshot(),
    );

    s
}

/// Emit one labeled counter family. Skips the family entirely if no
/// label tuples have been observed yet (Prometheus best practice:
/// don't emit empty families).
fn encode_labeled_counter(
    s: &mut String,
    name: &str,
    help: &str,
    map: &dashmap::DashMap<u64, std::sync::Arc<LabeledCounter>>,
) {
    if map.is_empty() {
        return;
    }
    write_help(s, name, help);
    write_type(s, name, "counter");
    // Iterate the DashMap; each shard is locked briefly for the
    // duration of its own iter pass. Under a 309k msg/sec writer
    // this introduces brief read-side contention at scrape time
    // only — Prometheus scrapes are seconds apart in production.
    for entry in map {
        let lc = entry.value();
        write_metric_with_labels(s, name, &lc.labels, lc.counter.get());
    }
}

fn encode_unlabeled_counter(s: &mut String, name: &str, help: &str, value: u64) {
    write_help(s, name, help);
    write_type(s, name, "counter");
    write_metric_no_labels(s, name, value);
}

fn encode_gauge(s: &mut String, name: &str, help: &str, value: u64) {
    write_help(s, name, help);
    write_type(s, name, "gauge");
    write_metric_no_labels(s, name, value);
}

fn encode_histogram(s: &mut String, name: &str, help: &str, snap: &HistogramSnapshot) {
    write_help(s, name, help);
    write_type(s, name, "histogram");
    // `_bucket{le="X"}` lines, one per finite bucket. `le` value is
    // the bucket's upper bound in SECONDS (matches the metric name's
    // `_seconds` suffix); we convert from the underlying ns store.
    // The u64→f64 cast is precision-lossy in theory but exact for
    // the bucket bounds we hard-code (powers-of-2 well under 2⁵² ns)
    // and "good enough" for the sum (Prometheus quantile estimates
    // tolerate sub-µs jitter).
    for (i, upper_ns) in HISTOGRAM_BUCKETS_NS.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let upper_seconds = (*upper_ns as f64) / 1_000_000_000.0;
        let _ = writeln!(
            s,
            "{name}_bucket{{le=\"{upper_seconds}\"}} {}",
            snap.cumulative_buckets[i]
        );
    }
    // The `+Inf` bucket is conventionally the same as `_count`.
    let _ = writeln!(s, "{name}_bucket{{le=\"+Inf\"}} {}", snap.count);
    // `_sum` is in seconds (matches the metric-name unit suffix).
    #[allow(clippy::cast_precision_loss)]
    let sum_seconds = (snap.sum_ns as f64) / 1_000_000_000.0;
    let _ = writeln!(s, "{name}_sum {sum_seconds}");
    let _ = writeln!(s, "{name}_count {}", snap.count);
}

#[inline]
fn write_help(s: &mut String, name: &str, help: &str) {
    let _ = write!(s, "# HELP {name} ");
    // Per spec, HELP text is escaped for `\\` and `\n` (NOT `"`).
    for ch in help.chars() {
        match ch {
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            other => s.push(other),
        }
    }
    s.push('\n');
}

#[inline]
fn write_type(s: &mut String, name: &str, ty: &str) {
    let _ = writeln!(s, "# TYPE {name} {ty}");
}

#[inline]
fn write_metric_no_labels(s: &mut String, name: &str, value: u64) {
    let _ = writeln!(s, "{name} {value}");
}

fn write_metric_with_labels(
    s: &mut String,
    name: &str,
    labels: &[(&'static str, String)],
    value: u64,
) {
    s.push_str(name);
    s.push('{');
    let mut first = true;
    for (key, val) in labels {
        if !first {
            s.push(',');
        }
        first = false;
        s.push_str(key);
        s.push_str("=\"");
        escape_label_value(s, val);
        s.push('"');
    }
    let _ = writeln!(s, "}} {value}");
}

/// Escape a label value per the Prometheus text-format spec:
/// `\\` → `\\\\`, `"` → `\\"`, `\n` → `\\n`. Other chars pass through.
#[inline]
fn escape_label_value(s: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            other => s.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_metrics_emits_unlabeled_families_only() {
        let inner = MetricsInner::new();
        let out = encode_text(&inner);
        // Unlabeled counters / gauges always emit (their value is 0).
        assert!(out.contains("# TYPE grx_node_mount_total counter"));
        assert!(out.contains("grx_node_mount_total 0"));
        assert!(out.contains("# TYPE grx_queue_depth_messages gauge"));
        assert!(out.contains("grx_queue_depth_messages 0"));
        // Labeled families are suppressed when empty.
        assert!(!out.contains("grx_messages_delivered_total{"));
    }

    #[test]
    fn labeled_counter_emits_one_line_per_tuple() {
        let inner = MetricsInner::new();
        inner.messages_delivered_inc("alice", "bob", "ping");
        inner.messages_delivered_inc("alice", "bob", "ping");
        inner.messages_delivered_inc("alice", "carol", "ping");
        let out = encode_text(&inner);
        assert!(
            out.contains("grx_messages_delivered_total{from=\"alice\",to=\"bob\",type=\"ping\"} 2"),
            "missing alice→bob line in:\n{out}"
        );
        assert!(
            out.contains(
                "grx_messages_delivered_total{from=\"alice\",to=\"carol\",type=\"ping\"} 1"
            ),
            "missing alice→carol line in:\n{out}"
        );
    }

    #[test]
    fn label_values_with_special_chars_get_escaped() {
        let inner = MetricsInner::new();
        // node id with a backslash, quote, and newline — the encoder
        // MUST escape all three to keep the line parseable.
        inner.messages_delivered_inc("a\\b", "c\"d", "e\nf");
        let out = encode_text(&inner);
        // The literal backslash in source `"\\"` represents one `\`
        // char; the encoder doubles it. So the wire bytes are
        // `from="a\\b"` — in this Rust source string we write that
        // as `r#"from="a\\b""#`.
        assert!(out.contains(r#"from="a\\b""#), "backslash escape: {out}");
        assert!(out.contains(r#"to="c\"d""#), "quote escape: {out}");
        assert!(out.contains(r#"type="e\nf""#), "newline escape: {out}");
    }

    #[test]
    fn histogram_emits_bucket_sum_and_count() {
        let inner = MetricsInner::new();
        inner.process_batch_duration.observe(2_000); // 2µs
        inner.process_batch_duration.observe(50_000); // 50µs
        inner.process_batch_duration.observe(2_000_000); // 2ms
        let out = encode_text(&inner);
        assert!(out.contains("# TYPE grx_process_batch_duration_seconds histogram"));
        // First bucket is 4µs = 4e-6 seconds.
        assert!(out.contains("grx_process_batch_duration_seconds_bucket{le=\"0.000004\"} 1"));
        // +Inf bucket equals total count.
        assert!(out.contains("grx_process_batch_duration_seconds_bucket{le=\"+Inf\"} 3"));
        assert!(out.contains("grx_process_batch_duration_seconds_count 3"));
        // _sum uses seconds units (sum_ns / 1e9).
        let expected_sum = (2_000.0 + 50_000.0 + 2_000_000.0) / 1_000_000_000.0;
        assert!(
            out.contains(&format!("grx_process_batch_duration_seconds_sum {expected_sum}")),
            "missing sum line in:\n{out}"
        );
    }

    #[test]
    fn output_is_a_complete_set_of_help_and_type_lines() {
        let inner = MetricsInner::new();
        inner.messages_delivered_inc("a", "b", "c");
        let out = encode_text(&inner);
        // Every emitted family needs both # HELP and # TYPE; check
        // by counting lines for each prefix matches.
        let help_count = out.lines().filter(|l| l.starts_with("# HELP ")).count();
        let type_count = out.lines().filter(|l| l.starts_with("# TYPE ")).count();
        assert_eq!(
            help_count, type_count,
            "HELP/TYPE asymmetry: {help_count} HELPs vs {type_count} TYPEs"
        );
    }
}
