//! Writing finished lines into the terminal's scrollback.
//!
//! This is the only place that calls `Terminal::insert_before`, and it exists
//! because of two things that API does not do for you:
//!
//! - it does **not** wrap: a line longer than the viewport is truncated, so
//!   everything is wrapped here first (see [`crate::tui::ui::wrap`]), and
//! - it **clears the viewport**, so the caller must redraw the live region
//!   afterwards — before the next paint, the bottom of the screen is blank.
// INVARIANTS-NONE: terminal writer; holds no state.

use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;
use ratatui::Terminal;

use crate::tui::ui::wrap;

/// Wrap `lines` to `width` and push them above the live region.
///
/// Returns the number of rows written, so the caller knows whether it owes the
/// terminal a repaint.
pub fn flush<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: &[Line<'_>],
    width: u16,
) -> Result<u16, B::Error> {
    if lines.is_empty() || width == 0 {
        return Ok(0);
    }
    let wrapped = wrap::wrap_lines(lines, width as usize);
    if wrapped.is_empty() {
        return Ok(0);
    }
    let height = wrapped.len().min(u16::MAX as usize) as u16;

    terminal.insert_before(height, |buf: &mut Buffer| {
        let area = buf.area;
        for (idx, line) in wrapped.iter().enumerate() {
            let row = Rect {
                x: area.x,
                y: area.y + idx as u16,
                width: area.width,
                height: 1,
            };
            line.render(row, buf);
        }
    })?;

    Ok(height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;
    use ratatui::widgets::Paragraph;
    use ratatui::{TerminalOptions, Viewport};

    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 8;
    const VIEWPORT: u16 = 3;

    /// A terminal whose viewport is a small strip at the bottom, so inserted
    /// lines have to scroll off the top once they stop fitting.
    fn terminal() -> Terminal<TestBackend> {
        let mut terminal = Terminal::with_options(
            TestBackend::new(WIDTH, HEIGHT),
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT),
            },
        )
        .expect("inline terminal");
        terminal
            .set_cursor_position(Position::new(0, 0))
            .expect("cursor home");
        terminal
    }

    /// Lines that have scrolled off the top of the screen.
    fn scrolled_off(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .scrollback()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// Everything currently on screen, one string per row.
    fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Row the live region starts on: the viewport is anchored to the cursor
    /// row when it is created and moves down as lines are inserted above it,
    /// so it is located by where the last draw landed rather than assumed to
    /// sit at the bottom of the screen.
    fn marker_row(terminal: &Terminal<TestBackend>, marker: &str) -> Option<usize> {
        rows(terminal).iter().position(|r| r.contains(marker))
    }

    /// Everything the terminal holds above the live region.
    fn above(terminal: &Terminal<TestBackend>, viewport_top: usize) -> String {
        rows(terminal)[..viewport_top].join("\n")
    }

    fn draw_live(terminal: &mut Terminal<TestBackend>, text: &str) {
        terminal
            .draw(|f| f.render_widget(Paragraph::new(text.to_string()), f.area()))
            .expect("draw");
    }

    /// One loop iteration, in order: flush, then repaint the live region. The
    /// frozen line must end up above the live region, and the live region must
    /// be painted again afterwards — `insert_before` blanks it.
    #[test]
    fn flushing_moves_content_above_the_live_region() {
        let mut terminal = terminal();
        draw_live(&mut terminal, "composer");
        let before = marker_row(&terminal, "composer").expect("composer drawn");

        let rows_written =
            flush(&mut terminal, &[Line::from("frozen line")], WIDTH).expect("flush");
        assert_eq!(rows_written, 1);
        draw_live(&mut terminal, "composer");

        let live_top = marker_row(&terminal, "composer").expect("the composer is painted again");
        assert!(live_top > before, "the live region moved down to make room");
        let frozen = marker_row(&terminal, "frozen line").expect("the frozen line is on screen");
        assert!(frozen < live_top, "frozen content sits above the live region");
        assert!(above(&terminal, live_top).contains("frozen line"));
    }

    /// Once there is no room left, the oldest lines scroll off into the
    /// terminal's own scrollback — that is what makes it history rather than a
    /// buffer we have to keep.
    #[test]
    fn content_that_stops_fitting_scrolls_into_the_scrollback() {
        let mut terminal = terminal();
        let lines: Vec<Line<'static>> = (0..30)
            .map(|i| Line::from(format!("scrolled line {i}")))
            .collect();
        let written = flush(&mut terminal, &lines, WIDTH).expect("flush");
        assert_eq!(written, 30);
        assert!(scrolled_off(&terminal).contains("scrolled line 0"));
    }

    /// The whole reason wrapping lives in this module: `insert_before` drops
    /// the tail of an over-wide line instead of wrapping it.
    #[test]
    fn long_lines_are_wrapped_rather_than_truncated() {
        let mut terminal = terminal();
        let long = "word ".repeat(20);
        flush(&mut terminal, &[Line::from(long)], WIDTH).expect("flush");

        let mut seen = rows(&terminal).join("\n");
        seen.push_str(&scrolled_off(&terminal));
        assert_eq!(seen.matches("word").count(), 20, "every word must survive the round trip");
    }

    #[test]
    fn styled_spans_keep_their_style() {
        let mut terminal = terminal();
        let line = Line::from(Span::styled("coloured", Style::default().fg(Color::Red)));
        flush(&mut terminal, &[line], WIDTH).expect("flush");

        let red_cells = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .chain(terminal.backend().scrollback().content.iter())
            .filter(|c| c.fg == Color::Red)
            .count();
        assert_eq!(red_cells, "coloured".len());
    }

    #[test]
    fn nothing_to_flush_is_not_an_error() {
        let mut terminal = terminal();
        assert_eq!(flush(&mut terminal, &[], WIDTH).expect("flush"), 0);
    }
}
