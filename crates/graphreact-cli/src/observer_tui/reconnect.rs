//! Exponential-backoff schedule for the polling task's reconnect
//! loop. Plan-locked: cap at 30s. Resets to the floor on every
//! successful fetch round.

use std::time::Duration;

/// Floor + ceiling + multiplier for the backoff schedule. Plan
/// lockin: floor 250ms, ceiling 30s, multiplier 2.
const FLOOR_MS: u64 = 250;
const CEILING_MS: u64 = 30_000;
const MULTIPLIER: u64 = 2;

/// Stateful exponential backoff. Construct once per polling task;
/// call `bump()` after each failure to advance the delay; call
/// `reset()` after each success.
#[derive(Debug, Clone)]
pub struct Backoff {
    next_delay_ms: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    pub fn new() -> Self {
        Self {
            next_delay_ms: FLOOR_MS,
        }
    }

    /// The duration we'll sleep before the next attempt.
    pub fn current(&self) -> Duration {
        Duration::from_millis(self.next_delay_ms)
    }

    /// The current delay in milliseconds — fed to
    /// [`crate::observer_tui::state::Connection::Disconnected`] for
    /// the status-bar countdown.
    pub fn current_ms(&self) -> u64 {
        self.next_delay_ms
    }

    /// Advance to the next backoff slot after a failure.
    pub fn bump(&mut self) {
        self.next_delay_ms = (self.next_delay_ms.saturating_mul(MULTIPLIER)).min(CEILING_MS);
    }

    /// Reset to the floor after a successful poll.
    pub fn reset(&mut self) {
        self.next_delay_ms = FLOOR_MS;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_to_ceiling_within_a_few_steps() {
        let mut b = Backoff::new();
        assert_eq!(b.current_ms(), FLOOR_MS);
        b.bump();
        assert_eq!(b.current_ms(), 500);
        b.bump();
        assert_eq!(b.current_ms(), 1000);
        for _ in 0..20 {
            b.bump();
        }
        assert_eq!(b.current_ms(), CEILING_MS);
    }

    #[test]
    fn reset_returns_to_floor() {
        let mut b = Backoff::new();
        for _ in 0..10 {
            b.bump();
        }
        assert_eq!(b.current_ms(), CEILING_MS);
        b.reset();
        assert_eq!(b.current_ms(), FLOOR_MS);
    }
}
