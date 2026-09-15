//! Reconnect backoff.
//!
//! The TUI is meant to be left running, so losing the gateway must not end the
//! session — but retrying flat out would hammer a gateway that is restarting.
//! The policy is a fixed ladder capped at eight seconds: fast enough that a
//! quick restart is barely noticeable, slow enough not to spam.
// INVARIANTS-NONE: pure policy; holds no shared state.

use std::time::Duration;

/// Delay before each successive reconnect attempt, in milliseconds.
const STEPS_MS: [u64; 5] = [500, 1_000, 2_000, 4_000, 8_000];

/// Tracks how many attempts have been made and how long to wait for the next.
#[derive(Debug, Default, Clone)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    /// A fresh backoff, as if just connected.
    pub fn new() -> Self {
        Self { attempt: 0 }
    }

    /// How many attempts have been made since the last reset.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The delay to wait before the next attempt, advancing the ladder.
    pub fn next_delay(&mut self) -> Duration {
        let idx = (self.attempt as usize).min(STEPS_MS.len() - 1);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(STEPS_MS[idx])
    }

    /// Go back to the start — call this once a connection succeeds.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_the_ladder_then_caps() {
        let mut backoff = Backoff::new();
        let delays: Vec<u64> = (0..7)
            .map(|_| backoff.next_delay().as_millis() as u64)
            .collect();
        assert_eq!(delays, vec![500, 1_000, 2_000, 4_000, 8_000, 8_000, 8_000]);
        assert_eq!(backoff.attempt(), 7);
    }

    #[test]
    fn resets_after_a_successful_connection() {
        let mut backoff = Backoff::new();
        backoff.next_delay();
        backoff.next_delay();
        backoff.reset();
        assert_eq!(backoff.attempt(), 0);
        assert_eq!(backoff.next_delay(), Duration::from_millis(500));
    }
}
