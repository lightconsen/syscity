//! Syntax highlighting for fenced code blocks.
//!
//! Called once per rendered run of `LineKind::Code` lines, and it returns
//! exactly one [`Line`] per input line so the caller can splice the result in
//! place (see `blocks::to_lines`).
//!
//! # Why the theme is synthesised rather than loaded
//!
//! A stock syntect theme (`ThemeSet::load_defaults()`) carries dozens of RGB
//! colours, each of which the terminal-capability check would then degrade on
//! its own, with no control over where they land. This palette has four usable
//! foregrounds on a 16-colour terminal (`text`, `dim`, `accent`, `warning` —
//! `accent` and `error` collapse onto one ANSI slot), so a dozen theme colours
//! would degrade into collisions nobody chose.
//!
//! Instead each scope maps to a *role* that is one of the palette's own
//! colours. That colour is handed to syntect as RGB and looked back up in a
//! reverse map afterwards, recovering the exact original [`Color`] — including
//! `Color::Indexed` after degradation. An RGB we did not put there falls back
//! to the text colour, so no truecolor escape can leak out of a 16-colour
//! session.
//!
//! # Caching
//!
//! While a fence is open the transcript holds its lines and the live region
//! re-renders them on nearly every tick, so one block is highlighted many times
//! a second. Results are cached by `(language, body text)`. A grown block is a
//! different key, which is the whole invalidation story: there is nothing to
//! expire, only entries that stop being asked for.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, OnceLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SynColor, FontStyle, ScopeSelectors, Style as SynStyle, StyleModifier,
    Theme as SynTheme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::Theme;

/// Which palette colour a scope is drawn in.
///
/// Deliberately the palette's own roles rather than new colours: they are the
/// ones the terminal-capability check already resolved, so highlighting cannot
/// spend a colour the terminal does not have.
#[derive(Clone, Copy)]
enum Role {
    /// Ordinary identifiers and function names.
    Text,
    /// Comments.
    Dim,
    /// Keywords, storage classes, type names.
    Accent,
    /// Strings, numbers, constants.
    Warning,
    /// Invalid or deprecated constructs.
    Error,
}

impl Role {
    fn color(self, theme: &Theme) -> Color {
        match self {
            Role::Text => theme.text,
            Role::Dim => theme.dim,
            Role::Accent => theme.accent,
            Role::Warning => theme.warning,
            Role::Error => theme.error,
        }
    }
}

/// Scope selector → role. Where several could match, the first wins.
const SCOPE_ROLES: &[(&str, Role)] = &[
    ("comment", Role::Dim),
    ("string", Role::Warning),
    ("constant.numeric", Role::Warning),
    ("constant", Role::Warning),
    ("keyword", Role::Accent),
    ("storage", Role::Accent),
    ("entity.name.type", Role::Accent),
    ("support.type", Role::Accent),
    ("entity.name.function", Role::Text),
    ("variable", Role::Text),
    ("invalid", Role::Error),
];

/// The entry cap bounds bookkeeping; the byte cap is the one that matters,
/// because a cached entry retains the block's text *and* its styled spans, and
/// one enormous history reprint must not evict the working set.
const CACHE_MAX_ENTRIES: usize = 500;
const CACHE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Blocks larger than this are highlighted but never cached.
const CACHE_MAX_BLOCK_BYTES: usize = 64 * 1024;

/// Highlight a run of consecutive `LineKind::Code` lines.
///
/// The run is the raw text the parser emitted, so it may hold several blocks
/// (two adjacent fences), a block whose opening fence was released by the
/// stream buffer's hold cap (body-only), or a block that has not closed yet.
/// Returns one line per input line, always.
pub fn highlight_code_lines(theme: &Theme, lines: &[String]) -> Vec<Line<'static>> {
    if lines.is_empty() {
        return Vec::new();
    }
    let blocks = split_blocks(lines);
    let mut state = lock_state();
    // A different palette means every cached span is the wrong colour.
    if state.theme != Some(*theme) {
        state.rebuild_theme(theme);
    }

    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    for block in &blocks {
        if let Some(opener) = block.opener {
            out.push(fence_line(&lines[opener], theme));
        }
        let body: Vec<&String> = block.body.iter().filter_map(|i| lines.get(*i)).collect();
        out.extend(state.highlight_block(theme, block.lang.as_deref(), &body));
        if let Some(closer) = block.closer {
            out.push(fence_line(&lines[closer], theme));
        }
    }
    debug_assert_eq!(out.len(), lines.len(), "one rendered line per input line");
    out
}

