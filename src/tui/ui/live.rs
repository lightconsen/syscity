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
use unicode_width::UnicodeWidthStr;

use crate::tui::state::{AppState, LiveMode, RunPhase};
use crate::tui::ui::blocks;
use crate::tui::ui::wrap as wrapmod;
use crate::tui::ui::{dim_style, highlight_style, prompt_style, status_error_style, status_style};

/// Total rows the live region occupies.
///
/// `Viewport::Inline(LIVE_HEIGHT)` is fixed for the process's lifetime.
pub const LIVE_HEIGHT: u16 = 8;

/// Rows the composer may grow to before it scrolls internally.
const COMPOSER_MAX_ROWS: u16 = 3;

/// Width of the `> ` prompt marker in front of the first input row.
const PROMPT_WIDTH: u16 = 2;

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

/// Wrap the input buffer to the composer width.
fn input_rows(state: &AppState, width: u16) -> Vec<Line<'static>> {
    if state.input_buffer.is_empty() {
        return vec![Line::from("")];
    }
    wrapmod::wrap_lines(&[Line::from(state.input_buffer.clone())], width as usize)
}

/// `1m 23s` past a minute, bare seconds below it.
fn format_elapsed(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// `20.6k` past a thousand, the raw number below it.
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
}

/// The one-line status row.
fn status_text(state: &AppState) -> (String, bool) {
    let mut parts: Vec<String> = Vec::new();
    // A running row is longer than an idle one, and what it pushes off the end
    // is the session id. The server version is the least useful thing here and
    // does not change mid-session, so it yields the space. A connection that is
    // *not* fine still speaks up: that is not noise.
    if !state.is_running || !state.connection.is_connected() {
        parts.push(state.connection.label());
    }
    if let Some(secs) = state.run_elapsed_secs() {
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
        parts.push(format!("{}… ({detail}) — esc stops", state.spinner_word()));
    }
    if !state.is_running {
        if let Some(tokens) = state.last_turn_tokens {
            parts.push(format!("↓ {} tokens", format_tokens(tokens)));
        }
    }
    if let Some(agent) = state.current_agent_info() {
        parts.push(format!("{} {}", agent.emoji, agent.display_name));
    } else if let Some(id) = state.current_agent.as_deref() {
        parts.push(id.to_string());
    }
    if let Some(session) = state.current_session.as_deref() {
        parts.push(short_session(session));
    }
    if let Some((status, _)) = &state.status {
        let is_error = status.starts_with('⚠') || status.starts_with('✘');
        parts.push(status.clone());
        return (parts.join("  ·  "), is_error);
    }
    (parts.join("  ·  "), false)
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
fn render_composer(f: &mut Frame, state: &AppState, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let rows = input_rows(state, area.width);
    let (cursor_row, cursor_col) = locate_cursor(&rows, state.input_cursor);

    // Keep the cursor's row inside the visible window when the buffer is
    // longer than the composer.
    let max_rows = area.height as usize;
    let start = cursor_row.saturating_sub(max_rows.saturating_sub(1));
    let visible: Vec<Line<'static>> = rows
        .iter()
        .skip(start)
        .take(max_rows)
        .enumerate()
        .map(|(idx, row)| {
            if start + idx == 0 {
                let mut spans = vec![Span::styled("> ", prompt_style())];
                spans.extend(row.spans.iter().cloned());
                Line::from(spans)
            } else {
                row.clone()
            }
        })
        .collect();

    f.render_widget(Paragraph::new(visible), area);

    let row_in_window = cursor_row.saturating_sub(start) as u16;
    let col = cursor_col as u16 + if cursor_row == 0 { PROMPT_WIDTH } else { 0 };
    f.set_cursor_position(ratatui::layout::Position {
        x: area.x + col.min(area.width.saturating_sub(1)),
        y: area.y + row_in_window.min(area.height.saturating_sub(1)),
    });
}

/// Render the approval prompt.
fn approval_lines(state: &AppState) -> Option<Vec<Line<'static>>> {
    let approval = state.current_approval()?;
    let mut lines = vec![Line::from(vec![
        Span::styled("Allow ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(approval.tool_name.clone(), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!("?  (risk: {})", approval.risk_level), dim_style()),
    ])];
    lines.push(Line::from(Span::styled(
        if approval.message.is_empty() {
            approval.requested_by.clone()
        } else {
            format!("{} — {}", approval.requested_by, approval.message)
        },
        dim_style(),
    )));
    lines.push(Line::from(Span::styled(args_preview(approval.args.as_ref()), dim_style())));
    lines.push(Line::from(vec![
        Span::styled(
            "  y approve  ",
            if state.approval_approve_selected {
                highlight_style()
            } else {
                dim_style()
            },
        ),
        Span::styled(
            "  n deny  ",
            if state.approval_approve_selected {
                dim_style()
            } else {
                highlight_style()
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
fn ask_lines(state: &AppState) -> Option<Vec<Line<'static>>> {
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
        lines.push(Line::from(Span::styled("  Enter to send", dim_style())));
    } else {
        let options: Vec<Span> = ask
            .options
            .iter()
            .take(4)
            .enumerate()
            .map(|(i, opt)| {
                let text = format!("  {}. {}  ", i + 1, opt);
                if state.ask_input == (i + 1).to_string() {
                    Span::styled(text, highlight_style())
                } else {
                    Span::styled(text, Style::default())
                }
            })
            .collect();
        lines.push(Line::from(options));
        lines.push(Line::from(Span::styled(
            format!("  or type an answer: {}", state.ask_input),
            dim_style(),
        )));
    }
    Some(lines)
}

/// Render the whole live region.
pub fn render(f: &mut Frame, state: &AppState) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let composer_rows = (input_rows(state, area.width).len() as u16).clamp(1, COMPOSER_MAX_ROWS);
    let l = layout(area, composer_rows);

    // Block area: a blocking prompt takes precedence over the stream preview.
    let block_lines = match state.live_mode {
        LiveMode::Approval => approval_lines(state),
        LiveMode::Ask => ask_lines(state),
        LiveMode::Composer => None,
    };
    let block_lines = match block_lines {
        Some(lines) => Some(lines),
        None => {
            let preview = state.transcript.preview(l.block.height as usize);
            if preview.is_empty() {
                None
            } else {
                Some(blocks::to_lines(&preview))
            }
        }
    };
    if let Some(lines) = block_lines {
        if l.block.height > 0 {
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), l.block);
        }
    }

    if let Some(status) = l.status {
        let (text, is_error) = status_text(state);
        let style = if is_error {
            status_error_style()
        } else {
            status_style()
        };
        f.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), status);
    }

    render_composer(f, state, l.composer);
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
    fn elapsed_and_tokens_format_for_the_status_row() {
        assert_eq!(format_elapsed(3), "3s");
        assert_eq!(format_elapsed(83), "1m 23s");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(20_600), "20.6k");
    }

    #[test]
    fn a_running_turn_shows_the_word_the_time_and_the_phase() {
        let mut state = AppState::default();
        state.begin_run();

        state.run_phase = RunPhase::Thinking;
        let (text, _) = status_text(&state);
        assert!(text.contains('…'), "the whimsical word, trailing dots: {text}");
        assert!(text.contains("(0s · thinking)"), "elapsed plus phase: {text}");
        assert!(text.contains("esc stops"), "the way out is still labelled: {text}");

        state.run_phase = RunPhase::ToolCall("file_read".into());
        let (text, _) = status_text(&state);
        assert!(text.contains("⚙ file_read"), "the tool in flight: {text}");

        // A turn that has only just been sent says nothing beyond the time.
        state.run_phase = RunPhase::Waiting;
        let (text, _) = status_text(&state);
        assert!(text.contains("(0s)"), "no phase hint while waiting: {text}");
    }

    #[test]
    fn an_idle_row_carries_the_last_turns_tokens() {
        let mut state = AppState::default();
        let (text, _) = status_text(&state);
        assert!(!text.contains("tokens"), "nothing to report yet: {text}");

        state.last_turn_tokens = Some(20_600);
        let (text, _) = status_text(&state);
        assert!(text.contains("↓ 20.6k tokens"), "the completed turn's meter: {text}");
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
        let (text, _) = status_text(&state);
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
        let (text, is_error) = status_text(&state);
        assert!(text.contains("gateway gone"));
        assert!(text.contains("secretary"));
        assert!(text.contains("sess"));
        assert!(!is_error);

        state.set_status("⚠ could not reach the gateway");
        let (_, is_error) = status_text(&state);
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
        let lines = approval_lines(&state).expect("a prompt");
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
