//! The live region: the fixed strip at the bottom of the screen.
//!
//! Everything here is redrawn every frame, which is why it is small. Content
//! that is finished does not live here — it goes to scrollback (see
//! [`crate::tui::scrollback`]). The region is a constant height because
//! `Viewport::Inline`'s height cannot be changed after construction; overflow
//! is handled by the transcript, not by growing this.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::state::{AppState, LiveMode, RunPhase};
use crate::tui::ui::blocks;
use crate::tui::ui::wrap as wrapmod;
use crate::tui::ui::Theme;

/// Total rows the live region occupies.
///
/// `Viewport::Inline(LIVE_HEIGHT)` is fixed for the process's lifetime.
pub const LIVE_HEIGHT: u16 = 8;

/// Rows the composer may grow to before it scrolls internally.
const COMPOSER_MAX_ROWS: u16 = 3;

/// The prompt marker in front of the first input row.
const PROMPT: &str = "> ";

/// Where each part of the live region goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveLayout {
    /// Prompt / preview area above the status row.
    pub block: Rect,
    /// Status row, absent on a terminal too short to afford it.
    pub status: Option<Rect>,
    /// Input area, always at the bottom.
    pub composer: Rect,
}

/// Lay the region out bottom-up so it stays sane at any terminal height.
pub fn layout(area: Rect, composer_rows: u16) -> LiveLayout {
    let height = area.height;
    let composer_rows = composer_rows.clamp(1, COMPOSER_MAX_ROWS).min(height.max(1));
    let composer = Rect {
        x: area.x,
        y: area.y + height.saturating_sub(composer_rows),
        width: area.width,
        height: composer_rows,
    };

    // The status row only exists when there is room for it *and* a block row.
    let has_status = height > composer_rows;
    let status = has_status.then(|| Rect {
        x: area.x,
        y: composer.y.saturating_sub(1),
        width: area.width,
        height: 1,
    });
    let block_bottom = status.map(|s| s.y).unwrap_or(composer.y);
    let block = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: block_bottom.saturating_sub(area.y),
    };
    LiveLayout { block, status, composer }
}

/// Byte length of a wrapped row's text.
fn row_len(row: &Line<'_>) -> usize {
    row.spans.iter().map(|s| s.content.len()).sum()
}

/// Locate the cursor within the wrapped input: `(row, column)`.
///
/// `wrap_lines` never drops a character, so walking the rows by byte length
/// finds the exact row the cursor sits on.
pub fn locate_cursor(rows: &[Line<'_>], cursor: usize) -> (usize, usize) {
    let mut remaining = cursor;
    for (idx, row) in rows.iter().enumerate() {
        let len = row_len(row);
        // `< len` is "inside this row"; `== len` is the boundary, which belongs
        // to the next row — except at the very end of the input, where it is
        // the end of the last row.
        let last = idx + 1 == rows.len();
        if remaining < len || (last && remaining <= len) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            let col = text
                .char_indices()
                .take_while(|(i, _)| *i < remaining)
                .map(|(_, c)| c)
                .collect::<String>();
            return (idx, UnicodeWidthStr::width(col.as_str()));
        }
        // +1 for the newline the wrap consumed.
        remaining = remaining.saturating_sub(len + 1);
    }
    (rows.len().saturating_sub(1), 0)
}

/// Wrap the input buffer to the composer width, prompt included.
///
/// The prompt is part of the first row's text, not a marker painted over it.
/// Wrapping the buffer alone to the full width hands the first row two columns
/// more than it has, so an input that fills the width exactly loses its last
/// character — the row is drawn without it and the cursor clamps on top of the
/// one before.
fn input_rows(state: &AppState, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    // Spans, not one string: the wrapper carries each character's style
    // across the wrap point, so the prompt keeps its own.
    let line = Line::from(vec![
        Span::styled(PROMPT, theme.prompt_style()),
        Span::raw(state.input_buffer.clone()),
    ]);
    wrapmod::wrap_line_hanging(&line, width as usize, PROMPT.len())
}