/// One fenced block inside a run: where its fences are, and which lines are body.
struct Block {
    lang: Option<String>,
    opener: Option<usize>,
    body: Vec<usize>,
    closer: Option<usize>,
}

/// Split a run of code lines into blocks.
///
/// A run normally holds exactly one complete block, but three other shapes
/// occur: two adjacent blocks, a body-only block (the opener was released by
/// the hold cap), and a block still streaming (no closer yet).
fn split_blocks(lines: &[String]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let (lang, opener) = if is_fence(&lines[i]) {
            let lang = info_string(&lines[i]);
            let opener = i;
            i += 1;
            (lang, Some(opener))
        } else {
            // The run starts mid-block, so this body has no language tag.
            (None, None)
        };

        let mut body = Vec::new();
        while i < lines.len() && !is_fence(&lines[i]) {
            body.push(i);
            i += 1;
        }
        // A fence here can only close: an opener would have been consumed above.
        let closer = if i < lines.len() {
            let closer = i;
            i += 1;
            Some(closer)
        } else {
            None
        };
        blocks.push(Block { lang, opener, body, closer });
    }
    blocks
}

/// True for a line that opens or closes a fence.
fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// The language token of an opening fence.
///
/// ```` ```rust ```` → `rust`; ```` ```rust,ignore ```` and
/// ```` ```rust title="x" ```` → `rust`; a bare fence → `None`.
fn info_string(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("```")?;
    let token = rest
        .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
        .next()?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// A fence row: the marker stays visible, styled as a caption rather than code.
fn fence_line(text: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(theme.dim).bg(theme.code_bg)))
}

/// A body row rendered without highlighting.
fn code_line(text: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), theme.code_style()))
}

/// Comment scopes read better slanted; nothing else needs a font style.
fn font_style_for(selector: &str) -> Option<FontStyle> {
    selector.starts_with("comment").then_some(FontStyle::ITALIC)
}

/// Rough retained size of a rendered block, for the byte budget.
fn lines_cost(lines: &[Line<'static>]) -> usize {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>()
        })
        .sum()
}

// ── state ────────────────────────────────────────────────────────────────────

/// Cache key: a language name and the block's body text.
type Key = (String, String);

struct HighlightState {
    /// Loaded when the state is first built, which only happens when a code
    /// block is first rendered: a session with no fences pays nothing.
    syntaxes: SyntaxSet,
    /// The palette the current syntect theme was built from.
    theme: Option<Theme>,
    syntect_theme: SynTheme,
    /// `(r, g, b)` handed to syntect → the palette colour it stands for.
    reverse: HashMap<(u8, u8, u8), Color>,
    cache: HashMap<Key, Vec<Line<'static>>>,
    order: VecDeque<Key>,
    bytes: usize,
}

