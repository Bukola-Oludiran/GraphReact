//! `EventSource` trait and built-in factories.
//!
//! M4 ships timer + delay sources via `tokio::time`. HTTP and file-watch
//! sources land in NAPI/integration crates where the relevant ecosystem
//! deps live (don't bloat core's compile graph for browser/WASM targets).

use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;

/// External events flowing into the graph as messages.
///
/// `start` is called on subscription; the source pushes events through the
/// supplied closure. `stop` halts production and releases resources.
pub trait EventSource: Send + Sync {
    /// Stable identifier for this source instance.
    fn id(&self) -> &str;
    /// Begin producing events. Implementations call `push(type, payload)`
    /// for each event; the scheduler routes those into the graph.
    fn start(&self, push: Arc<dyn Fn(&str, Value) + Send + Sync>);
    /// Stop producing events and release resources.
    fn stop(&self);
}

/// Factory for an [`EventSource`] — what `useSource` accepts.
pub type EventSourceFactory = Arc<dyn Fn() -> Arc<dyn EventSource> + Send + Sync>;

static SOURCE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_id(prefix: &str) -> String {
    let n = SOURCE_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{prefix}-{n}")
}

// ---- Timer source ----

/// Repeating timer — emits `tick` events at `interval_ms` cadence.
#[must_use]
pub fn timer_source(interval_ms: u64) -> Arc<dyn EventSource> {
    Arc::new(TimerSource {
        id: next_id("timer"),
        interval_ms,
        handle: parking_lot::Mutex::new(None),
    })
}

struct TimerSource {
    id: String,
    interval_ms: u64,
    handle: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

impl EventSource for TimerSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self, push: Arc<dyn Fn(&str, Value) + Send + Sync>) {
        let interval = self.interval_ms;
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval));
            // Skip the immediate first tick — match TS `setInterval` semantics.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                push("tick", serde_json::json!({}));
            }
        });
        *self.handle.lock() = Some(handle);
    }

    fn stop(&self) {
        if let Some(h) = self.handle.lock().take() {
            h.abort();
        }
    }
}

// ---- Delay source ----

/// One-shot delay — emits `timeout` once after `delay_ms` then completes.
#[must_use]
pub fn delay_source(delay_ms: u64) -> Arc<dyn EventSource> {
    Arc::new(DelaySource {
        id: next_id("delay"),
        delay_ms,
        handle: parking_lot::Mutex::new(None),
    })
}

struct DelaySource {
    id: String,
    delay_ms: u64,
    handle: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

impl EventSource for DelaySource {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self, push: Arc<dyn Fn(&str, Value) + Send + Sync>) {
        let delay = self.delay_ms;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            push("timeout", serde_json::json!({}));
        });
        *self.handle.lock() = Some(handle);
    }

    fn stop(&self) {
        if let Some(h) = self.handle.lock().take() {
            h.abort();
        }
    }
}

/// Reset the shared source counter — for deterministic tests only.
pub fn reset_source_counter() {
    SOURCE_COUNTER.store(0, Ordering::Relaxed);
}
