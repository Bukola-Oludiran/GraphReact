//! Message creation and TTL handling.

use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::constants::DEFAULT_TTL;
use super::types::Message;

static MESSAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Build a [`Message`] with monotonically-increasing ID and current timestamp.
pub fn create_message(
    r#type: impl Into<String>,
    from: impl Into<String>,
    to: impl Into<String>,
    payload: Value,
    ttl: Option<u32>,
) -> Message {
    let counter = MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| {
            let millis: u128 = d.as_millis();
            u64::try_from(millis).unwrap_or(u64::MAX)
        })
        .unwrap_or(0);
    Message {
        id: format!("msg_{counter}"),
        r#type: r#type.into(),
        from: from.into(),
        to: to.into(),
        payload,
        ttl: ttl.unwrap_or(DEFAULT_TTL),
        timestamp,
        direct: false,
        // **M16-Backend P10.** Local-origin messages have no remote
        // OTel context. The dist crate's `dispatch_inbound` populates
        // this field when a message arrives from a remote peer with
        // OTel tracing enabled.
        trace_context: None,
        // **M-Ingress phase 2.** Locally-emitted messages are not
        // part of an HTTP call flow by default. The runtime's
        // auto-propagation logic in `Scheduler::deliver` overlays
        // the inbound correlation_id onto the first matching
        // outgoing emit; explicit ingress entrypoints
        // (`SchedulerHandle::send_ingress_with_correlation`) set
        // it directly on construction.
        correlation_id: None,
    }
}

/// Decrement the message's TTL in place. Returns `true` if the message can
/// still be delivered (TTL above zero), `false` if it should be dropped.
pub fn decrement_ttl(msg: &mut Message) -> bool {
    msg.ttl = msg.ttl.saturating_sub(1);
    msg.ttl > 0
}

/// Reset the global message counter — for deterministic tests.
pub fn reset_message_counter() {
    MESSAGE_COUNTER.store(0, Ordering::Relaxed);
}