/// `1m 23s` past a minute, bare seconds below it.
fn format_elapsed(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// The one-line status row, as styled spans.
///
/// The spinner frame and its word are colored; the facts (time, phase, agent,
/// session) keep the plain status color. `status_text` flattens this for
/// tests that only care about the words.
fn status_line(state: &AppState, theme: &Theme) -> (Line<'static>, bool) {
    let sep = || Span::styled("  ·  ", theme.status_style());
    let mut spans: Vec<Span<'static>> = Vec::new();
    // A running row is longer than an idle one, and what it pushes off the end
    // is the session id. The server version is the least useful thing here and
    // does not change mid-session, so it yields the space. A connection that is
    // *not* fine still speaks up: that is not noise.
    if !state.is_running || !state.connection.is_connected() {
        spans.push(Span::styled(state.connection.label(), theme.status_style()));
    }
    if let Some(secs) = state.run_elapsed_secs() {
        if !spans.is_empty() {
            spans.push(sep());
        }
        spans.push(Span::styled(format!("{} ", state.spinner_frame()), theme.spinner_style()));
        spans.push(Span::styled(format!("{}…", state.spinner_word()), theme.spinner_style()));
        // The word is whimsy; the parenthetical is the truth.
        let mut detail = format_elapsed(secs);
        match &state.run_phase {
            RunPhase::Waiting => {}
            RunPhase::Thinking => detail.push_str(" · thinking"),
            RunPhase::Responding => detail.push_str(" · responding"),
            RunPhase::ToolCall(name) => {
                detail.push_str(&format!(" · ⚙ {name}"));
            }
        }
        spans.push(Span::styled(format!(" ({detail}) — esc stops"), theme.status_style()));
    }
    let agent = state
        .current_agent_info()
        .map(|a| format!("{} {}", a.emoji, a.display_name))
        .or_else(|| state.current_agent.as_deref().map(str::to_string));
    if let Some(agent) = agent {
        if !spans.is_empty() {
            spans.push(sep());
        }
        spans.push(Span::styled(agent, theme.status_style()));
    }
    if let Some(session) = state.current_session.as_deref() {
        if !spans.is_empty() {
            spans.push(sep());
        }
        spans.push(Span::styled(short_session(session), theme.status_style()));
    }
    if let Some((status, _)) = &state.status {
        let is_error = status.starts_with('⚠') || status.starts_with('✘');
        if !spans.is_empty() {
            spans.push(sep());
        }
        let style = if is_error {
            theme.status_error_style()
        } else {
            theme.status_style()
        };
        spans.push(Span::styled(status.clone(), style));
        return (Line::from(spans), is_error);
    }
    (Line::from(spans), false)
}

/// The status row as plain text (tests read the words, not the colors).
#[cfg(test)]
fn status_text(state: &AppState, theme: &Theme) -> (String, bool) {
    let (line, is_error) = status_line(state, theme);
    (line.spans.iter().map(|s| s.content.to_string()).collect(), is_error)
}

/// Compact a session id for the status row.
///
/// Ids look like `tui:<uuid>`; the qualifier is noise in a one-line status bar.
fn short_session(id: &str) -> String {
    let name = id.rsplit(':').next().unwrap_or(id);
    let tail: String = if name.chars().count() > 12 {
        name.chars().take(12).collect()
    } else {
        name.to_string()
    };
    format!("sess {tail}")
}

/// Render the composer's rows and place the cursor.
fn render_composer(f: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let rows = input_rows(state, theme, area.width);
    // The cursor is a byte offset into the buffer; the rows carry the prompt
    // in front of it, so the offset shifts by the prompt's own bytes.
    let (cursor_row, cursor_col) = locate_cursor(&rows, state.input_cursor + PROMPT.len());

    // Keep the cursor's row inside the visible window when the buffer is
    // longer than the composer.
    let max_rows = area.height as usize;
    let start = cursor_row.saturating_sub(max_rows.saturating_sub(1));
    let visible: Vec<Line<'static>> = rows.iter().skip(start).take(max_rows).cloned().collect();

    f.render_widget(Paragraph::new(visible), area);

    let row_in_window = cursor_row.saturating_sub(start) as u16;
    // `cursor_col` is measured over the row as drawn, prompt included.
    let col = cursor_col as u16;
    f.set_cursor_position(ratatui::layout::Position {
        x: area.x + col.min(area.width.saturating_sub(1)),
        y: area.y + row_in_window.min(area.height.saturating_sub(1)),
    });
}

/// Render the approval prompt.
fn approval_lines(state: &AppState, theme: &Theme) -> Option<Vec<Line<'static>>> {
    let approval = state.current_approval()?;
    let mut lines = vec![Line::from(vec![
        Span::styled("Allow ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(approval.tool_name.clone(), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!("?  (risk: {})", approval.risk_level), theme.dim_style()),
    ])];
    lines.push(Line::from(Span::styled(
        if approval.message.is_empty() {
            approval.requested_by.clone()
        } else {
            format!("{} — {}", approval.requested_by, approval.message)
        },
        theme.dim_style(),
    )));
    lines.push(Line::from(Span::styled(
        args_preview(approval.args.as_ref()),
        theme.dim_style(),
    )));
    lines.push(Line::from(vec![
        Span::styled(
            "  y approve  ",
            if state.approval_approve_selected {
                theme.highlight_style()
            } else {
                theme.dim_style()
            },
        ),
        Span::styled(
            "  n deny  ",
            if state.approval_approve_selected {
                theme.dim_style()
            } else {
                theme.highlight_style()
            },
        ),
    ]));
    Some(lines)
}

/// One-line rendering of tool arguments for the prompt.
fn args_preview(args: Option<&Value>) -> String {
    let Some(args) = args else {
        return String::new();
    };
    let text = serde_json::to_string(args).unwrap_or_default();
    let mut out: String = text.chars().take(160).collect();
    if text.chars().count() > 160 {
        out.push('…');
    }
    out
}

/// Render an `ask_user` question.
fn ask_lines(state: &AppState, theme: &Theme) -> Option<Vec<Line<'static>>> {
    let ask = state.pending_ask.as_ref()?;
    let mut lines = vec![Line::from(vec![
        Span::styled("Agent asks: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(ask.question.clone(), Style::default()),
    ])];
    if ask.options.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  answer: {}", state.ask_input),
            Style::default(),
        )));
        lines.push(Line::from(Span::styled("  Enter to send", theme.dim_style())));
    } else {
        let options: Vec<Span> = ask
            .options
            .iter()
            .take(4)
            .enumerate()
            .map(|(i, opt)| {
                let text = format!("  {}. {}  ", i + 1, opt);
                if state.ask_input == (i + 1).to_string() {
                    Span::styled(text, theme.highlight_style())
                } else {
                    Span::styled(text, Style::default())
                }
            })
            .collect();
        lines.push(Line::from(options));
        lines.push(Line::from(Span::styled(
            format!("  or type an answer: {}", state.ask_input),
            theme.dim_style(),
        )));
    }
    Some(lines)
}

