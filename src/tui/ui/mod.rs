//! Rendering helpers and the live region.
//!
//! There is no full-screen layout any more: finished output goes to the
//! terminal's scrollback (see [`crate::tui::scrollback`]) and only
//! [`live`] — the composer, the status row and any blocking prompt — is
//! redrawn. What remains here are the shared styles both sides use.
//!
//! The palette is modeless on purpose: [`Theme::dark`] and [`Theme::light`]
//! mirror Claude Code's default themes. [`Theme::for_mode`] degrades the
//! chosen palette to what the terminal supports.

use ratatui::style::{Color, Modifier, Style};

use crate::tui::osc11::ColorMode;

pub mod blocks;
pub mod live;
pub mod wrap;

/// Which palette to render with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeId {
    Dark,
    Light,
}

/// The dark/light palette the TUI is painted with.
///
/// Palettes come from Claude Code's default themes
/// (`claude-code/src/utils/theme.ts`), which pin explicit RGB values so
/// terminals' custom ANSI colors cannot skew them:
///   dark:  text `rgb(255,255,255)`, dim `rgb(153,153,153)`, subtle `rgb(80,80,80)`,
///          user_bg `rgb(55,55,55)`, select_bg `rgb(44,50,62)`,
///          accent `rgb(177,185,249)`, error `rgb(255,107,128)`, warning `rgb(255,193,7)`
///   light: text `rgb(0,0,0)`, dim `rgb(102,102,102)`, subtle `rgb(175,175,175)`,
///          user_bg `rgb(240,240,240)`, select_bg `rgb(180,213,255)`,
///          accent `rgb(87,105,247)`, error `rgb(171,43,63)`, warning `rgb(150,108,30)`
/// `code_bg` is the elevated background behind fenced code blocks: slightly
/// darker than the terminal's on dark themes, slightly lighter on light ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub text: Color,
    pub dim: Color,
    pub subtle: Color,
    pub user_bg: Color,
    pub select_bg: Color,
    pub accent: Color,
    pub error: Color,
    pub warning: Color,
    pub code_bg: Color,
}

impl Theme {
    pub const fn dark() -> Self {
        Self {
            text: Color::Rgb(255, 255, 255),
            dim: Color::Rgb(153, 153, 153),
            subtle: Color::Rgb(80, 80, 80),
            user_bg: Color::Rgb(55, 55, 55),
            select_bg: Color::Rgb(44, 50, 62),
            accent: Color::Rgb(177, 185, 249),
            error: Color::Rgb(255, 107, 128),
            warning: Color::Rgb(255, 193, 7),
            code_bg: Color::Rgb(38, 38, 38),
        }
    }

    pub const fn light() -> Self {
        Self {
            text: Color::Rgb(0, 0, 0),
            dim: Color::Rgb(102, 102, 102),
            subtle: Color::Rgb(175, 175, 175),
            user_bg: Color::Rgb(240, 240, 240),
            // Claude Code's light-mode selection blue, not its message-actions
            // gray: the highlight is a selection affordance.
            select_bg: Color::Rgb(180, 213, 255),
            accent: Color::Rgb(87, 105, 247),
            error: Color::Rgb(171, 43, 63),
            warning: Color::Rgb(150, 108, 30),
            code_bg: Color::Rgb(240, 240, 240),
        }
    }

    /// Dim style for secondary text.
    pub fn dim_style(&self) -> Style {
        Style::default().fg(self.dim)
    }

