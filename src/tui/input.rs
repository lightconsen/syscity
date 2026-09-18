//! Keyboard input, behind a seam the event loop can be tested through.
//!
//! The loop polls rather than receives events on a channel. That is
//! deliberate: an inline viewport asks the terminal for its cursor position on
//! every draw, and that reply arrives on the same stdin a concurrent reader
//! would be draining — a race the query loses by timing out ("the cursor
//! position could not be read"). Polling only from the loop means nothing else
//! reads stdin while a frame is drawn.
//!
//! [`InputSource`] is the seam: production reads crossterm, tests script
//! actions. Which means resize handling, key timing against a slow gateway,
//! and the approval key round-trip can be driven end to end through the real
//! loop without a pty.
// INVARIANTS-NONE: input adapter; holds no state.

use std::time::Duration;

use crossterm::event::{Event, KeyEventKind};

use crate::tui::actions::TuiAction;

/// Where the event loop reads actions from.
pub trait InputSource: Send {
    /// The next action, or `None` when nothing is pending.
    ///
    /// Non-blocking, like the crossterm poll it replaces: the loop calls this
    /// in a drain and then waits on gateway work, not on input.
    fn poll(&mut self) -> Option<TuiAction>;
}

/// The real keyboard: crossterm's event queue.
#[derive(Debug, Default)]
pub struct CrosstermInput;

impl InputSource for CrosstermInput {
    fn poll(&mut self) -> Option<TuiAction> {
        if !crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
            return None;
        }
        match crossterm::event::read() {
            // Key *release* and repeat events are ignored: acting on them
            // would double every keystroke on terminals that report both.
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                Some(TuiAction::from_key_event(key))
            }
            Ok(Event::Key(_)) => None,
            Ok(Event::Resize(cols, rows)) => Some(TuiAction::Resize(cols, rows)),
            Ok(_) => None,
            Err(_) => None,
        }
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
        let action = CrosstermInput.poll();
        assert!(start.elapsed() < Duration::from_millis(500), "poll must not block");
        // With no terminal attached there is nothing to read, but the call must
        // still return rather than wait for input that will never arrive.
        let _ = action;
    }
}
