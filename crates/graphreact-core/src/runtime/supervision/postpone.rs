//! gen_statem-style selective receive via postpone queue.
//!
//! When a message handler invokes [`postpone`], the active message is
//! appended to the active node's postpone queue. On the next
//! re-evaluation (any state change the scheduler observes), postponed
//! messages are drained back into the main message queue so the new
//! handler set sees them.
//!
//! The thread-local [`POSTPONE_FLAG`] is set by the hook and consumed
//! by the scheduler's `deliver()` after the handler returns.

use std::cell::Cell;

use crate::runtime::types::Message;

thread_local! {
    /// `true` if the current message handler called [`postpone`]. Read
    /// and cleared by the scheduler after each handler invocation.
    pub(crate) static POSTPONE_FLAG: Cell<bool> = const { Cell::new(false) };
}

/// Defer the message currently being delivered. The scheduler replays
/// it after the next state transition the active node observes (any
/// re-evaluation triggered by `Signal::set` or topology change).
///
/// Calls outside a message handler are no-ops — the flag is only
/// consumed by the scheduler's delivery path.
pub fn postpone() {
    POSTPONE_FLAG.with(|c| c.set(true));
}

/// Internal — read and clear the postpone flag. Called by the
/// scheduler after each handler runs.
pub(crate) fn take_postpone_flag() -> bool {
    POSTPONE_FLAG.with(|c| {
        let v = c.get();
        c.set(false);
        v
    })
}

/// Per-node FIFO of deferred messages.
#[derive(Debug, Default)]
pub struct PostponeQueue {
    pending: Vec<Message>,
}

impl PostponeQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Defer `msg`. The handler returned without consuming it; the
    /// scheduler will replay it on the next state transition.
    pub fn push(&mut self, msg: Message) {
        self.pending.push(msg);
    }

    /// Drain every postponed message. Caller is the scheduler at
    /// state-transition time.
    pub fn drain(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.pending)
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<Message> {
        self.pending.clone()
    }

    /// Whether there are any pending messages.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Number of postponed messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Clear without replay — used on unmount.
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::message::create_message;

    #[test]
    fn postpone_queue_is_fifo() {
        let mut q = PostponeQueue::new();
        q.push(create_message("a", "x", "y", serde_json::Value::Null, None));
        q.push(create_message("b", "x", "y", serde_json::Value::Null, None));
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].r#type, "a");
        assert_eq!(drained[1].r#type, "b");
        assert!(q.is_empty());
    }
}
