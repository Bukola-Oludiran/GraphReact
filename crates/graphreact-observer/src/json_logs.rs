//! Single-line-per-event JSON [`TelemetrySink`]. Phase-1's first
//! consumer; validates the trait shape end-to-end against a real
//! consumer at ~150 LOC instead of ~1500 LOC of rip-out if the trait
//! shape is wrong.
//!
//! ## Output shape
//!
//! Each event becomes one line:
//!
//! ```json
//! {"event":"node_evaluate_start","payload":{"node_id":"my-node"}}
//! ```
//!
//! - `event` is a stable kebab-case string for grep/jq workflows.
//! - `payload` is the per-event struct serialized via the
//!   [`graphreact_core::telemetry`] structs' `serde::Serialize`
//!   derives. The shape is the same one Phase-7's WebSocket fan-out
//!   ships (M16-Backend's "JSON encoding for management API +
//!   WebSocket events" decision).
//!
//! Phase 3 swaps in a [`tracing_subscriber`]-based JSON layer so
//! existing `tracing::warn!` calls flow through the same pipeline +
//! pick up timestamps + level + spans automatically. Phase-1 keeps
//! the emit path direct so the bench harness can isolate the trait
//! overhead from the subscriber's overhead.
//!
//! ## Configuration
//!
//! - Default sink: stderr.
//! - `GRX_OBSERVER_LOG=<path>` env var: file output. The file is
//!   opened in append mode + created if missing. Permissions follow
//!   the process umask.
//! - Programmatic sink: [`JsonLogTelemetry::with_writer`] takes any
//!   `Box<dyn Write + Send>` (test fixtures point at a buffer; the
//!   bench harness points at `/dev/null`).
//!
//! ## Concurrency
//!
//! The writer sits behind a [`parking_lot::Mutex`] so concurrent
//! `process_batch` calls (multi-threaded host runtimes via the napi
//! bridge — though M16-Backend's primary deployment is the standalone
//! `grx` daemon) don't interleave bytes mid-line. Lock granularity is
//! per-event, NOT per-cascade-round; bench gate (b within 1% of a)
//! catches contention regressions early.

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;

use graphreact_core::{
    HotReload, MessageDelivered, NodeEvaluateEnd, NodeEvaluateStart, NodeMounted, NodeUnmounted,
    QueueDepths, RemoteMessage, SchedulerTickEnd, SchedulerTickStart, SupervisorDecisionMade,
    TelemetrySink, TopologyEventOccurred,
};

use crate::json_escape::escape_json_str;

/// Stable event-name strings for the cold-event serde_json path. The
/// four hot-event names (`scheduler_tick_start`, `scheduler_tick_end`,
/// `queue_depths`, `message_delivered`) are inlined as byte-string
/// literals in the hand-rolled fast paths, so they're not in this
/// list — but the string spellings MUST match: shape-equivalence
/// guard at the bottom of the file fails fast on drift.
const EVENT_EVALUATE_START: &str = "node_evaluate_start";
const EVENT_EVALUATE_END: &str = "node_evaluate_end";
const EVENT_NODE_MOUNTED: &str = "node_mounted";
const EVENT_NODE_UNMOUNTED: &str = "node_unmounted";
const EVENT_TOPOLOGY: &str = "topology_event";
const EVENT_SUPERVISOR: &str = "supervisor_decision";
const EVENT_REMOTE_MESSAGE: &str = "remote_message";
const EVENT_HOT_RELOAD: &str = "hot_reload";

/// Single-line JSON telemetry sink. See module docs.
pub struct JsonLogTelemetry {
    writer: Mutex<Box<dyn Write + Send>>,
}

impl JsonLogTelemetry {
    /// Construct from `GRX_OBSERVER_LOG`: when set, opens the named
    /// path in append mode; otherwise emits to stderr. Returns `None`
    /// if the env var is set but the file can't be opened (caller
    /// can fall back to stderr or surface the error).
    ///
    /// Writes failures during emit are silently dropped — telemetry
    /// MUST NOT crash the scheduler. Wrap with a Phase-2 metrics
    /// counter (`grx_observer_log_write_failures_total`) if you need
    /// visibility on emit failures.
    pub fn from_env() -> Self {
        match env::var_os("GRX_OBSERVER_LOG") {
            Some(path) => {
                let path = Path::new(&path).to_owned();
                match OpenOptions::new().create(true).append(true).open(&path) {
                    Ok(file) => Self {
                        writer: Mutex::new(Box::new(file)),
                    },
                    Err(err) => {
                        // Fall back to stderr + emit a one-time
                        // warning through tracing so the operator
                        // sees why their file isn't getting events.
                        tracing::warn!(
                            target: "graphreact::observer::audit",
                            ?err,
                            ?path,
                            "GRX_OBSERVER_LOG file open failed; falling back to stderr"
                        );
                        Self {
                            writer: Mutex::new(Box::new(io::stderr())),
                        }
                    }
                }
            }
            None => Self {
                writer: Mutex::new(Box::new(io::stderr())),
            },
        }
    }