fn lock_state() -> MutexGuard<'static, HighlightState> {
    static STATE: OnceLock<Mutex<HighlightState>> = OnceLock::new();
    // A poisoned lock means a previous render panicked. The cache is derived
    // data, so the right recovery is to keep using it, not to propagate.
    STATE
        .get_or_init(|| Mutex::new(HighlightState::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl HighlightState {
    fn new() -> Self {
        Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: None,
            syntect_theme: empty_theme(),
            reverse: HashMap::new(),
            cache: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    /// Build the syntect theme from a palette, and the map back out of it.
    fn rebuild_theme(&mut self, theme: &Theme) {
        let mut reverse: HashMap<(u8, u8, u8), Color> = HashMap::new();
        let mut scopes: Vec<ThemeItem> = Vec::new();
        let text_rgb = super::color_to_rgb(theme.text).unwrap_or((255, 255, 255));

        for (selector, role) in SCOPE_ROLES {
            let color = role.color(theme);
            let Some(rgb) = super::color_to_rgb(color) else {
                continue;
            };
            // Two roles can land on one RGB — under a 16-colour terminal
            // `accent` and `error` share a slot. Either mapping renders
            // identically, so the first one to claim it is fine.
            reverse.entry(rgb).or_insert(color);

            let Ok(scope) = selector.parse::<ScopeSelectors>() else {
                // A selector syntect cannot parse is a bug in SCOPE_ROLES, not
                // a runtime condition — and it must not take the terminal
                // down. The scope is skipped and the text stays legible.
                tracing::debug!(selector, "syntect rejected a scope selector");
                continue;
            };
            scopes.push(ThemeItem {
                scope,
                style: StyleModifier {
                    foreground: Some(syn_color(rgb)),
                    background: None,
                    font_style: font_style_for(selector),
                },
            });
        }

        self.syntect_theme = SynTheme {
            name: Some("syscity".to_string()),
            author: None,
            settings: ThemeSettings {
                foreground: Some(syn_color(text_rgb)),
                ..ThemeSettings::default()
            },
            scopes,
        };
        self.reverse = reverse;
        self.theme = Some(*theme);
        self.cache.clear();
        self.order.clear();
        self.bytes = 0;
    }

    /// Highlight one block's body, through the cache.
    fn highlight_block(
        &mut self,
        theme: &Theme,
        lang: Option<&str>,
        body: &[&String],
    ) -> Vec<Line<'static>> {
        if body.is_empty() {
            return Vec::new();
        }
        // Resolve to a name and drop the borrow before the cache is touched.
        let Some(name) = lang
            .and_then(|token| self.syntax_for(token))
            .map(|s| s.name.clone())
        else {
            // No tag, or a tag syntect does not know: plain rows, no pass.
            return body.iter().map(|line| code_line(line, theme)).collect();
        };

        let text = body_text(body);
        let large = text.len() > CACHE_MAX_BLOCK_BYTES;
        let key: Key = (name, text);

        if !large {
            if let Some(hit) = self.get(&key) {
                return hit;
            }
        }

        let rendered = self.run_highlighter(theme, &key.0, body);
        if !large {
            self.put(key, rendered.clone());
        }
        rendered
    }

    /// A syntax by language token, then by extension (`rs`, `py`, …).
    fn syntax_for(&self, token: &str) -> Option<&SyntaxReference> {
        self.syntaxes
            .find_syntax_by_token(token)
            .or_else(|| self.syntaxes.find_syntax_by_extension(token))
    }

    /// The highlighting pass: one state machine over the whole block, so
    /// multi-line comments and strings are correct, emitting per-line spans.
    fn run_highlighter(&self, theme: &Theme, name: &str, body: &[&String]) -> Vec<Line<'static>> {
        let Some(syntax) = self.syntaxes.find_syntax_by_name(name) else {
            return body.iter().map(|line| code_line(line, theme)).collect();
        };
        let mut highlighter = HighlightLines::new(syntax, &self.syntect_theme);
        let mut out = Vec::with_capacity(body.len());
        for line in body {
            // The newline is what lets the parser carry state across lines; it
            // is stripped again in `spans_for` so it never reaches the terminal.
            let source = format!("{line}\n");
            match highlighter.highlight_line(&source, &self.syntaxes) {
                Ok(ranges) => {
                    let spans = self.spans_for(ranges, theme);
                    if spans.is_empty() {
                        out.push(code_line(line, theme));
                    } else {
                        out.push(Line::from(spans));
                    }
                }
                // A line the parser chokes on is shown unhighlighted rather
                // than dropped: the text is the point, the colour is not.
                Err(e) => {
                    tracing::debug!(error = %e, "syntect could not highlight a line");
                    out.push(code_line(line, theme));
                }
            }
        }
        out
    }

    /// Map syntect's styled ranges onto ratatui spans.
    fn spans_for(&self, ranges: Vec<(SynStyle, &str)>, theme: &Theme) -> Vec<Span<'static>> {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(ranges.len());
        for (style, text) in ranges {
            // The parser was fed "<line>\n"; that newline is ours.
            let text = text.strip_suffix('\n').unwrap_or(text);
            if text.is_empty() {
                continue;
            }
            spans.push(Span::styled(text.to_string(), self.span_style(style, theme)));
        }
        spans
    }

    fn span_style(&self, style: SynStyle, theme: &Theme) -> Style {
        let key = (style.foreground.r, style.foreground.g, style.foreground.b);
        // An RGB we did not put there must not reach a 16-colour terminal.
        let fg = self.reverse.get(&key).copied().unwrap_or(theme.text);
        let mut out = Style::default().fg(fg).bg(theme.code_bg);
        if style.font_style.contains(FontStyle::BOLD) {
            out = out.add_modifier(Modifier::BOLD);
        }
        if style.font_style.contains(FontStyle::ITALIC) {
            out = out.add_modifier(Modifier::ITALIC);
        }
        if style.font_style.contains(FontStyle::UNDERLINE) {
            out = out.add_modifier(Modifier::UNDERLINED);
        }
        out
    }

    /// Move a cached entry to the MRU end and return it.
    fn get(&mut self, key: &Key) -> Option<Vec<Line<'static>>> {
        let hit = self.cache.get(key).cloned()?;
        self.order.retain(|k| k != key);
        self.order.push_back(key.clone());
        Some(hit)
    }

    fn put(&mut self, key: Key, lines: Vec<Line<'static>>) {
        let cost = key.1.len() + lines_cost(&lines);
        self.cache.insert(key.clone(), lines);
        self.order.push_back(key);
        self.bytes += cost;
        while self.order.len() > CACHE_MAX_ENTRIES || self.bytes > CACHE_MAX_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(dropped) = self.cache.remove(&oldest) {
                self.bytes = self
                    .bytes
                    .saturating_sub(oldest.1.len() + lines_cost(&dropped));
            }
        }
    }
}

