//! Rendering helpers and the live region.
//!
//! There is no full-screen layout any more: finished output goes to the
//! terminal's scrollback (see [`crate::tui::scrollback`]) and only
//! [`live`] — the composer, the status row and any blocking prompt — is
//! redrawn. What remains here are the shared styles both sides use.

use ratatui::style::{Color, Modifier, Style};

pub mod blocks;
pub mod live;
pub mod wrap;

/// Dim style for secondary text.
pub fn dim_style() -> Style {
    Style::default().fg(Color::Gray)
}

/// Highlight style for selected items.
pub fn highlight_style() -> Style {
    Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

/// Style for the user's own messages.
pub fn user_style() -> Style {
    Style::default().fg(Color::Cyan)
}

/// Style for assistant prose.
pub fn assistant_style() -> Style {
    Style::default().fg(Color::Green)
}

/// Style for error notices.
pub fn error_style() -> Style {
    Style::default().fg(Color::Red)
}

/// Style for system / command output.
pub fn system_style() -> Style {
    Style::default().fg(Color::Yellow)
}

/// Style for reasoning blocks.
pub fn reasoning_style() -> Style {
    Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC)
}

/// Style for tool calls and their results.
pub fn tool_call_style() -> Style {
    Style::default().fg(Color::Magenta)
}

/// Style for code blocks.
pub fn code_style() -> Style {
    Style::default().bg(Color::Black).fg(Color::White)
}

/// Style for the composer's `> ` prompt marker.
pub fn prompt_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Style for the status row.
pub fn status_style() -> Style {
    dim_style()
}

/// Style for a status row that is reporting a problem.
pub fn status_error_style() -> Style {
    error_style()
}