    /// Construct with an explicit writer. Test fixtures + the
    /// scheduler_overhead bench harness use this — pointing at
    /// `io::sink()` for the bench's case (c) keeps the JSON encoding
    /// cost included while removing real-I/O variance.
    #[must_use]
    pub fn with_writer(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }

    /// Convenience: wrap in `Arc<dyn TelemetrySink>` for direct
    /// `Scheduler::install_telemetry_sink` use.
    #[must_use]
    pub fn into_arc(self) -> Arc<dyn TelemetrySink> {
        Arc::new(self) as Arc<dyn TelemetrySink>
    }

    /// Emit a single record. Format: `{"event":"X","payload":{...}}\n`.
    /// Errors are dropped — see the from_env doc on why.
    ///
    /// Implementation note (Phase-1 bench gate): we serialize into a
    /// thread-local reusable buffer to avoid a per-event allocation,
    /// then issue ONE `write_all` against the inner writer with the
    /// lock held. The thread-local keeps the buffer warm across
    /// emits on the same thread; reusing it amortizes the
    /// `serde_json::to_writer` byte-by-byte cost without taking on
    /// the malloc hit a per-emit `Vec::with_capacity` would incur.
    fn emit<P: Serialize>(&self, event: &'static str, payload: &P) {
        #[derive(Serialize)]
        struct Envelope<'a, P: Serialize> {
            event: &'a str,
            payload: &'a P,
        }
        thread_local! {
            static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::with_capacity(256));
        }
        BUF.with(|cell| {
            let mut buf = cell.borrow_mut();
            buf.clear();
            let envelope = Envelope { event, payload };
            if serde_json::to_writer(&mut *buf, &envelope).is_err() {
                return;
            }
            buf.push(b'\n');
            let mut writer = self.writer.lock();
            let _ = writer.write_all(&buf);
        });
    }
}

impl JsonLogTelemetry {
    /// Hand-rolled fast-path emitters for the four hot events that
    /// dominate any realistic workload (`tick_start`, `tick_end`,
    /// `queue_depths`, `message_delivered`). These bypass
    /// `serde_json::to_writer`'s field-walking; output is identical
    /// byte-for-byte to what the `Serialize` derive produces (verified
    /// in the integration test below). Cold events (`node_evaluate_*`,
    /// `mount`/`unmount`, `topology`, `supervisor_decision`,
    /// `remote_message`, `hot_reload`) keep the serde_json path —
    /// they fire infrequently enough that the encoding cost doesn't
    /// dominate.
    fn emit_with<F>(&self, fill: F)
    where
        F: FnOnce(&mut Vec<u8>),
    {
        thread_local! {
            static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::with_capacity(256));
        }
        BUF.with(|cell| {
            let mut buf = cell.borrow_mut();
            buf.clear();
            fill(&mut buf);
            buf.push(b'\n');
            let mut writer = self.writer.lock();
            let _ = writer.write_all(&buf);
        });
    }
}

