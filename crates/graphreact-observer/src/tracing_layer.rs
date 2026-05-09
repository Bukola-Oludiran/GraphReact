//! **M16-Backend Phase 3.** `tracing_subscriber::Layer` that captures
//! every `tracing::*` event the workspace emits (scheduler supervisor
//! escalation at scheduler.rs:2561; `graphreact_dist::reload` events
//! across the cluster reload path; `graphreact::observer::audit`
//! events from P2's HTTP server) and writes them as single-line
//! JSON in the Loki/Elastic-friendly schema.
//!
//! ## Output schema
//!
//! ```json
//! {"ts":"2026-05-07T12:34:56.789Z","level":"WARN","target":"graphreact_dist::reload","name":"event src/cluster.rs:634","fields":{"message":"...","peer":"alice","..."},"span":{"name":"reload","id":"..."}}
//! ```
//!
//! Stable field order (`ts` → `level` → `target` → `name` →
//! `fields` → `span`) so `jq` / `grep` workflows stay deterministic.
//! Span context is included only when the event fires inside one
//! (most workspace events fire at the top level, so the `span` key
//! is omitted via `serde(skip_serializing_if = "Option::is_none")`).
//!
//! ## Why this lives alongside [`crate::JsonLogTelemetry`]
//!
//! P1's [`crate::JsonLogTelemetry`] receives [`crate::TelemetrySink`]
//! callbacks — events from inside the scheduler/dist/hotreload hot
//! paths. THIS layer receives `tracing::*` macro calls — pre-existing
//! workspace logging, audit events, debug breadcrumbs. Both emit
//! single-line JSON to (potentially) the same writer; operators see
//! one unified stream rather than two parallel ones.
//!
//! The two schemas are deliberately distinct:
//! - JsonLogTelemetry: `{"event":"X","payload":{...}}` (telemetry-event-shaped)
//! - TracingLayer:     `{"ts":"...","level":"...","target":"...","fields":{...}}` (Loki/Elastic-shaped)
//!
//! Operators distinguish them by presence of `event` vs `ts` keys.
//! Both are valid scrape inputs for log aggregators.
//!
//! ## Hot-path discipline
//!
//! `Layer::on_event` runs for EVERY `tracing::*` macro call across
//! the workspace. Inside the scheduler hot path the only firing site
//! is `scheduler.rs:2561` (supervisor escalation) — fires rarely, so
//! per-event encoding cost doesn't dominate. Bench gate (b within 1%
//! of a) re-runs after this phase to catch regressions IF a future
//! refactor sticks `tracing::trace!` into the inner cascade loop.

use std::io::Write;
use std::sync::Mutex;
use std::time::SystemTime;

use parking_lot::RwLock as PLRwLock;
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Maximum number of structured fields captured per event. Caps
/// log-line growth in pathological cases (e.g. an event that
/// accidentally includes a 200-field debug-snapshot). 32 is well
/// above any current call site.
const MAX_FIELDS_PER_EVENT: usize = 32;

/// Type-erased writer the layer holds behind a `Mutex`. We use a
/// `Box<dyn Write + Send>` rather than a generic `W` parameter so
/// the layer is `dyn`-compatible — `tracing_subscriber::Registry`
/// stores layers as type-erased `Vec<Box<dyn Layer<S>>>` when more
/// than one is attached, and a generic `W` would require a separate
/// `with_writer<W>` builder dance.
type SharedWriter = Mutex<Box<dyn Write + Send>>;

/// `tracing_subscriber::Layer` that emits one JSON line per
/// captured event to a configured writer. Stateless except for the
/// writer mutex.
pub struct TracingJsonLayer {
    writer: SharedWriter,
}

impl TracingJsonLayer {
    /// Construct with an explicit writer. Use [`stderr_writer`] for
    /// the conventional default; pass a `File` for append-mode log
    /// output; pass `io::sink()` in the bench harness.
    #[must_use]
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }

    /// Convenience: construct writing to stderr.
    #[must_use]
    pub fn to_stderr() -> Self {
        Self::new(Box::new(std::io::stderr()))
    }
}

/// Builder helper exposed at crate root for the conventional
/// `Observer::install_json_logs_subscriber()` form. See
/// [`crate::install_json_logs_subscriber`].
#[must_use]
pub fn stderr_writer() -> Box<dyn Write + Send> {
    Box::new(std::io::stderr())
}

