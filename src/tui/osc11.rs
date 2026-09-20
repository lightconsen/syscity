//! Terminal background detection (OSC 11) and color-depth fallback.
//!
//! crossterm surfaces no OSC response as an event, so the reply to an OSC 11
//! query is read straight off stdin during the setup window — before the
//! event loop's first `crossterm::read`, while nothing else is reading. The
//! one-shot raw read can swallow a keystroke typed inside that window (a lost
//! character, at worst), but it cannot desync crossterm's parser, because
//! crossterm has not touched the stream yet.
//!
//! The parse mirrors Claude Code's `systemTheme.ts`: `rgb:` components are
//! 1–4 hex digits normalized to `[0,1]` by `value / (16^len - 1)`, and the
//! dark/light split is the ITU-R BT.709 relative-luminance threshold.

use std::time::Duration;

use crate::gateway::ThemeSetting;
use crate::tui::ui::ThemeId;

/// A terminal background color, each channel normalized to 0.0–1.0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BgColor {
    r: f64,
    g: f64,
    b: f64,
}

impl BgColor {
    pub const fn new(r: f64, g: f64, b: f64) -> Self {
        Self { r, g, b }
    }

    /// ITU-R BT.709 relative luminance, the perceptual weight of the color.
    pub fn luminance(&self) -> f64 {
        0.2126 * self.r + 0.7152 * self.g + 0.0722 * self.b
    }

    /// `> 0.5` counts as a light background, like Claude Code.
    pub fn is_light(&self) -> bool {
        self.luminance() > 0.5
    }
}

/// One component of an OSC 11 reply: 1–4 hex digits, normalized so the max
/// value maps to 1.0 (`ffff/ffff` == `ff/ff` == 1.0).
fn parse_component(part: &str) -> Option<f64> {
    if part.is_empty() || part.len() > 4 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let max = (16u64.pow(part.len() as u32) - 1) as f64;
    let value = u64::from_str_radix(part, 16).ok()? as f64;
    Some(value / max)
}

/// Parse the payload after `OSC 11;`, e.g. `rgb:ffff/0000/0000` or
/// `#ff0000` / `#ffff00000000`. `rgba:` replies are accepted with their
/// alpha ignored.
fn parse_spec(spec: &str) -> Option<BgColor> {
    // `rgba:` replies carry a fourth alpha channel, which is noise here.
    if let Some(rest) = spec
        .strip_prefix("rgb:")
        .or_else(|| spec.strip_prefix("rgba:"))
    {
        let mut parts = rest.split('/');
        let r = parse_component(parts.next()?)?;
        let g = parse_component(parts.next()?)?;
        let b = parse_component(parts.next()?)?;
        return Some(BgColor::new(r, g, b));
    }
    // `#RRGGBB` / `#RRRRGGGGBBBB`: 3, 6, 9 or 12 hex digits → 1, 2, 3 or 4
    // per channel.
    let rest = spec.strip_prefix('#')?;
    if rest.len() % 3 != 0 {
        return None;
    }
    let per = rest.len() / 3;
    if !(1..=4).contains(&per) {
        return None;
    }
    if !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = parse_component(&rest[..per])?;
    let g = parse_component(&rest[per..per * 2])?;
    let b = parse_component(&rest[per * 2..])?;
    Some(BgColor::new(r, g, b))
}

/// Extract the background color from a raw OSC 11 reply.
///
/// Replies are `ESC]11;rgb:…` terminated by BEL (`\x07`) or ST (`ESC \`).
/// The reply may be embedded in a buffer that also carries other bytes (a
/// keystroke that arrived while we were reading), so it is located by its
/// `ESC]11;` marker rather than assumed to start at byte zero.
pub fn parse_response(bytes: &[u8]) -> Option<BgColor> {
    let text = std::str::from_utf8(bytes).ok()?;
    let start = text.find("\x1b]11;")?;
    let rest = &text[start + 5..];
    // Everything before a BEL, or the ST two-byte terminator.
    let bel = rest.find('\x07');
    let st = rest.find("\x1b\\");
    match (bel, st) {
        (Some(b), Some(s)) => parse_spec(&rest[..b.min(s)]),
        (Some(b), None) => parse_spec(&rest[..b]),
        (None, Some(s)) => parse_spec(&rest[..s]),
        (None, None) => None,
    }
}

/// Map a `$COLORFGBG` background field to a theme guess.
///
/// The var is `fg:bg` with SGR color numbers; 0–6 and 8 are dark backgrounds,
/// 7 and 9–15 are light. Returns `None` for anything unparsable (including
/// the `default` placeholder some terminals emit).
fn colorfgbg_hint(bg: &str) -> Option<ThemeId> {
    let value = bg.parse::<u8>().ok()?;
    Some(if value <= 6 || value == 8 {
        ThemeId::Dark
    } else {
        ThemeId::Light
    })
}