fn syn_color(rgb: (u8, u8, u8)) -> SynColor {
    SynColor {
        r: rgb.0,
        g: rgb.1,
        b: rgb.2,
        a: 255,
    }
}

/// A block's body as one string, for the cache key and the size check.
fn body_text(body: &[&String]) -> String {
    let mut out = String::new();
    for line in body {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The theme syntect wants before one has been built from a palette.
fn empty_theme() -> SynTheme {
    SynTheme {
        name: None,
        author: None,
        settings: ThemeSettings::default(),
        scopes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::osc11::ColorMode;

    fn lines(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| (*s).to_string()).collect()
    }

    /// Every character the parser gave us still comes out, whatever colouring
    /// happens in between.
    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_rust_block_is_coloured_and_loses_nothing() {
        let source = lines(&["```rust", "// note", "let x: u32 = 1;", "```"]);
        let out = highlight_code_lines(&Theme::dark(), &source);

        assert_eq!(out.len(), source.len(), "one rendered line per input line");
        assert_eq!(text_of(&out), source.join("\n"), "no character lost");

        let body = &out[2];
        assert!(body.spans.len() > 1, "the body is tokenized, not one span: {body:?}");
        // The comment is dimmed, the keyword is accented — different colours
        // from each other and from plain text.
        let comment = &out[1];
        assert_eq!(comment.spans[0].style.fg, Some(Theme::dark().dim), "the comment is dimmed");
    }

    #[test]
    fn the_fence_rows_stay_visible_and_read_as_captions() {
        let theme = Theme::dark();
        let out = highlight_code_lines(&theme, &lines(&["```rust", "let x = 1;", "```"]));
        // Both fences keep their marker text, styled as a caption rather than
        // as code.
        assert_eq!(out[0].spans[0].content, "```rust");
        assert_eq!(out[2].spans[0].content, "```");
        assert_eq!(out[0].spans[0].style.fg, Some(theme.dim));
    }

    #[test]
    fn an_unknown_language_falls_back_to_plain() {
        let theme = Theme::dark();
        let out = highlight_code_lines(&theme, &lines(&["```notalanguage", "some text", "```"]));
        assert_eq!(out[1].spans.len(), 1, "plain text needs no tokenizing");
        assert_eq!(out[1].spans[0].style, theme.code_style());
    }

    #[test]
    fn a_bare_fence_needs_no_language_either() {
        let theme = Theme::dark();
        let out = highlight_code_lines(&theme, &lines(&["```", "plain", "```"]));
        assert_eq!(out[1].spans.len(), 1);
    }

    #[test]
    fn a_body_without_its_opener_renders_plain_rather_than_guessing() {
        // What the stream buffer's hold cap leaves behind: no language to use.
        let theme = Theme::dark();
        let out = highlight_code_lines(&theme, &lines(&["let x = 1;", "let y = 2;"]));
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|line| line.spans.len() == 1));
    }

    #[test]
    fn an_unterminated_block_still_highlights_and_gains_no_fence_row() {
        let theme = Theme::dark();
        let out = highlight_code_lines(&theme, &lines(&["```rust", "let x = 1;"]));
        assert_eq!(out.len(), 2, "no invented closing fence");
        assert!(out[1].spans.len() > 1, "a streaming block is still coloured");
    }

    /// Two blocks with no prose between them are one run of code lines, and
    /// must not be treated as a single block.
    #[test]
    fn adjacent_blocks_are_highlighted_separately() {
        let theme = Theme::dark();
        let out = highlight_code_lines(
            &theme,
            &lines(&["```rust", "let x = 1;", "```", "```py", "y = 1", "```"]),
        );
        assert_eq!(out.len(), 6);
        assert_eq!(out[0].spans[0].content, "```rust");
        assert_eq!(out[3].spans[0].content, "```py");
    }

    /// The language tag can carry extras: `````rust,ignore````, or an attribute
    /// block. The first token is the language.
    #[test]
    fn the_info_string_is_cut_to_its_language_token() {
        assert_eq!(info_string("```rust"), Some("rust".to_string()));
        assert_eq!(info_string("  ```rust,ignore"), Some("rust".to_string()));
        assert_eq!(info_string("```rust title=\"x\""), Some("rust".to_string()));
        assert_eq!(info_string("```rust{linenos=1}"), Some("rust".to_string()));
        assert_eq!(info_string("```"), None);
        assert_eq!(info_string("not a fence"), None);
    }

    #[test]
    fn a_language_extension_is_accepted_as_a_token() {
        let theme = Theme::dark();
        let out = highlight_code_lines(&theme, &lines(&["```rs", "let x = 1;", "```"]));
        assert!(out[1].spans.len() > 1, "`rs` resolves like `rust`");
    }

    /// A string opened on one line and closed on another keeps its colour all
    /// the way through — the property that needs a whole-block pass.
    #[test]
    fn a_multi_line_construct_keeps_its_colour_across_lines() {
        let theme = Theme::dark();
        let out = highlight_code_lines(
            &theme,
            &lines(&["```rust", "let s = \"first", "still inside\";", "```"]),
        );
        let first = out[1].spans.last().and_then(|s| s.style.fg);
        let second = out[2].spans.first().and_then(|s| s.style.fg);
        assert_eq!(first, second, "the string keeps one colour across the break");
        assert_ne!(first, Some(theme.text), "and it is not the plain colour");
    }

    /// The invariant that keeps a 16-colour terminal honest: nothing the
    /// highlighter emits may be a truecolor value.
    #[test]
    fn a_sixteen_colour_theme_never_emits_truecolor() {
        let theme = Theme::dark().for_mode(ColorMode::C16);
        let out =
            highlight_code_lines(&theme, &lines(&["```rust", "// c", "let x = \"s\";", "```"]));
        for line in &out {
            for span in &line.spans {
                if let Some(fg) = span.style.fg {
                    assert!(
                        matches!(fg, Color::Indexed(_)),
                        "expected an ANSI index, got {fg:?} in {span:?}"
                    );
                }
            }
        }
    }

    /// The palette's own roles are the whole colour budget, and on this
    /// terminal `accent` and `error` collapse onto one slot. The highlighter
    /// must still be able to tell a keyword from a string.
    #[test]
    fn the_roles_stay_distinguishable_under_sixteen_colours() {
        let theme = Theme::dark().for_mode(ColorMode::C16);
        let out = highlight_code_lines(&theme, &lines(&["```rust", "let x = \"s\";", "```"]));
        let mut seen = std::collections::HashSet::new();
        for span in &out[1].spans {
            if let Some(fg) = span.style.fg {
                seen.insert(format!("{fg:?}"));
            }
        }
        assert!(seen.len() > 1, "the body is not monochrome: {seen:?}");
    }

    // ── the cache ────────────────────────────────────────────────────────

    #[test]
    fn a_second_look_at_the_same_block_is_a_hit_and_a_grown_block_is_not() {
        let mut state = HighlightState::new();
        let theme = Theme::dark();

        let key: Key = ("Rust".to_string(), "let x = 1;\n".to_string());
        assert!(state.get(&key).is_none(), "cold");
        state.put(key.clone(), vec![Line::from("x")]);
        assert!(state.get(&key).is_some(), "warm");

        let grown: Key = ("Rust".to_string(), "let x = 1;\nlet y = 2;\n".to_string());
        assert!(state.get(&grown).is_none(), "a grown block is a different key");
        // And building the theme is what clears it.
        state.rebuild_theme(&theme);
        assert!(state.get(&key).is_none(), "a theme change invalidates");
    }

    #[test]
    fn the_cache_evicts_the_least_recently_used_entry() {
        let mut state = HighlightState::new();
        for i in 0..CACHE_MAX_ENTRIES {
            state.put((format!("L{i}"), format!("body {i}")), vec![Line::from("x")]);
        }
        // Touch the oldest so it is no longer the eviction candidate.
        let oldest: Key = ("L0".to_string(), "body 0".to_string());
        assert!(state.get(&oldest).is_some());

        state.put(("new".to_string(), "body new".to_string()), vec![Line::from("x")]);

        assert!(state.get(&oldest).is_some(), "recently used, so it survives");
        assert!(
            state
                .get(&("L1".to_string(), "body 1".to_string()))
                .is_none(),
            "the coldest goes instead"
        );
        assert!(state.cache.len() <= CACHE_MAX_ENTRIES);
    }

    #[test]
    fn the_byte_budget_bounds_a_run_of_large_blocks() {
        let mut state = HighlightState::new();
        let big = "x".repeat(CACHE_MAX_BYTES / 8);
        for i in 0..16 {
            state.put((format!("L{i}"), big.clone()), vec![Line::from(big.clone())]);
        }
        assert!(state.bytes <= CACHE_MAX_BYTES, "{} bytes retained", state.bytes);
    }
}