impl<S> Layer<S> for TracingJsonLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = metadata.level();
        let target = metadata.target();
        let name = metadata.name();

        // Capture the event's structured fields via the Visit
        // pattern. Each field name + formatted-value pair gets
        // pushed onto a small bounded SmallVec analogue — we use a
        // plain `Vec` with a heuristic capacity so common cases
        // (1-3 fields per event) don't allocate beyond initial.
        let mut visitor = JsonFieldVisitor::with_capacity(8);
        event.record(&mut visitor);

        // Span context: walk the current scope for the closest
        // ancestor span name + id. Workspace tracing usage doesn't
        // currently use spans — most events fire at top level — so
        // this lookup short-circuits to None on the first iteration.
        let span_meta = ctx.event_span(event).map(|span| SpanRef {
            name: span.name(),
            id: span.id().into_u64(),
        });

        // Format timestamp. RFC3339 with millisecond precision —
        // the format Loki/Elastic ingest cleanly. SystemTime::now()
        // is monotonic-ish across the lifetime of one process and
        // matches the convention of every other JSON-log tool.
        let ts = format_rfc3339_millis(SystemTime::now());

        let record = LogRecord {
            ts: &ts,
            level: level.as_str(),
            target,
            name,
            fields: &visitor.fields,
            span: span_meta,
        };

        // Serialize directly into a thread-local buffer + write
        // under the mutex in a single call. Mirrors P1's
        // `JsonLogTelemetry::emit_with` to keep the encoder hot
        // path allocation-bounded.
        thread_local! {
            static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::with_capacity(512));
        }
        BUF.with(|cell| {
            let mut buf = cell.borrow_mut();
            buf.clear();
            if serde_json::to_writer(&mut *buf, &record).is_err() {
                return;
            }
            buf.push(b'\n');
            // Mutex.lock() poisoning is a panic; treat as best-
            // effort — telemetry must NEVER panic the program.
            if let Ok(mut writer) = self.writer.lock() {
                let _ = writer.write_all(&buf);
            }
        });
    }
}

#[derive(Serialize)]
struct LogRecord<'a> {
    ts: &'a str,
    level: &'a str,
    target: &'a str,
    name: &'a str,
    fields: &'a [(String, String)],
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SpanRef<'a>>,
}

#[derive(Serialize)]
struct SpanRef<'a> {
    name: &'a str,
    id: u64,
}

/// `tracing::field::Visit` impl that captures each field as a
/// `(name, value)` string pair. `Display` is preferred (matches the
/// shape of typical `tracing::warn!("text {var}")` calls); `Debug`
/// is the fallback for non-Display values.
struct JsonFieldVisitor {
    fields: Vec<(String, String)>,
}

impl JsonFieldVisitor {
    fn with_capacity(cap: usize) -> Self {
        Self {
            fields: Vec::with_capacity(cap),
        }
    }

    fn push(&mut self, name: &str, value: String) {
        if self.fields.len() < MAX_FIELDS_PER_EVENT {
            self.fields.push((name.to_string(), value));
        }
    }
}

