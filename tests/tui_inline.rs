//! Integration tests for the inline TUI.
//!
//! These drive the real rendering path — [`scrollback::flush`] followed by a
//! draw of the live region — against `TestBackend`, whose inline viewport
//! carries a real scrollback buffer. That makes the central invariant
//! assertable: a line is either in the scrollback or in the live region, never
//! both, and never lost.

use ratatui::backend::TestBackend;
use ratatui::layout::Position;
use ratatui::text::Line;
use ratatui::{Terminal, TerminalOptions, Viewport};

use syscity::tui::scrollback;
use syscity::tui::state::{AppState, ConnectionState, LiveMode};
use syscity::tui::transcript::LineKind;
use syscity::tui::ui::{blocks, live};

const WIDTH: u16 = 60;
const HEIGHT: u16 = 12;

/// A terminal with an inline viewport, anchored at the top of the screen.
fn terminal() -> Terminal<TestBackend> {
    let mut terminal = Terminal::with_options(
        TestBackend::new(WIDTH, HEIGHT),
        TerminalOptions {
            viewport: Viewport::Inline(live::LIVE_HEIGHT),
        },
    )
    .expect("inline terminal");
    terminal
        .set_cursor_position(Position::new(0, 0))
        .expect("cursor home");
    terminal
}

/// Everything currently painted on screen.
fn visible(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect()
}

/// Everything the terminal holds, on screen or scrolled off the top.
///
/// Small writes stay on screen; once the screen is full the oldest lines move
/// into the backend's scrollback. Either way nothing may be lost.
fn everything(terminal: &Terminal<TestBackend>) -> String {
    let mut text = visible(terminal);
    text.push_str(
        &terminal
            .backend()
            .scrollback()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>(),
    );
    text
}

/// The two halves of one loop iteration: flush what has graduated, then
/// repaint the live region.
fn settle(terminal: &mut Terminal<TestBackend>, state: &mut AppState) {
    let pending = state.transcript.take_flushable();
    if !pending.is_empty() {
        let lines = blocks::to_lines(&pending);
        scrollback::flush(terminal, &lines, WIDTH).expect("flush");
    }
    terminal
        .draw(|f| live::render(f, state))
        .expect("draw the live region");
}

fn state_with(mode: LiveMode) -> AppState {
    AppState {
        live_mode: mode,
        ..AppState::default()
    }
}

#[test]
fn the_composer_is_drawn_and_stays_after_a_flush() {
    let mut terminal = terminal();
    let mut state = state_with(LiveMode::Composer);
    settle(&mut terminal, &mut state);
    assert!(
        visible(&terminal).contains("> "),
        "the composer should be visible: {:?}",
        visible(&terminal)
    );
}

#[test]
fn the_status_row_reports_the_connection() {
    let mut terminal = terminal();
    let mut state = AppState {
        connection: ConnectionState::Lost("gateway offline".to_string()),
        ..AppState::default()
    };
    settle(&mut terminal, &mut state);
    assert!(
        visible(&terminal).contains("offline"),
        "status row should say so: {:?}",
        visible(&terminal)
    );
}

/// A pending approval takes over the live region, and the composer stops
/// accepting text — this is the whole point of the blocking prompt.
#[test]
fn an_approval_prompt_is_rendered_in_the_live_region() {
    use syscity::tui::gateway_calls::ApprovalDetail;
    let mut terminal = terminal();
    let mut state = state_with(LiveMode::Approval);
    state.approvals.push_back(ApprovalDetail {
        id: "ap1".to_string(),
        tool_name: "file_write".to_string(),
        risk_level: "High".to_string(),
        requested_by: "secretary".to_string(),
        message: "writes outside the workspace".to_string(),
        args: Some(serde_json::json!({ "path": "/etc/hosts" })),
    });
    settle(&mut terminal, &mut state);

    let screen = visible(&terminal);
    assert!(screen.contains("file_write"), "got {screen:?}");
    assert!(screen.contains("High"), "got {screen:?}");
    assert!(screen.contains("/etc/hosts"), "the args must be shown: {screen:?}");
}

/// An over-wide line must wrap into the scrollback rather than being cut off —
/// `insert_before` truncates silently, so this is the guard for that.
#[test]
fn long_frozen_lines_are_not_truncated() {
    let mut terminal = terminal();
    let long = format!("{}END", "x".repeat(200));
    let lines = vec![Line::from(long.clone())];
    scrollback::flush(&mut terminal, &lines, WIDTH).expect("flush");
    let seen = everything(&terminal);
    assert!(seen.contains("END"), "the tail must survive: {seen:?}");
}

#[test]
fn a_streamed_turn_freezes_into_scrollback_when_it_finishes() {
    let mut terminal = terminal();
    let mut state = AppState::default();
    state
        .transcript
        .push_delta("assistant", LineKind::Assistant, "first line\nsecond");
    // Mid-stream, only the completed line has graduated.
    let mid = state.transcript.take_flushable();
    assert_eq!(mid.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["first line"]);
    scrollback::flush(&mut terminal, &blocks::to_lines(&mid), WIDTH).expect("flush");
    assert!(everything(&terminal).contains("first line"));
    assert!(
        !everything(&terminal).contains("second"),
        "the unfinished line stays live until its newline arrives"
    );

    state
        .transcript
        .finish_stream("assistant", Some("first line\nsecond\n"));
    let rest = state.transcript.take_flushable();
    scrollback::flush(&mut terminal, &blocks::to_lines(&rest), WIDTH).expect("flush");
    assert!(everything(&terminal).contains("second"));
    // And the live region is still usable afterwards.
    settle(&mut terminal, &mut state);
    assert!(visible(&terminal).contains("> "));
}

/// The cursor must sit where the user is typing, at any input size.
#[test]
fn the_cursor_follows_the_input() {
    let mut terminal = terminal();
    let mut state = AppState {
        input_buffer: "hello".to_string(),
        input_cursor: 5,
        ..AppState::default()
    };
    settle(&mut terminal, &mut state);
    let position = terminal.get_cursor_position().expect("a cursor position");
    // Prompt marker (2 columns) plus the five typed characters.
    assert_eq!(position.x, 7);

    state.input_buffer = "中文".to_string();
    state.input_cursor = "中文".len();
    settle(&mut terminal, &mut state);
    let position = terminal.get_cursor_position().expect("a cursor position");
    assert_eq!(position.x, 6, "wide characters take two columns each");
}

/// A tiny terminal must not panic the render path.
#[test]
fn renders_at_degenerate_sizes() {
    for (w, h) in [(1u16, 1u16), (20, 2), (40, 3), (WIDTH, HEIGHT)] {
        let mut terminal = Terminal::with_options(
            TestBackend::new(w, h),
            TerminalOptions {
                viewport: Viewport::Inline(live::LIVE_HEIGHT),
            },
        )
        .expect("terminal");
        let state = state_with(LiveMode::Composer);
        let lines = vec![Line::from("frozen content")];
        scrollback::flush(&mut terminal, &lines, w).expect("flush");
        terminal.draw(|f| live::render(f, &state)).expect("draw");
    }
}
