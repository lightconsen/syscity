//! Keyboard input.
//!
//! Events are polled by the event loop rather than read by a background task.
//! That is deliberate: an inline viewport asks the terminal for its cursor
//! position on every draw, and that reply arrives on the same stdin a
//! concurrent reader would be draining — a race the query loses by timing out
//! ("the cursor position could not be read"). Polling only from the loop means
//! nothing else is reading stdin while a frame is drawn.
// INVARIANTS-NONE: pure input adapter; holds no state.

use std::time::Duration;

use crossterm::event::{Event, KeyEventKind};

use crate::tui::actions::TuiAction;

/// Poll for a single input action.
///
/// Non-blocking: returns `None` when nothing is pending, so the caller can
/// drive it from inside its own loop.
pub fn poll_action() -> Option<TuiAction> {
    if !crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
        return None;
    }
    match crossterm::event::read() {
        // Key *release* and repeat events are ignored: acting on them would
        // double every keystroke on terminals that report both.
        Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
            Some(TuiAction::from_key_event(key))
        }
        Ok(Event::Key(_)) => None,
        Ok(Event::Resize(cols, rows)) => Some(TuiAction::Resize(cols, rows)),
        Ok(_) => None,
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing is queued when no terminal is attached to the test harness, and
    /// polling must not block waiting for input that will never come.
    #[test]
    fn polling_without_input_returns_immediately() {
        let start = std::time::Instant::now();
        let action = poll_action();
        assert!(start.elapsed() < Duration::from_millis(500), "poll_action must not block");
        // With no terminal attached there is nothing to read, but the call must
        // still return rather than wait for input that will never arrive.
        let _ = action;
    }
}
