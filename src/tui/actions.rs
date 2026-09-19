//! Keyboard input mapped to user intent.
//!
//! The mapping is deliberately unconditional: a key always produces the same
//! action, and the *mode* decides what that action means (an approval prompt
//! ignores typing, for instance). Keeping the two apart is what stops "y" from
//! meaning three different things depending on where the check lives.
// INVARIANTS-NONE: pure key mapping; holds no state.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// High-level user intent emitted by the input mapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiAction {
    /// Send the current input.
    SendMessage,
    /// Run a slash command (the string includes the leading `/`).
    RunSlashCommand(String),
    /// Append a character to the input.
    InputChar(char),
    /// Insert a newline.
    InputNewline,
    /// Delete the character before the cursor.
    InputBackspace,
    /// Delete the character under the cursor.
    InputDelete,
    /// Move the cursor left.
    CursorLeft,
    /// Move the cursor right.
    CursorRight,
    /// Move to the start of the input.
    CursorHome,
    /// Move to the end of the input.
    CursorEnd,
    /// Move up a line, or recall the previous input.
    CursorUp,
    /// Move down a line, or recall the next input.
    CursorDown,
    /// Cycle to the next completion candidate.
    CompleteNext,
    /// Cycle to the previous completion candidate.
    CompletePrev,
    /// Dismiss the current prompt.
    Escape,
    /// Abort the running turn, or quit when idle.
    Abort,
    /// Quit.
    Quit,
    /// The terminal was resized.
    Resize(u16, u16),
    /// Nothing to do.
    None,
}

impl TuiAction {
    /// Map a crossterm key event to a `TuiAction`.
    pub fn from_key_event(key: KeyEvent) -> Self {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => Self::Abort,
            KeyCode::Char('q') if ctrl => Self::Quit,
            KeyCode::Char('d') if ctrl && key.modifiers.contains(KeyModifiers::SHIFT) => Self::Quit,
            KeyCode::Char('h') if ctrl => Self::RunSlashCommand("/help".to_string()),
            KeyCode::Char('e') if ctrl => Self::RunSlashCommand("/config".to_string()),
            KeyCode::Char('r') if ctrl => Self::RunSlashCommand("/resume".to_string()),
            KeyCode::Esc => Self::Escape,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => Self::InputNewline,
            KeyCode::Enter => Self::SendMessage,
            KeyCode::Up => Self::CursorUp,
            KeyCode::Down => Self::CursorDown,
            KeyCode::Left => Self::CursorLeft,
            KeyCode::Right => Self::CursorRight,
            KeyCode::Home => Self::CursorHome,
            KeyCode::End => Self::CursorEnd,
            KeyCode::Backspace => Self::InputBackspace,
            KeyCode::Delete => Self::InputDelete,
            KeyCode::Tab => Self::CompleteNext,
            KeyCode::BackTab => Self::CompletePrev,
            // A control chord that is not bound above is not text. Without
            // this the catch-all inserted the bare letter, so Ctrl+U put a
            // `u` in the draft — and every other unbound chord its letter.
            //
            // Ctrl+Alt is excluded: on several keyboard layouts that is AltGr,
            // which types real characters (`@`, `\`, `|`). Swallowing those
            // would be a worse bug than the one this fixes.
            KeyCode::Char(_) if ctrl && !key.modifiers.contains(KeyModifiers::ALT) => Self::None,
            KeyCode::Char(c) => Self::InputChar(c),
            _ => Self::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn control_keys_map_to_their_actions() {
        assert_eq!(TuiAction::from_key_event(ctrl('c')), TuiAction::Abort);
        assert_eq!(TuiAction::from_key_event(ctrl('q')), TuiAction::Quit);
        assert_eq!(
            TuiAction::from_key_event(ctrl('r')),
            TuiAction::RunSlashCommand("/resume".to_string())
        );
    }

    /// An unbound control chord does nothing — it must not type its letter.
    #[test]
    fn unbound_control_chords_do_not_insert_text() {
        for c in ['u', 'k', 'w', 'a', 'z', 'l', 'b', 'f', 'n', 'p', 't'] {
            assert_eq!(
                TuiAction::from_key_event(ctrl(c)),
                TuiAction::None,
                "Ctrl+{c} must not reach the composer"
            );
        }
    }

    /// Ctrl+Alt is AltGr on several layouts, where it types real characters.
    /// Swallowing those would break `@` and `\` for those users.
    #[test]
    fn ctrl_alt_still_types_the_character() {
        let mut altgr = ctrl('@');
        altgr.modifiers |= KeyModifiers::ALT;
        assert_eq!(TuiAction::from_key_event(altgr), TuiAction::InputChar('@'));
    }

    #[test]
    fn enter_sends_and_shift_enter_breaks_the_line() {
        assert_eq!(
            TuiAction::from_key_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            TuiAction::SendMessage
        );
        assert_eq!(
            TuiAction::from_key_event(key(KeyCode::Enter, KeyModifiers::SHIFT)),
            TuiAction::InputNewline
        );
    }

    #[test]
    fn arrows_navigate_and_tab_completes() {
        assert_eq!(
            TuiAction::from_key_event(key(KeyCode::Up, KeyModifiers::NONE)),
            TuiAction::CursorUp
        );
        assert_eq!(
            TuiAction::from_key_event(key(KeyCode::Tab, KeyModifiers::NONE)),
            TuiAction::CompleteNext
        );
        assert_eq!(
            TuiAction::from_key_event(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            TuiAction::CompletePrev
        );
    }

    /// A plain `d` is text, not a quit — only the control combination quits.
    #[test]
    fn plain_characters_are_text() {
        assert_eq!(
            TuiAction::from_key_event(key(KeyCode::Char('d'), KeyModifiers::NONE)),
            TuiAction::InputChar('d')
        );
    }
}