/// Synchronous, environment-only hint: `$COLORFGBG` when the terminal set it.
pub fn env_hint() -> Option<ThemeId> {
    let raw = std::env::var("COLORFGBG").ok()?;
    let bg = raw.split(';').next_back()?;
    colorfgbg_hint(bg)
}

/// Ask the terminal for its background color and answer the theme.
///
/// Non-blocking up to `timeout`. Any error or a missing reply (a terminal
/// that does not speak OSC) yields `None` — callers fall back to dark.
#[cfg(unix)]
pub fn query_background(timeout: Duration) -> Option<ThemeId> {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    use nix::poll::{poll, PollFd, PollFlags};
    use nix::unistd::read;

    // 1. Ask. The reply arrives on stdin even though we asked on stdout.
    let mut out = std::io::stdout();
    if out.write_all(b"\x1b]11;?\x07").is_err() || out.flush().is_err() {
        return None;
    }

    // 2. Wait for the reply, up to `timeout`.
    let stdin = std::io::stdin();
    let mut fds = [PollFd::new(&stdin, PollFlags::POLLIN)];
    let ready = poll(&mut fds, timeout.as_millis().min(i32::MAX as u128) as i32).ok()?;
    if ready == 0 {
        return None;
    }

    // 3. One framed reply fits comfortably in a KiB.
    let mut buf = [0u8; 1024];
    let n = read(stdin.as_raw_fd(), &mut buf).ok()?;
    let bg = parse_response(&buf[..n])?;
    Some(if bg.is_light() {
        ThemeId::Light
    } else {
        ThemeId::Dark
    })
}

/// No terminal wrangling on platforms without `nix::poll`.
#[cfg(not(unix))]
pub fn query_background(_timeout: Duration) -> Option<ThemeId> {
    None
}

/// Resolve a setting into a theme, consulting the wired detection and the
/// cheap environment hint in that order.
///
/// `detected` is the result of a fresh OSC 11 query; `hint` is `$COLORFGBG`.
pub fn resolve(setting: ThemeSetting, hint: Option<ThemeId>, detected: Option<ThemeId>) -> ThemeId {
    match setting {
        ThemeSetting::Dark => ThemeId::Dark,
        ThemeSetting::Light => ThemeId::Light,
        ThemeSetting::Auto => hint.or(detected).unwrap_or(ThemeId::Dark),
    }
}

/// What the terminal can render, from its environment, for color fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Truecolor,
    C256,
    C16,
}