    /// Highlight style for selected items.
    pub fn highlight_style(&self) -> Style {
        Style::default()
            .bg(self.select_bg)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for the user's own messages: default text on a muted gray box,
    /// the way Claude Code paints user turns.
    pub fn user_style(&self) -> Style {
        Style::default().fg(self.text).bg(self.user_bg)
    }

    /// Style for assistant prose. Plain text color — no role tint, like
    /// Claude Code, where prose stays white and only constructs get their
    /// own colors.
    pub fn assistant_style(&self) -> Style {
        Style::default().fg(self.text)
    }

    /// Style for error notices.
    pub fn error_style(&self) -> Style {
        Style::default().fg(self.error)
    }

    /// Style for system / command output.
    pub fn system_style(&self) -> Style {
        Style::default().fg(self.warning)
    }

    /// Style for reasoning blocks. Claude Code renders thinking with
    /// `dimColor` (its `inactive` gray); the italic is our own addition to
    /// keep it apart.
    pub fn reasoning_style(&self) -> Style {
        Style::default().fg(self.dim).add_modifier(Modifier::ITALIC)
    }

    /// Style for tool calls and their results. Claude Code's accent
    /// blue-purple (`suggestion`/`permission`, also its inline-code color).
    pub fn tool_call_style(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Style for code blocks. No syntax highlighter here, so the block gets
    /// the nearest analog of Claude Code's fenced code: plain text on an
    /// elevated background slightly off the terminal's.
    pub fn code_style(&self) -> Style {
        Style::default().bg(self.code_bg).fg(self.text)
    }

    /// Style for an inline code span: the accent color, the way Claude Code
    /// paints inline code.
    pub fn inline_code_style(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Style for the composer's `> ` prompt marker. Claude Code's `❯` is its
    /// `subtle` gray; bold keeps it legible on black.
    pub fn prompt_style(&self) -> Style {
        Style::default()
            .fg(self.subtle)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for the status row.
    pub fn status_style(&self) -> Style {
        self.dim_style()
    }

    /// Style for a status row that is reporting a problem.
    pub fn status_error_style(&self) -> Style {
        self.error_style()
    }

    /// Style for the spinner: the accent color, the one color that can mean
    /// "working" on either theme.
    pub fn spinner_style(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// Degrade the palette to what the terminal can actually render.
    ///
    /// Truecolor terminals keep the exact RGB values; 256-color terminals
    /// round each channel onto the 6×6×6 ANSI cube; 16-color terminals pick
    /// the nearest named ANSI color. Non-RGB colors pass through untouched.
    pub fn for_mode(&self, mode: ColorMode) -> Theme {
        let d = |c: Color| degrade(c, mode);
        Theme {
            text: d(self.text),
            dim: d(self.dim),
            subtle: d(self.subtle),
            user_bg: d(self.user_bg),
            select_bg: d(self.select_bg),
            accent: d(self.accent),
            error: d(self.error),
            warning: d(self.warning),
            code_bg: d(self.code_bg),
        }
    }
}

/// The RGB values xterm assigns to the 16 named ANSI colors, normal and
/// bright; the 16-color degradation target.
const ANSI_16: [(u8, u8, u8); 16] = [
    (0, 0, 0),       // 0  black
    (128, 0, 0),     // 1  red
    (0, 128, 0),     // 2  green
    (128, 128, 0),   // 3  yellow
    (0, 0, 128),     // 4  blue
    (128, 0, 128),   // 5  magenta
    (0, 128, 128),   // 6  cyan
    (192, 192, 192), // 7  white
    (128, 128, 128), // 8  bright black
    (255, 0, 0),     // 9  bright red
    (0, 255, 0),     // 10 bright green
    (255, 255, 0),   // 11 bright yellow
    (0, 0, 255),     // 12 bright blue
    (255, 0, 255),   // 13 bright magenta
    (0, 255, 255),   // 14 bright cyan
    (255, 255, 255), // 15 bright white
];

/// Nearest named ANSI color under squared Euclidean distance.
fn nearest_ansi16(r: u8, g: u8, b: u8) -> u8 {
    let (mut best, mut best_d) = (0u8, i64::MAX);
    for (i, &(ar, ag, ab)) in ANSI_16.iter().enumerate() {
        let dr = i64::from(r) - i64::from(ar);
        let dg = i64::from(g) - i64::from(ag);
        let db = i64::from(b) - i64::from(ab);
        let d = dr * dr + dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best = i as u8;
        }
    }
    best
}

/// Round one color onto the terminal's color space; identity on Truecolor.
pub fn degrade(color: Color, mode: ColorMode) -> Color {
    match mode {
        ColorMode::Truecolor => color,
        ColorMode::C256 => match color {
            Color::Rgb(r, g, b) => {
                // Index 16..=231 of the xterm palette: the 6×6×6 color cube
                // (levels 0, 51, …, 255), plus 16 for the system colors.
                let cube = |v: u8| ((u16::from(v) * 5 + 127) / 255).min(5) as u8;
                Color::Indexed(16 + 36 * cube(r) + 6 * cube(g) + cube(b))
            }
            other => other,
        },
        ColorMode::C16 => match color {
            Color::Rgb(r, g, b) => Color::Indexed(nearest_ansi16(r, g, b)),
            other => other,
        },
    }
}

impl From<ThemeId> for Theme {
    fn from(id: ThemeId) -> Self {
        match id {
            ThemeId::Dark => Theme::dark(),
            ThemeId::Light => Theme::light(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_palettes_are_distinct() {
        let dark = Theme::dark();
        let light = Theme::light();
        assert_ne!(dark, light);
        for (dark, light) in [
            (dark.text, light.text),
            (dark.dim, light.dim),
            (dark.subtle, light.subtle),
            (dark.user_bg, light.user_bg),
            (dark.select_bg, light.select_bg),
            (dark.accent, light.accent),
            (dark.error, light.error),
            (dark.warning, light.warning),
            (dark.code_bg, light.code_bg),
        ] {
            assert_ne!(dark, light, "palettes must not share a color");
        }
    }

    #[test]
    fn styles_carry_the_theme_colors() {
        let dark = Theme::dark();
        assert_eq!(dark.assistant_style().fg, Some(dark.text));
        assert_eq!(dark.user_style().bg, Some(dark.user_bg));
        assert_eq!(dark.tool_call_style().fg, Some(dark.accent));
        assert_eq!(dark.code_style().bg, Some(dark.code_bg));
        assert_eq!(dark.status_error_style(), dark.error_style());
        // The spinner is the accent, plus the bold working feel.
        assert_eq!(dark.spinner_style().fg, Some(dark.accent));
        assert!(dark.spinner_style().add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn theme_id_maps_to_the_matching_palette() {
        assert_eq!(Theme::from(ThemeId::Dark), Theme::dark());
        assert_eq!(Theme::from(ThemeId::Light), Theme::light());
    }

    #[test]
    fn for_mode_truecolor_is_the_identity() {
        assert_eq!(Theme::dark().for_mode(ColorMode::Truecolor), Theme::dark());
        assert_eq!(Theme::light().for_mode(ColorMode::Truecolor), Theme::light());
    }

    #[test]
    fn c256_rounds_onto_the_ansi_cube() {
        // Pure primaries land on the cube corners; index = 16 + 36r + 6g + b.
        assert_eq!(degrade(Color::Rgb(255, 0, 0), ColorMode::C256), Color::Indexed(196));
        assert_eq!(degrade(Color::Rgb(0, 255, 0), ColorMode::C256), Color::Indexed(46));
        assert_eq!(degrade(Color::Rgb(0, 0, 0), ColorMode::C256), Color::Indexed(16));
        assert_eq!(degrade(Color::Rgb(255, 255, 255), ColorMode::C256), Color::Indexed(231));
        // Close to 255 rounds up to the top level, not truncation.
        assert_eq!(degrade(Color::Rgb(253, 0, 0), ColorMode::C256), Color::Indexed(196));
    }

    #[test]
    fn c16_picks_the_nearest_named_color() {
        assert_eq!(degrade(Color::Rgb(255, 0, 0), ColorMode::C16), Color::Indexed(9));
        assert_eq!(degrade(Color::Rgb(0, 0, 0), ColorMode::C16), Color::Indexed(0));
        assert_eq!(degrade(Color::Rgb(255, 255, 255), ColorMode::C16), Color::Indexed(15));
        // A mid gray is closer to plain white (192) than to the black end.
        assert_eq!(degrade(Color::Rgb(200, 200, 200), ColorMode::C16), Color::Indexed(7));
        // The pure ANSI red maps to itself.
        assert_eq!(degrade(Color::Rgb(128, 0, 0), ColorMode::C16), Color::Indexed(1));
    }

    #[test]
    fn degraded_palettes_never_carry_truecolor_rgb() {
        for &mode in &[ColorMode::C256, ColorMode::C16] {
            for theme in [Theme::dark(), Theme::light()] {
                let degraded = theme.for_mode(mode);
                for (name, color) in [
                    ("text", degraded.text),
                    ("dim", degraded.dim),
                    ("subtle", degraded.subtle),
                    ("user_bg", degraded.user_bg),
                    ("select_bg", degraded.select_bg),
                    ("accent", degraded.accent),
                    ("error", degraded.error),
                    ("warning", degraded.warning),
                    ("code_bg", degraded.code_bg),
                ] {
                    assert!(
                        !matches!(color, Color::Rgb(..)),
                        "{mode:?}: {name} must not stay RGB, got {color:?}"
                    );
                }
            }
        }
    }
}
