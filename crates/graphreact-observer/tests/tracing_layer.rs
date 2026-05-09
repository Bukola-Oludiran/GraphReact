//! **M16-Backend P3 verification gate.** End-to-end tracing capture:
//! `tracing::warn!` calls fired from outside the observer crate flow
//! through `TracingJsonLayer` and emerge as single-line JSON in the
//! Loki/Elastic shape. Mirrors the actual call sites at
//! `scheduler.rs:2561` (supervisor escalation) +
//! `crates/graphreact-dist/src/cluster.rs` (reload audit) +
//! `crates/graphreact-observer/src/audit.rs` (P2 HTTP audit).

use std::sync::{Arc, Mutex};

use graphreact_observer::{install_json_logs_subscriber, TracingJsonLayer};

/// Test writer that snapshots written bytes. Cloneable so the
/// subscriber-hosting closure can keep one + the assertion code
/// can keep another.
#[derive(Clone)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    fn snapshot(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("utf-8")
    }
}

impl std::io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Mirrors scheduler.rs:2561's `tracing::warn!(...)` shape — a
/// formatted message + named structured fields. The subscriber
/// captures BOTH the message text and the structured kvs.
#[test]
fn warn_with_message_and_fields_preserves_both() {
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    let buf = SharedBuffer::new();
    let subscriber =
        Registry::default().with(TracingJsonLayer::new(Box::new(buf.clone())));

    with_default(subscriber, || {
        tracing::warn!(
            target: "graphreact_dist::reload",
            peer = "alice",
            outcome = "applied",
            duration_ms = 42_u64,
            "remote reload converged"
        );
    });

    let out = buf.snapshot();
    let line = out.lines().next().expect("at least one line");
    let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");

    assert_eq!(v["level"], "WARN");
    assert_eq!(v["target"], "graphreact_dist::reload");

    let fields = v["fields"].as_array().expect("fields array");
    let map: std::collections::HashMap<&str, &str> = fields
        .iter()
        .map(|el| {
            let arr = el.as_array().unwrap();
            (arr[0].as_str().unwrap(), arr[1].as_str().unwrap())
        })
        .collect();
    assert_eq!(map.get("message").copied(), Some("remote reload converged"));
    assert_eq!(map.get("peer").copied(), Some("alice"));
    assert_eq!(map.get("outcome").copied(), Some("applied"));
    assert_eq!(map.get("duration_ms").copied(), Some("42"));
}

/// The `install_json_logs_subscriber` helper builds a subscriber
/// the caller can install globally. Mirrors the typical daemon-
/// startup wiring shape.
#[test]
fn install_helper_returns_usable_subscriber() {
    use tracing::subscriber::with_default;

    let buf = SharedBuffer::new();
    let subscriber = install_json_logs_subscriber(Box::new(buf.clone()));

    with_default(subscriber, || {
        tracing::info!(target: "graphreact::test", "installed");
    });

    let out = buf.snapshot();
    let line = out.lines().next().expect("at least one line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["target"], "graphreact::test");
    assert_eq!(v["level"], "INFO");
}

/// All three logging levels (INFO/WARN/ERROR) emit + the layer
/// preserves their textual representation.
#[test]
fn levels_are_uppercase_strings() {
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    let buf = SharedBuffer::new();
    let subscriber =
        Registry::default().with(TracingJsonLayer::new(Box::new(buf.clone())));

    with_default(subscriber, || {
        tracing::trace!("trace");
        tracing::debug!("debug");
        tracing::info!("info");
        tracing::warn!("warn");
        tracing::error!("error");
    });

    let out = buf.snapshot();
    let levels: Vec<String> = out
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["level"].as_str().unwrap().to_string()
        })
        .collect();
    // tracing's default subscriber-level filter is INFO+ — but we
    // didn't install a filter, so trace/debug come through too.
    // The Registry default has no filter; all 5 levels emit.
    assert!(levels.contains(&"TRACE".to_string()));
    assert!(levels.contains(&"DEBUG".to_string()));
    assert!(levels.contains(&"INFO".to_string()));
    assert!(levels.contains(&"WARN".to_string()));
    assert!(levels.contains(&"ERROR".to_string()));
}

/// Audit events emitted via the `target: "graphreact::observer::audit"`
/// convention used by P2's `audit.rs` flow through cleanly. This
/// mirrors what an operator running the daemon with both the
/// observer HTTP server AND the JSON-log subscriber installed
/// would see.
#[test]
fn audit_target_events_pass_through() {
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    let buf = SharedBuffer::new();
    let subscriber =
        Registry::default().with(TracingJsonLayer::new(Box::new(buf.clone())));

    with_default(subscriber, || {
        // Mirrors graphreact_observer::audit::log_response.
        tracing::info!(
            target: "graphreact::observer::audit",
            peer = "127.0.0.1:54321",
            path = "/metrics",
            status = 200_u64,
            bytes = 1024_u64,
            "observer response"
        );
    });

    let out = buf.snapshot();
    let line = out.lines().next().expect("one line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["target"], "graphreact::observer::audit");
    assert_eq!(v["level"], "INFO");
}

/// Span-aware lookup: when an event fires inside a span, the layer
/// captures the span name + id so log aggregators can correlate
/// events. Workspace tracing usage doesn't currently use spans,
/// but the mechanism must exist for future cross-node correlation
/// (P10 OTel hand-off).
#[test]
fn span_context_is_captured_when_present() {
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    let buf = SharedBuffer::new();
    let subscriber =
        Registry::default().with(TracingJsonLayer::new(Box::new(buf.clone())));

    with_default(subscriber, || {
        let span = tracing::info_span!("reload", peer = "alice");
        let _enter = span.enter();
        tracing::warn!("inside span");
    });

    let out = buf.snapshot();
    let line = out.lines().next().expect("one line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    let span = v["span"].as_object().expect("span object present");
    assert_eq!(span["name"], "reload");
    assert!(span["id"].as_u64().is_some());
}

/// Without a span, the `span` key is omitted (NOT emitted as
/// `null`). Keeps the line short for scrape pipelines that count
/// bytes.
#[test]
fn span_field_omitted_when_event_is_top_level() {
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    let buf = SharedBuffer::new();
    let subscriber =
        Registry::default().with(TracingJsonLayer::new(Box::new(buf.clone())));

    with_default(subscriber, || {
        tracing::info!("top-level");
    });

    let out = buf.snapshot();
    let line = out.lines().next().expect("one line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert!(v.as_object().unwrap().get("span").is_none());
}