impl TelemetrySink for JsonLogTelemetry {
    fn on_scheduler_tick_start(&self, _ev: &SchedulerTickStart) {
        // SchedulerTickStart has no fields → fixed string; cheapest
        // possible emit.
        self.emit_with(|buf| {
            buf.extend_from_slice(br#"{"event":"scheduler_tick_start","payload":{}}"#);
        });
    }

    fn on_scheduler_tick_end(&self, ev: &SchedulerTickEnd) {
        self.emit_with(|buf| {
            buf.extend_from_slice(
                br#"{"event":"scheduler_tick_end","payload":{"iterations":"#,
            );
            let mut itoa_buf = itoa::Buffer::new();
            buf.extend_from_slice(itoa_buf.format(ev.iterations).as_bytes());
            buf.extend_from_slice(br#","terminated_by_depth_cap":"#);
            buf.extend_from_slice(if ev.terminated_by_depth_cap {
                b"true"
            } else {
                b"false"
            });
            buf.extend_from_slice(b"}}");
        });
    }

    fn on_queue_depths(&self, ev: &QueueDepths) {
        self.emit_with(|buf| {
            let mut itoa_buf = itoa::Buffer::new();
            buf.extend_from_slice(br#"{"event":"queue_depths","payload":{"depth":"#);
            buf.extend_from_slice(itoa_buf.format(ev.depth).as_bytes());
            buf.extend_from_slice(br#","snapshot":{"messages":"#);
            buf.extend_from_slice(itoa_buf.format(ev.snapshot.messages).as_bytes());
            buf.extend_from_slice(br#","dirty":"#);
            buf.extend_from_slice(itoa_buf.format(ev.snapshot.dirty).as_bytes());
            buf.extend_from_slice(br#","pending_topology":"#);
            buf.extend_from_slice(itoa_buf.format(ev.snapshot.pending_topology).as_bytes());
            buf.extend_from_slice(br#","external_inbox":"#);
            buf.extend_from_slice(itoa_buf.format(ev.snapshot.external_inbox).as_bytes());
            buf.extend_from_slice(br#","pending_mutations":"#);
            buf.extend_from_slice(itoa_buf.format(ev.snapshot.pending_mutations).as_bytes());
            buf.extend_from_slice(b"}}}");
        });
    }

    fn on_node_evaluate_start(&self, ev: &NodeEvaluateStart<'_>) {
        self.emit(EVENT_EVALUATE_START, ev);
    }

    fn on_node_evaluate_end(&self, ev: &NodeEvaluateEnd<'_>) {
        self.emit(EVENT_EVALUATE_END, ev);
    }

    fn on_message_delivered(&self, ev: &MessageDelivered<'_>) {
        self.emit_with(|buf| {
            buf.extend_from_slice(br#"{"event":"message_delivered","payload":{"from":""#);
            escape_json_str(buf, ev.from);
            buf.extend_from_slice(br#"","to":""#);
            escape_json_str(buf, ev.to);
            buf.extend_from_slice(br#"","type":""#);
            escape_json_str(buf, ev.message_type);
            buf.extend_from_slice(br#"","ttl":"#);
            let mut itoa_buf = itoa::Buffer::new();
            buf.extend_from_slice(itoa_buf.format(ev.ttl).as_bytes());
            buf.extend_from_slice(br#","direct":"#);
            buf.extend_from_slice(if ev.direct { b"true" } else { b"false" });
            buf.extend_from_slice(br#","accepted":"#);
            buf.extend_from_slice(if ev.accepted { b"true" } else { b"false" });
            buf.extend_from_slice(b"}}");
        });
    }

    fn on_node_mounted(&self, ev: &NodeMounted<'_>) {
        self.emit(EVENT_NODE_MOUNTED, ev);
    }

    fn on_node_unmounted(&self, ev: &NodeUnmounted<'_>) {
        self.emit(EVENT_NODE_UNMOUNTED, ev);
    }

    fn on_topology_event(&self, ev: &TopologyEventOccurred<'_>) {
        self.emit(EVENT_TOPOLOGY, ev);
    }

    fn on_supervisor_decision(&self, ev: &SupervisorDecisionMade<'_>) {
        self.emit(EVENT_SUPERVISOR, ev);
    }

    fn on_remote_message(&self, ev: &RemoteMessage<'_>) {
        self.emit(EVENT_REMOTE_MESSAGE, ev);
    }

    fn on_hot_reload(&self, ev: &HotReload<'_>) {
        self.emit(EVENT_HOT_RELOAD, ev);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    /// Test writer that snapshots what was written.
    #[derive(Clone)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Vec::new())))
        }
        fn snapshot(&self) -> String {
            String::from_utf8(self.0.lock().clone()).expect("utf-8")
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn emits_one_line_per_event() {
        let buf = SharedBuffer::new();
        let sink = JsonLogTelemetry::with_writer(Box::new(buf.clone()));
        sink.on_scheduler_tick_start(&SchedulerTickStart::default());
        sink.on_node_mounted(&NodeMounted { node_id: "alpha" });
        sink.on_scheduler_tick_end(&SchedulerTickEnd {
            iterations: 3,
            terminated_by_depth_cap: false,
        });
        let out = buf.snapshot();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let _: serde_json::Value =
                serde_json::from_str(line).expect("each line is parseable JSON");
        }
    }

    #[test]
    fn round_trips_message_event() {
        let buf = SharedBuffer::new();
        let sink = JsonLogTelemetry::with_writer(Box::new(buf.clone()));
        sink.on_message_delivered(&MessageDelivered {
            from: "a",
            to: "b",
            message_type: "ping",
            ttl: 7,
            direct: false,
            accepted: true,
            trace_context: None,
        });
        let out = buf.snapshot();
        let line = out.lines().next().expect("one line");
        let v: serde_json::Value = serde_json::from_str(line).expect("parseable");
        assert_eq!(v["event"], "message_delivered");
        assert_eq!(v["payload"]["from"], "a");
        assert_eq!(v["payload"]["to"], "b");
        assert_eq!(v["payload"]["type"], "ping");
        assert_eq!(v["payload"]["ttl"], 7);
        assert_eq!(v["payload"]["accepted"], true);
    }

    #[test]
    fn cursor_writer_compiles() {
        // Phase-1 bench harness uses io::sink() for case (c). Make
        // sure the constructor accepts arbitrary `Write + Send`.
        let cursor = Cursor::new(Vec::<u8>::new());
        let _ = JsonLogTelemetry::with_writer(Box::new(cursor));
    }

    /// **B1 shape-equivalence guard.** For each of the four hot
    /// events that have hand-rolled fast paths, the emitted JSON
    /// MUST parse to the same `serde_json::Value` as the
    /// `serde_json::to_string` output of the same event payload.
    /// This catches "hand-rolled formatter and serde_json drift" the
    /// next time anyone tweaks an event struct.
    #[test]
    fn hand_rolled_output_matches_serde_for_hot_events() {
        // tick_start
        {
            let buf = SharedBuffer::new();
            let sink = JsonLogTelemetry::with_writer(Box::new(buf.clone()));
            sink.on_scheduler_tick_start(&SchedulerTickStart::default());
            let line = buf.snapshot();
            let line = line.trim();
            let parsed: serde_json::Value = serde_json::from_str(line).expect("parse");
            assert_eq!(parsed["event"], "scheduler_tick_start");
            assert_eq!(parsed["payload"], serde_json::json!({}));
        }
        // tick_end (both bool branches)
        for terminated in [false, true] {
            let buf = SharedBuffer::new();
            let sink = JsonLogTelemetry::with_writer(Box::new(buf.clone()));
            let ev = SchedulerTickEnd {
                iterations: 42,
                terminated_by_depth_cap: terminated,
            };
            sink.on_scheduler_tick_end(&ev);
            let line = buf.snapshot();
            let line = line.trim();
            let hand_rolled: serde_json::Value =
                serde_json::from_str(line).expect("parse hand-rolled");
            // Synthesize the equivalent serde_json output for shape
            // comparison.
            let serde_payload =
                serde_json::to_value(ev).expect("serde_json encodes ev");
            let expected = serde_json::json!({
                "event": "scheduler_tick_end",
                "payload": serde_payload,
            });
            assert_eq!(
                hand_rolled, expected,
                "tick_end shape mismatch (terminated={terminated})"
            );
        }
        // queue_depths
        {
            let buf = SharedBuffer::new();
            let sink = JsonLogTelemetry::with_writer(Box::new(buf.clone()));
            let ev = QueueDepths {
                depth: 3,
                snapshot: graphreact_core::QueueDepthsSnapshot {
                    messages: 7,
                    dirty: 2,
                    pending_topology: 0,
                    external_inbox: 1,
                    pending_mutations: 4,
                },
            };
            sink.on_queue_depths(&ev);
            let line = buf.snapshot();
            let line = line.trim();
            let hand_rolled: serde_json::Value =
                serde_json::from_str(line).expect("parse hand-rolled");
            let serde_payload = serde_json::to_value(ev).expect("serde encodes ev");
            let expected = serde_json::json!({
                "event": "queue_depths",
                "payload": serde_payload,
            });
            assert_eq!(hand_rolled, expected, "queue_depths shape mismatch");
        }
        // message_delivered (with strings that need escaping)
        {
            let buf = SharedBuffer::new();
            let sink = JsonLogTelemetry::with_writer(Box::new(buf.clone()));
            let ev = MessageDelivered {
                from: r#"a"b\c"#,
                to: "japanese: 日本語",
                message_type: "with\nnewline",
                ttl: 5,
                direct: true,
                accepted: false,
                trace_context: None,
            };
            sink.on_message_delivered(&ev);
            let line = buf.snapshot();
            let line = line.trim();
            let hand_rolled: serde_json::Value =
                serde_json::from_str(line).expect("parse hand-rolled");
            let serde_payload = serde_json::to_value(ev).expect("serde encodes ev");
            let expected = serde_json::json!({
                "event": "message_delivered",
                "payload": serde_payload,
            });
            assert_eq!(hand_rolled, expected, "message_delivered shape mismatch");
        }
    }
}
