//! Width-correct wrapping for scrollback writes.
//!
//! `Terminal::insert_before` renders into a buffer exactly the viewport's
//! width and blits the cells: a line that is too long does **not** wrap, its
//! tail is silently dropped. Everything written to scrollback therefore has to
//! be wrapped here first, and the row count passed to `insert_before` has to
//! match what this produces.
//!
//! Wrapping is display-width aware (`unicode-width`), so CJK and emoji — two
//! columns per character — wrap where they visually should. Styles survive:
//! spans are split at the wrap points and regrouped per row.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// One character and the style it carries.
type Cell = (char, Style);

/// Flatten a line into its characters, keeping the style of each.
fn cells(line: &Line<'_>) -> Vec<Cell> {
    let mut out = Vec::new();
    for span in &line.spans {
        for ch in span.content.chars() {
            out.push((ch, span.style));
        }
    }
    out
}

/// Rebuild a line from a run of styled characters, merging equal neighbours
/// back into single spans.
fn line_of(run: &[Cell]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut style = Style::default();
    for (ch, s) in run {
        if !buf.is_empty() && *s != style {
            spans.push(Span::styled(std::mem::take(&mut buf), style));
        }
        style = *s;
        buf.push(*ch);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
}

/// Display width of a character, treating zero-width (combining marks) as 0.
fn width_of(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Wrap one line to `width` columns.
///
/// Breaks at the last space on the row when there is one, so words are not cut
/// in half; falls back to a hard break for a word longer than the row (a URL,
/// a path). No character is ever dropped, and a double-width character that
/// does not fit in the last column moves to the next row rather than
/// overflowing.
pub fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    wrap_line_hanging(line, width, 0)
}

/// Wrap one line, keeping the first `hang` columns of the first row whole.
///
/// A caller that draws a marker in front of the text — the composer's `> ` —
/// needs the text to wrap *after* that marker, not at the space inside it.
/// Without this the marker's own space is the last one the row contains when
/// it fills, so the row ends up holding the marker alone and the text starts
/// on the next one. `hang` is measured in columns and only applies to the
/// first row; a wrapped continuation row is ordinary text.
pub fn wrap_line_hanging(line: &Line<'_>, width: usize, hang: usize) -> Vec<Line<'static>> {
    let chars = cells(line);
    if width == 0 {
        return vec![line_of(&chars)];
    }

    let width_of_run = |run: &[Cell]| -> usize { run.iter().map(|(c, _)| width_of(*c)).sum() };

    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut row: Vec<Cell> = Vec::new();
    let mut used = 0usize;
    // Index within `row` just past the most recent space — the preferred break.
    let mut last_break: Option<usize> = None;

    for (ch, style) in chars {
        let w = width_of(ch);
        // Break *before* adding the character, as many times as it takes: a
        // double-width character that does not fit the last column gets its
        // own row rather than overflowing.
        while used + w > width && !row.is_empty() {
            match last_break {
                // Break after the space: the space stays on the row it ended.
                Some(at) => {
                    let rest = row.split_off(at);
                    rows.push(std::mem::take(&mut row));
                    used = width_of_run(&rest);
                    row = rest;
                }
                None => {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
            }
            last_break = None;
        }
        // A run of leading whitespace is indentation, not a place to break —
        // otherwise a wrapped code line would break off its own indent. Nor is
        // anything inside the hanging prefix, which must stay put.
        if ch == ' ' && used >= hang && row.iter().any(|(c, _)| !c.is_whitespace()) {
            last_break = Some(row.len() + 1);
        }
        used += w;
        row.push((ch, style));
    }
    rows.push(row);

    rows.iter().map(|r| line_of(r)).collect()
}

/// Wrap a batch of lines, preserving blank lines.
pub fn wrap_lines(lines: &[Line<'_>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for line in lines {
        out.extend(wrap_line(line, width));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use unicode_width::UnicodeWidthStr;

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The property the scrollback writer depends on: nothing over-wide ever
    /// reaches `insert_before`, and nothing is lost on the way.
    fn assert_widths(input: &str, width: usize) -> Vec<String> {
        let line = Line::from(input.to_string());
        let wrapped = wrap_line(&line, width);
        for w in &wrapped {
            assert!(
                UnicodeWidthStr::width(text(w).as_str()) <= width,
                "row {:?} exceeds width {width}",
                text(w)
            );
        }
        let joined: String = wrapped.iter().map(text).collect();
        assert_eq!(joined, input, "wrapping must not lose or add characters");
        wrapped.iter().map(text).collect()
    }

    #[test]
    fn wraps_prose_at_spaces() {
        assert_eq!(assert_widths("the quick brown fox", 10), vec!["the quick ", "brown fox"]);
    }

    #[test]
    fn hard_breaks_a_word_longer_than_the_row() {
        assert_eq!(assert_widths("abcdefghijkl", 5), vec!["abcde", "fghij", "kl"]);
    }

    #[test]
    fn counts_wide_characters_as_two_columns() {
        // Six CJK characters are twelve columns: three rows of four.
        assert_eq!(assert_widths("中文测试汉字", 4), vec!["中文", "测试", "汉字"]);
    }

    #[test]
    fn moves_a_wide_character_that_does_not_fit_the_last_column() {
        assert_eq!(assert_widths("中文", 3), vec!["中", "文"]);
    }

    #[test]
    fn keeps_leading_indentation_of_code_lines() {
        assert_eq!(assert_widths("    indented", 8), vec!["    inde", "nted"]);
    }

    /// A marker in front of the text must stay attached to the first row.
    #[test]
    fn a_hanging_prefix_is_not_a_break_point() {
        let line = Line::from("> abcdef");
        // Without the hang the space after `>` is the last one on the row, so
        // the marker would be left alone and the text pushed down.
        assert_eq!(assert_widths("> abcdef", 4), vec!["> ", "abcd", "ef"]);
        let hung = wrap_line_hanging(&line, 4, 2);
        let rows: Vec<String> = hung.iter().map(text).collect();
        assert_eq!(rows, vec!["> ab", "cdef"], "the marker keeps its first word");
        assert!(!rows[0].trim_end().eq(">"), "never the marker alone");
    }

    #[test]
    fn blank_and_short_lines_pass_through() {
        let lines = vec![Line::from("short"), Line::from(""), Line::from("x")];
        let wrapped = wrap_lines(&lines, 10);
        assert_eq!(wrapped.len(), 3);
        assert_eq!(text(&wrapped[1]), "");
    }

    #[test]
    fn zero_width_is_not_a_panic() {
        let line = Line::from("anything");
        assert_eq!(wrap_line(&line, 0).len(), 1);
    }

    #[test]
    fn styles_are_preserved_across_the_wrap_point() {
        let line = Line::from(vec![
            Span::styled("red", Style::default().fg(Color::Red)),
            Span::styled("green", Style::default().fg(Color::Green)),
        ]);
        let wrapped = wrap_line(&line, 5);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(text(&wrapped[0]), "redgr");
        assert_eq!(wrapped[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(wrapped[1].spans[0].style.fg, Some(Color::Green));
    }
}