/// Infer the color depth from `$COLORTERM` / `$TERM`.
pub fn color_mode() -> ColorMode {
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return ColorMode::Truecolor;
    }
    if std::env::var("TERM")
        .unwrap_or_default()
        .contains("256color")
    {
        ColorMode::C256
    } else {
        ColorMode::C16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rgb_with_variable_digit_counts() {
        // 4-digit percent scale, 2-digit, 1-digit — all the same red.
        for (spec, expected) in [
            ("rgb:ffff/0000/0000", BgColor::new(1.0, 0.0, 0.0)),
            ("rgb:ff/00/00", BgColor::new(1.0, 0.0, 0.0)),
            ("rgb:f/0/0", BgColor::new(1.0, 0.0, 0.0)),
            ("rgb:8080/8080/8080", BgColor::new(0.50196, 0.50196, 0.50196)),
        ] {
            let got = parse_response(format!("\x1b]11;{spec}\x07").as_bytes()).expect(spec);
            assert!((got.r - expected.r).abs() < 1e-4, "{spec}: r {}", got.r);
            assert!((got.g - expected.g).abs() < 1e-4, "{spec}");
            assert!((got.b - expected.b).abs() < 1e-4, "{spec}");
        }
    }

    #[test]
    fn parses_hash_replies_with_the_same_normalization() {
        // 2 hex digits per channel (`#RRGGBB`).
        let two = parse_response(b"\x1b]11;#ffffff\x1b\\").unwrap();
        assert!((two.r - 1.0).abs() < 1e-4);
        // 4 hex digits per channel (`#RRRRGGGGBBBB`).
        let four = parse_response(b"\x1b]11;#ffff00000000\x1b\\").unwrap();
        assert!((four.r - 1.0).abs() < 1e-4);
        assert!(four.g.abs() < 1e-4);
    }

    #[test]
    fn rgba_replies_are_accepted_with_alpha_ignored() {
        // Some terminals answer with `rgba:`; the alpha channel is noise.
        let bg = parse_response(b"\x1b]11;rgba:0000/0000/0000/0000\x07").unwrap();
        assert!(!bg.is_light());
    }

    #[test]
    fn garbage_and_foreign_responses_are_none() {
        assert!(parse_response(b"no").is_none());
        // An OSC 12 (cursor color) reply carrying a background-shaped value
        // must not be mistaken for the answer.
        assert!(parse_response(b"\x1b]12;rgb:fff/fff/fff\x07").is_none());
        assert!(parse_response(b"\x1b]11;rgb:zz/00/00\x07").is_none());
        assert!(parse_response(b"\x1b]11;#fffff\x07").is_none());
        // Unterminated reply, or a reply split across reads, is incomplete.
        assert!(parse_response(b"\x1b]11;rgb:ff/ff/ff").is_none());
    }

    #[test]
    fn the_reply_is_found_even_when_it_is_not_at_byte_zero() {
        // A keystroke landing in the read window shares the buffer with the
        // reply; the marker scan must still find it.
        let bytes = b"a\x1b]11;rgb:0000/0000/0000\x07";
        assert!(!parse_response(bytes).unwrap().is_light());
    }

    #[test]
    fn luminance_splits_dark_from_light_at_half() {
        assert!(!BgColor::new(0.1, 0.1, 0.1).is_light());
        assert!(BgColor::new(0.95, 0.95, 0.95).is_light());
        // Pure red is perceptually dark; pure blue darker still.
        assert!(!BgColor::new(1.0, 0.0, 0.0).is_light());
        // A mid-gray (0.5,0.5,0.5) is exactly at threshold: not light.
        assert!(!BgColor::new(0.5, 0.5, 0.5).is_light());
    }

    #[test]
    fn colorfgbg_backgrounds_map_to_themes() {
        assert_eq!(colorfgbg_hint("0"), Some(ThemeId::Dark)); // black
        assert_eq!(colorfgbg_hint("6"), Some(ThemeId::Dark));
        assert_eq!(colorfgbg_hint("8"), Some(ThemeId::Dark)); // bright black
        assert_eq!(colorfgbg_hint("7"), Some(ThemeId::Light)); // white
        assert_eq!(colorfgbg_hint("15"), Some(ThemeId::Light)); // bright white
        assert_eq!(colorfgbg_hint("default"), None);
        assert_eq!(colorfgbg_hint(""), None);
    }

    #[test]
    fn resolve_falls_back_in_order() {
        use crate::tui::ui::ThemeId::{Dark, Light};
        assert_eq!(resolve(ThemeSetting::Dark, None, None), Dark);
        assert_eq!(resolve(ThemeSetting::Light, None, None), Light);
        // A setting always wins; hint beats the wire query; the wire query
        // beats the dark default.
        assert_eq!(resolve(ThemeSetting::Dark, Some(Light), Some(Light)), Dark);
        assert_eq!(resolve(ThemeSetting::Auto, Some(Light), Some(Dark)), Light);
        assert_eq!(resolve(ThemeSetting::Auto, None, Some(Light)), Light);
        assert_eq!(resolve(ThemeSetting::Auto, None, None), Dark);
    }

    #[test]
    fn color_mode_prefers_truecolor_then_256() {
        // Pure function over explicit env: probe each mode directly.
        let prev_c = std::env::var("COLORTERM").ok();
        let prev_t = std::env::var("TERM").ok();
        let probe = |colorterm: Option<&str>, term: Option<&str>| {
            match colorterm {
                Some(v) => std::env::set_var("COLORTERM", v),
                None => std::env::remove_var("COLORTERM"),
            }
            match term {
                Some(v) => std::env::set_var("TERM", v),
                None => std::env::remove_var("TERM"),
            }
            color_mode()
        };
        assert_eq!(probe(Some("truecolor"), Some("xterm-256color")), ColorMode::Truecolor);
        assert_eq!(probe(Some("24bit"), None), ColorMode::Truecolor);
        assert_eq!(probe(None, Some("xterm-256color")), ColorMode::C256);
        assert_eq!(probe(None, Some("xterm")), ColorMode::C16);
        assert_eq!(probe(None, None), ColorMode::C16);
        // Restore the caller's environment.
        match prev_c {
            Some(v) => std::env::set_var("COLORTERM", v),
            None => std::env::remove_var("COLORTERM"),
        }
        match prev_t {
            Some(v) => std::env::set_var("TERM", v),
            None => std::env::remove_var("TERM"),
        }
    }
}