impl Visit for JsonFieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field.name(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field.name(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field.name(), value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.push(field.name(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field.name(), value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.push(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // Default `Visit` fallback. `tracing::warn!("text", ?var)`
        // and most positional-argument macro forms land here.
        self.push(field.name(), format!("{value:?}"));
    }
}

/// Format a `SystemTime` as RFC3339 with millisecond precision in
/// UTC. Hand-rolled (no `chrono` dep) — about 30 lines of integer
/// math with leap-year handling, fast enough for hot-path tracing
/// emit. The format is fixed-width 24 chars: `YYYY-MM-DDTHH:MM:SS.SSSZ`.
fn format_rfc3339_millis(time: SystemTime) -> String {
    let dur = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let (year, month, day, hour, min, sec) = unix_to_civil(secs);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z"
    )
}

/// Convert UNIX timestamp (seconds) to civil date (Y, M, D, h, m, s)
/// in UTC. Algorithm from Howard Hinnant's "chrono" reference, used
/// by `chrono` and `time` crates internally — exact through 9999
/// CE, no leap-second handling (Prometheus / Loki / Elastic don't
/// honour them either, so the parity is fine).
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn unix_to_civil(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let day_secs: u64 = secs % 86_400;
    let days: i64 = (secs / 86_400) as i64;
    let hour = (day_secs / 3_600) as u32;
    let min = ((day_secs % 3_600) / 60) as u32;
    let sec = (day_secs % 60) as u32;

    // Hinnant: 0000-03-01 is day 0 of the era; January and February
    // are months 11 and 12 of the previous year. UNIX epoch
    // 1970-01-01 is offset 719_468 days from 0000-03-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (y + i64::from(m <= 2)) as u32;
    (year, m, d, hour, min, sec)
}

/// Mutable handle to a [`TracingJsonLayer`]'s writer. Held on the
/// observer side so a daemon can later swap stdout for a file (e.g.
/// SIGHUP-triggered log rotation) without re-installing the
/// subscriber. P3 ships the type but doesn't wire SIGHUP — that's
/// M-Prod #7 territory.
pub struct WriterHandle {
    inner: PLRwLock<Box<dyn Write + Send>>,
}

impl WriterHandle {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            inner: PLRwLock::new(writer),
        }
    }

    /// Replace the underlying writer atomically. In-flight writes
    /// from other threads complete on the OLD writer; subsequent
    /// writes use the new one. Use with care — log records issued
    /// concurrently with a swap may interleave between the two
    /// destinations.
    pub fn replace(&self, writer: Box<dyn Write + Send>) {
        *self.inner.write() = writer;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thread-shared writer the test harness reads back. Holds bytes
    /// behind an `Arc<Mutex<Vec<u8>>>` so the test can clone it +
    /// snapshot the buffer after firing tracing events.
    #[derive(Clone)]
    struct SharedBuffer(std::sync::Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn new() -> Self {
            Self(std::sync::Arc::new(Mutex::new(Vec::new())))
        }
        fn snapshot(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).expect("utf-8")
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[allow(clippy::many_single_char_names)]
    #[test]
    fn unix_to_civil_at_epoch() {
        let (year, month, day, hour, min, sec) = unix_to_civil(0);
        assert_eq!((year, month, day, hour, min, sec), (1970, 1, 1, 0, 0, 0));
    }

    #[allow(clippy::many_single_char_names)]
    #[test]
    fn unix_to_civil_at_known_date() {
        // 2026-05-07T12:34:56Z = 1778157296 (verified via `date -u`).
        let secs = 1_778_157_296;
        let (year, month, day, hour, min, sec) = unix_to_civil(secs);
        assert_eq!((year, month, day, hour, min, sec), (2026, 5, 7, 12, 34, 56));
    }

    #[test]
    fn rfc3339_format_is_24_chars() {
        let s = format_rfc3339_millis(SystemTime::UNIX_EPOCH);
        assert_eq!(s, "1970-01-01T00:00:00.000Z");
        assert_eq!(s.len(), 24);
    }

    #[test]
    fn rfc3339_handles_leap_year_correctly() {
        // 2024-02-29T00:00:00Z is UNIX 1709164800.
        let s = format_rfc3339_millis(
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_709_164_800),
        );
        assert_eq!(s, "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn layer_emits_json_per_tracing_event() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Registry;

        let buf = SharedBuffer::new();
        let layer = TracingJsonLayer::new(Box::new(buf.clone()));
        let subscriber = Registry::default().with(layer);

        with_default(subscriber, || {
            tracing::warn!(
                target: "graphreact::test",
                peer = "alice",
                count = 42_u64,
                "hello {who}",
                who = "world"
            );
        });

        let out = buf.snapshot();
        let line = out.lines().next().expect("at least one line");
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert_eq!(v["level"], "WARN");
        assert_eq!(v["target"], "graphreact::test");
        // Fields preserve message + structured kvs.
        let fields = v["fields"].as_array().expect("fields is array");
        let map: std::collections::HashMap<String, String> = fields
            .iter()
            .map(|el| {
                let arr = el.as_array().unwrap();
                (
                    arr[0].as_str().unwrap().to_string(),
                    arr[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(map.get("message").map(String::as_str), Some("hello world"));
        assert_eq!(map.get("peer").map(String::as_str), Some("alice"));
        assert_eq!(map.get("count").map(String::as_str), Some("42"));
    }

    #[test]
    fn layer_captures_multiple_events() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Registry;

        let buf = SharedBuffer::new();
        let subscriber =
            Registry::default().with(TracingJsonLayer::new(Box::new(buf.clone())));

        with_default(subscriber, || {
            tracing::info!("first");
            tracing::warn!("second");
            tracing::error!("third");
        });

        let out = buf.snapshot();
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        let levels: Vec<_> = lines
            .iter()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["level"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(levels, vec!["INFO", "WARN", "ERROR"]);
    }

    #[test]
    fn ts_field_is_rfc3339() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Registry;

        let buf = SharedBuffer::new();
        let subscriber =
            Registry::default().with(TracingJsonLayer::new(Box::new(buf.clone())));
        with_default(subscriber, || tracing::info!("now"));

        let out = buf.snapshot();
        let line = out.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let ts = v["ts"].as_str().unwrap();
        // `YYYY-MM-DDTHH:MM:SS.SSSZ` — 24 chars, ends in Z, has T at index 10.
        assert_eq!(ts.len(), 24);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[10..11], "T");
    }
}