/// Chop text to `max` columns, ending with `…` when anything was cut, so one
/// candidate can never wrap into the row budget of the next.
fn truncate_to_width(text: &str, max: usize) -> String {
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    out
}

/// Slash-command candidates while a `/command` is being typed, with the
/// current Tab selection highlighted.
///
/// Returns `None` when the input is not completing a command, so the block
/// area falls back to the stream preview. The list is windowed around the
/// selection: a catalog longer than the block stays readable, and cycling
/// with Tab always keeps the highlighted row on screen.
fn completion_lines(
    state: &AppState,
    theme: &Theme,
    max_rows: usize,
    width: usize,
) -> Option<Vec<Line<'static>>> {
    let candidates = state.completions();
    if candidates.is_empty() || max_rows == 0 {
        return None;
    }
    let selected = state.completion_index.min(candidates.len() - 1);
    let window = max_rows.min(candidates.len());
    let start = selected
        .saturating_sub(window - 1)
        .min(candidates.len() - window);
    let lines = candidates[start..start + window]
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let highlighted = start + i == selected;
            let name_style = if highlighted {
                theme.highlight_style()
            } else {
                Style::default()
            };
            let desc_style = if highlighted {
                theme.highlight_style()
            } else {
                theme.dim_style()
            };
            Line::from(vec![
                Span::styled(format!("  /{:<12}", cmd.name), name_style),
                Span::styled(
                    truncate_to_width(&cmd.description, width.saturating_sub(16)),
                    desc_style,
                ),
            ])
        })
        .collect();
    Some(lines)
}

