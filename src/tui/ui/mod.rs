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

// Palette: Claude Code's default dark theme (`claude-code/src/utils/theme.ts`),
// which pins explicit RGB values so terminals' custom ANSI colors cannot skew it:
//   text            rgb(255,255,255)   userMessageBackground rgb(55,55,55)
//   inactive (dim)  rgb(153,153,153)   messageActionsBackground rgb(44,50,62)
//   subtle          rgb(80,80,80)      suggestion/permission rgb(177,185,249)
//   error           rgb(255,107,128)   warning rgb(255,193,7)  claude rgb(215,119,87)
const TEXT: Color = Color::Rgb(255, 255, 255);
const DIM: Color = Color::Rgb(153, 153, 153);
const SUBTLE: Color = Color::Rgb(80, 80, 80);
const USER_BG: Color = Color::Rgb(55, 55, 55);
const SELECT_BG: Color = Color::Rgb(44, 50, 62);
const ACCENT: Color = Color::Rgb(177, 185, 249);
const ERROR: Color = Color::Rgb(255, 107, 128);
const WARNING: Color = Color::Rgb(255, 193, 7);

/// Dim style for secondary text.
pub fn dim_style() -> Style {
    Style::default().fg(DIM)
}

/// Highlight style for selected items.
pub fn highlight_style() -> Style {
    Style::default().bg(SELECT_BG).add_modifier(Modifier::BOLD)
}

/// Style for the user's own messages: default text on a muted gray box, the
/// way Claude Code paints user turns.
pub fn user_style() -> Style {
    Style::default().fg(TEXT).bg(USER_BG)
}

/// Style for assistant prose. Plain text color — no role tint, like Claude
/// Code, where prose stays white and only constructs get their own colors.
pub fn assistant_style() -> Style {
    Style::default().fg(TEXT)
}

/// Style for error notices.
pub fn error_style() -> Style {
    Style::default().fg(ERROR)
}

/// Style for system / command output.
pub fn system_style() -> Style {
    Style::default().fg(WARNING)
}

/// Style for reasoning blocks. Claude Code renders thinking with `dimColor`
/// (its `inactive` gray); the italic is our own addition to keep it apart.
pub fn reasoning_style() -> Style {
    Style::default().fg(DIM).add_modifier(Modifier::ITALIC)
}

/// Style for tool calls and their results. Claude Code's accent blue-purple
/// (`suggestion`/`permission`, also its inline-code color).
pub fn tool_call_style() -> Style {
    Style::default().fg(ACCENT)
}

/// Style for code blocks. No syntax highlighter here, so the block gets the
/// nearest analog of Claude Code's fenced code: plain white on an elevated
/// background slightly lighter than the terminal's.
pub fn code_style() -> Style {
    Style::default().bg(Color::Rgb(38, 38, 38)).fg(TEXT)
}

/// Style for the composer's `> ` prompt marker. Claude Code's `❯` is its
/// `subtle` gray; bold keeps it legible on black.
pub fn prompt_style() -> Style {
    Style::default().fg(SUBTLE).add_modifier(Modifier::BOLD)
}

/// Style for the status row.
pub fn status_style() -> Style {
    dim_style()
}

/// Style for a status row that is reporting a problem.
pub fn status_error_style() -> Style {
    error_style()
}