/// Render the whole live region.
pub fn render(f: &mut Frame, state: &AppState) {
    let theme = &state.active_theme;
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let composer_rows =
        (input_rows(state, theme, area.width).len() as u16).clamp(1, COMPOSER_MAX_ROWS);
    let l = layout(area, composer_rows);

    // Block area: a blocking prompt takes precedence over the stream preview,
    // and a completion list takes precedence while a `/command` is being
    // typed — the typist's attention is on the command, not the stream.
    let block_lines = match state.live_mode {
        LiveMode::Approval => approval_lines(state, theme),
        LiveMode::Ask => ask_lines(state, theme),
        LiveMode::Composer => {
            completion_lines(state, theme, l.block.height as usize, l.block.width as usize)
        }
    };
    let block_lines = match block_lines {
        Some(lines) => Some(lines),
        None => {
            let preview = state.transcript.preview(l.block.height as usize);
            if preview.is_empty() {
                None
            } else {
                Some(blocks::to_lines(&preview, theme))
            }
        }
    };
    if let Some(lines) = block_lines {
        if l.block.height > 0 {
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), l.block);
        }
    }

    if let Some(status) = l.status {
        let (line, _) = status_line(state, theme);
        f.render_widget(Paragraph::new(line), status);
    }

    render_composer(f, state, theme, l.composer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::gateway_calls::{ApprovalDetail, SessionInfo};
    use crate::tui::state::{AskPrompt, ConnectionState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn rect(y: u16, height: u16) -> Rect {
        Rect { x: 0, y, width: 40, height }
    }

    /// The layout must never produce overlapping rows, at any height.
    #[test]
    fn layout_never_overlaps_at_any_height() {
        for height in 1..=LIVE_HEIGHT {
            let area = rect(0, height);
            let l = layout(area, 1);
            assert!(l.composer.height >= 1, "height {height}");
            assert_eq!(l.composer.y + l.composer.height, area.height);
            if let Some(status) = l.status {
                assert!(status.y < l.composer.y, "height {height}");
                assert!(l.block.y + l.block.height <= status.y);
            } else {
                assert!(l.block.y + l.block.height <= l.composer.y, "height {height}");
            }
        }
    }

    #[test]
    fn the_composer_grows_with_the_input_up_to_its_cap() {
        let area = rect(0, LIVE_HEIGHT);
        assert_eq!(layout(area, 1).composer.height, 1);
        assert_eq!(layout(area, 3).composer.height, 3);
        assert_eq!(layout(area, 9).composer.height, COMPOSER_MAX_ROWS);
    }

    #[test]
    fn cursor_location_tracks_wrapped_rows() {
        let rows = wrapmod::wrap_lines(&[Line::from("hello world")], 6);
        assert_eq!(rows.len(), 2);
        assert_eq!(locate_cursor(&rows, 0), (0, 0));
        assert_eq!(locate_cursor(&rows, 5), (0, 5));
        // Byte 6 is the first character of the second row.
        assert_eq!(locate_cursor(&rows, 6), (1, 0));
    }

    #[test]
    fn cursor_location_counts_wide_characters_as_two_columns() {
        let rows = wrapmod::wrap_lines(&[Line::from("中文")], 20);
        assert_eq!(locate_cursor(&rows, "中".len()), (0, 2));
    }

    #[test]
    fn session_ids_are_shortened_for_the_status_row() {
        assert_eq!(short_session("tui:anonymous"), "sess anonymous");
        assert_eq!(short_session("tui:1234567890abcdef"), "sess 1234567890ab");
    }

    #[test]
    fn elapsed_formats_for_the_status_row() {
        assert_eq!(format_elapsed(3), "3s");
        assert_eq!(format_elapsed(83), "1m 23s");
    }

    #[test]
    fn a_running_turn_shows_the_word_the_time_and_the_phase() {
        let mut state = AppState::default();
        state.begin_run();

        state.run_phase = RunPhase::Thinking;
        let (text, _) = status_text(&state, &Theme::dark());
        assert!(text.contains('…'), "the whimsical word, trailing dots: {text}");
        assert!(text.contains("(0s · thinking)"), "elapsed plus phase: {text}");
        assert!(text.contains("esc stops"), "the way out is still labelled: {text}");

        state.run_phase = RunPhase::ToolCall("file_read".into());
        let (text, _) = status_text(&state, &Theme::dark());
        assert!(text.contains("⚙ file_read"), "the tool in flight: {text}");

        // A turn that has only just been sent says nothing beyond the time.
        state.run_phase = RunPhase::Waiting;
        let (text, _) = status_text(&state, &Theme::dark());
        assert!(text.contains("(0s)"), "no phase hint while waiting: {text}");
    }

    /// The idle row is the connection, the agent and the session — and
    /// nothing left over from the turn that just finished.
    #[test]
    fn an_idle_row_carries_no_run_leftovers() {
        let mut state = AppState::default();
        let (text, _) = status_text(&state, &Theme::dark());
        assert_eq!(text, "disconnected", "nothing but the connection");

        state.current_session = Some("tui:1234567890".into());
        state.current_agent = Some("secretary".into());
        state.set_status("");
        let (text, _) = status_text(&state, &Theme::dark());
        assert!(text.contains("secretary"));
        assert!(text.contains("sess"));
        assert!(!text.contains("tokens"), "no token meter once idle: {text}");
    }

    /// The run indicator is an animation with a color, not static text — and
    /// both go away the moment the turn ends.
    #[test]
    fn the_spinner_animates_in_color_and_leaves_with_the_run() {
        let mut state = AppState {
            connection: ConnectionState::Connected {
                features: vec![],
                scopes_granted: vec![],
                server_version: "0.3.6".into(),
            },
            ..AppState::default()
        };
        state.begin_run();

        let (line, _) = status_line(&state, &Theme::dark());
        let frame = line.spans[0].content.to_string();
        let accent = Theme::dark().accent;
        assert_eq!(line.spans[0].style.fg, Some(accent), "the frame is colored");
        // The word rides the same color, the facts do not.
        assert_eq!(line.spans[1].style.fg, Some(accent), "the word is colored");
        assert!(line.spans[1].content.ends_with('…'));
        assert_ne!(line.spans[2].style.fg, Some(accent), "the facts stay plain");

        state.spinner = state.spinner.wrapping_add(1);
        let (next, _) = status_line(&state, &Theme::dark());
        assert_ne!(next.spans[0].content, frame, "the frame moves with the tick");

        // Turn over: nothing of the indicator survives.
        state.end_run();
        let (text, _) = status_text(&state, &Theme::dark());
        assert!(!text.contains('…'), "no word after the run: {text}");
        assert!(!text.contains("esc stops"), "no hint after the run: {text}");
        for frame in ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"] {
            assert!(!text.contains(frame), "no frame after the run: {text}");
        }
    }

    /// A command catalog of `n` entries named `c0..c{n-1}`.
    fn catalog(n: usize) -> Vec<crate::tui::state::CommandInfo> {
        (0..n)
            .map(|i| crate::tui::state::CommandInfo {
                name: format!("c{i}"),
                description: format!("command {i}"),
                ..Default::default()
            })
            .collect()
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn typing_a_slash_lists_the_commands_with_the_selection_highlighted() {
        let mut state = AppState {
            command_list: catalog(3),
            ..AppState::default()
        };
        state.set_input("/".into());
        let lines = completion_lines(&state, &Theme::dark(), 6, 80)
            .expect("a bare slash lists every command");
        assert_eq!(lines.len(), 3);
        assert!(line_text(&lines[0]).contains("/c0"));
        assert!(line_text(&lines[0]).contains("command 0"));
        // The first candidate starts highlighted, for the Tab that applies it.
        assert_eq!(lines[0].spans[0].style, Theme::dark().highlight_style());
        assert_eq!(lines[1].spans[0].style, Style::default());

        // Anything not starting with `/` is not completing: the block area
        // must fall back to the stream preview.
        state.set_input("hello".into());
        assert!(completion_lines(&state, &Theme::dark(), 6, 80).is_none());
    }

    #[test]
    fn the_completion_window_follows_the_tab_selection() {
        let mut state = AppState {
            command_list: catalog(10),
            ..AppState::default()
        };
        state.set_input("/".into());
        state.completion_index = 8;
        let lines = completion_lines(&state, &Theme::dark(), 3, 80).expect("completions");
        assert_eq!(lines.len(), 3, "capped at the block height");
        // The window ends on the selection: c6, c7, c8.
        assert!(line_text(&lines[2]).contains("/c8"), "selection visible: {lines:?}");
        assert_eq!(lines[2].spans[0].style, Theme::dark().highlight_style());
    }

    #[test]
    fn a_long_description_is_chopped_rather_than_wrapped() {
        let mut state = AppState {
            command_list: vec![crate::tui::state::CommandInfo {
                name: "c".into(),
                description: "x".repeat(100),
                ..Default::default()
            }],
            ..AppState::default()
        };
        state.set_input("/".into());
        let lines = completion_lines(&state, &Theme::dark(), 3, 40).expect("completions");
        let text = line_text(&lines[0]);
        assert!(UnicodeWidthStr::width(text.as_str()) <= 40, "one row, no wrap: {text}");
        assert!(text.ends_with('…'), "the cut is marked: {text}");
    }

    /// The row has to survive a plain 80-column terminal, which is the narrow
    /// case that matters: the run indicator is the longest thing that can
    /// appear in it, and a row that overflows loses its *tail* — the session
    /// id — which is exactly what a user goes looking for.
    #[test]
    fn the_running_row_fits_an_eighty_column_terminal() {
        let mut state = AppState {
            connection: ConnectionState::Connected {
                features: vec![],
                scopes_granted: vec![],
                server_version: "0.3.6".into(),
            },
            current_session: Some("tui:1234567890abcdef".into()),
            current_agent: Some("secretary".into()),
            ..AppState::default()
        };
        state.begin_run();
        state.run_phase = RunPhase::ToolCall("file_read".into());
        let (text, _) = status_text(&state, &Theme::dark());
        assert!(
            UnicodeWidthStr::width(text.as_str()) <= 80,
            "the running row is {} columns: {text}",
            UnicodeWidthStr::width(text.as_str())
        );
    }

    #[test]
    fn status_row_reports_connection_and_session() {
        let mut state = AppState {
            connection: ConnectionState::Lost("gateway gone".into()),
            current_session: Some("tui:1234567890".into()),
            current_agent: Some("secretary".into()),
            ..AppState::default()
        };
        let (text, is_error) = status_text(&state, &Theme::dark());
        assert!(text.contains("gateway gone"));
        assert!(text.contains("secretary"));
        assert!(text.contains("sess"));
        assert!(!is_error);

        state.set_status("⚠ could not reach the gateway");
        let (_, is_error) = status_text(&state, &Theme::dark());
        assert!(is_error, "a warning is styled as an error");
    }

    #[test]
    fn approval_prompt_shows_the_tool_risk_and_arguments() {
        let mut state = AppState {
            live_mode: LiveMode::Approval,
            ..AppState::default()
        };
        state.approvals.push_back(ApprovalDetail {
            id: "ap1".into(),
            tool_name: "file_write".into(),
            risk_level: "High".into(),
            requested_by: "secretary".into(),
            message: "outside the workspace".into(),
            args: Some(serde_json::json!({ "path": "/tmp/x" })),
        });
        let lines = approval_lines(&state, &Theme::dark()).expect("a prompt");
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("file_write"));
        assert!(text.contains("High"));
        assert!(text.contains("/tmp/x"));
    }

    /// Nothing in the live region may panic or paint outside its area when the
    /// terminal is tiny.
    #[test]
    fn renders_without_panicking_at_any_size() {
        for (w, h) in [(1, 1), (2, 2), (10, 3), (40, 4), (80, LIVE_HEIGHT)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
            let mut state = AppState::default();
            state.sessions.push(SessionInfo::default());
            state.pending_ask = Some(AskPrompt {
                ask_id: "a1".into(),
                question: "which one?".into(),
                options: vec!["one".into(), "two".into()],
                required: true,
                default: None,
            });
            state.live_mode = LiveMode::Ask;
            terminal
                .draw(|f| render(f, &state))
                .unwrap_or_else(|e| panic!("draw failed at {w}x{h}: {e}"));
        }
    }
}
