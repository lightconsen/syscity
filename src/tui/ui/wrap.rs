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

/// A run of URL characters bound to a wrapped row, in *columns* relative to
/// that row's first cell.
///
/// Only pure ASCII `https?://` URLs are recognized (`detect_links`), so every
/// character of a run is one column wide and `start..start+len` maps onto
/// consecutive cells without any wide-character or continuation paperwork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRun {
    /// Display column (within the row) where the link starts.
    pub start: usize,
    /// Number of columns — and characters — the link occupies.
    pub len: usize,
    /// The URL the segment should open.
    pub url: String,
}

/// The URL schemes we turn into hyperlinks.
fn url_schemes() -> &'static [&'static [u8]] {
    &[b"http://", b"https://"]
}

/// Characters a URL may keep on running through; everything else, notably
/// whitespace, quotes and angle brackets, ends it.
fn is_url_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b':'
                | b'/'
                | b'?'
                | b'#'
                | b'['
                | b']'
                | b'@'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b'%'
        )
}

/// Find ASCII `http(s)://` URLs in `text`, as spans of source *characters*.
///
/// The scan walks bytes — the URL itself is all ASCII — but the run's
/// `start`/`len` are character offsets, so a wide character before the link
/// cannot shift the mapping onto the wrong cells (`wrap_rows` records a
/// character index per cell). The run ends the moment a non-URL byte shows
/// up; nothing is stripped, so the wrapped line keeps exactly its source
/// characters (width-neutrality holds).
fn detect_links(text: &str) -> Vec<LinkRun> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let scheme_len = url_schemes()
            .iter()
            .find_map(|s| rest.starts_with(s).then_some(s.len()));
        match scheme_len {
            Some(n) => {
                let mut end = i + n;
                while end < bytes.len() && is_url_byte(bytes[end]) {
                    end += 1;
                }
                if end > i + n {
                    let url = &text[i..end];
                    out.push(LinkRun {
                        start: text[..i].chars().count(),
                        len: url.chars().count(),
                        url: url.to_string(),
                    });
                    i = end;
                } else {
                    i += n;
                }
            }
            None => i += 1,
        }
    }
    out
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

/// Split `line` into rows and record, for each cell, which source character
/// it came from. The source index lets a link found in the flat text be
/// located inside any row: cells keep their order, so each row covers a
/// contiguous slice of the characters.
fn wrap_rows(line: &Line<'_>, width: usize, hang: usize) -> (Vec<Vec<Cell>>, Vec<Vec<usize>>) {
    let chars = cells(line);
    if width == 0 {
        let srcs: Vec<usize> = (0..chars.len()).collect();
        return (vec![chars], vec![srcs]);
    }

    let width_of_run = |run: &[Cell]| -> usize { run.iter().map(|(c, _)| width_of(*c)).sum() };

    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut srcs: Vec<Vec<usize>> = Vec::new();
    let mut row: Vec<Cell> = Vec::new();
    let mut row_src: Vec<usize> = Vec::new();
    let mut used = 0usize;
    // Index within `row` just past the most recent space — the preferred break.
    let mut last_break: Option<usize> = None;

    for (src, (ch, style)) in chars.into_iter().enumerate() {
        let w = width_of(ch);
        // Break *before* adding the character, as many times as it takes: a
        // double-width character that does not fit the last column gets its
        // own row rather than overflowing.
        while used + w > width && !row.is_empty() {
            match last_break {
                // Break after the space: the space stays on the row it ended.
                Some(at) => {
                    let rest = row.split_off(at);
                    let rest_src = row_src.split_off(at);
                    rows.push(std::mem::take(&mut row));
                    srcs.push(std::mem::take(&mut row_src));
                    used = width_of_run(&rest);
                    row = rest;
                    row_src = rest_src;
                }
                None => {
                    rows.push(std::mem::take(&mut row));
                    srcs.push(std::mem::take(&mut row_src));
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
        row_src.push(src);
    }
    rows.push(row);
    srcs.push(row_src);

    (rows, srcs)
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
    wrap_rows(line, width, hang)
        .0
        .iter()
        .map(|r| line_of(r))
        .collect()
}

/// Wrap a batch of lines, preserving blank lines.
pub fn wrap_lines(lines: &[Line<'_>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for line in lines {
        out.extend(wrap_line(line, width));
    }
    out
}

/// Wrap one line, carrying each URL run onto the wrapped row it lands on.
///
/// The caller (the scrollback writer) renders the rows and wants each link
/// reachable: this hands back, per row, the runs in that row's own column
/// space, so the writer can wrap the row's cells in the OSC 8 escape pair.
/// A URL split across two rows becomes one run on each, each covering the
/// characters that row holds — a terminal lets both halves open.
pub fn wrap_line_links(line: &Line<'_>, width: usize) -> (Vec<Line<'static>>, Vec<Vec<LinkRun>>) {
    let (rows, srcs) = wrap_rows(line, width, 0);
    let text: String = cells(line).iter().map(|(c, _)| c).collect();
    let runs = detect_links(&text);
    let per_row = rows
        .iter()
        .zip(&srcs)
        .map(|(row, src)| row_links(row, src, &runs))
        .collect();
    let lines: Vec<Line<'static>> = rows.iter().map(|r| line_of(r)).collect();
    (lines, per_row)
}

/// Map source-position link runs onto one row, merging contiguous cells of
/// one URL into a single run whose `start` is the row-relative column.
fn row_links(row: &[Cell], srcs: &[usize], runs: &[LinkRun]) -> Vec<LinkRun> {
    let mut out: Vec<LinkRun> = Vec::new();
    let mut col = 0usize;
    for ((c, _), &src) in row.iter().zip(srcs) {
        if let Some(run) = runs
            .iter()
            .find(|r| src >= r.start && src < r.start + r.len)
        {
            match out.last_mut() {
                Some(seg) if seg.url == run.url && seg.start + seg.len == col => seg.len += 1,
                _ => out.push(LinkRun {
                    start: col,
                    len: 1,
                    url: run.url.clone(),
                }),
            }
        }
        col += width_of(*c);
    }
    out
}

/// Wrap a batch of lines, carrying links, preserving blank lines.
pub fn wrap_lines_links(
    lines: &[Line<'_>],
    width: usize,
) -> (Vec<Line<'static>>, Vec<Vec<LinkRun>>) {
    let mut out_lines = Vec::new();
    let mut out_runs = Vec::new();
    for line in lines {
        let (rows, runs) = wrap_line_links(line, width);
        out_lines.extend(rows);
        out_runs.extend(runs);
    }
    (out_lines, out_runs)
}

/// Pad a string to a display width with trailing spaces (`unicode-width`).
///
/// A cell whose display width already meets `width` is returned unchanged;
/// a wider-than-`width` string is left alone rather than truncated. Used to
/// right-align table columns on the display grid, where a hanzi takes two
/// cells but one byte boundary looks like a column of its own.
pub fn pad_to_width(s: &str, width: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    if w >= width {
        return s.to_string();
    }
    let mut out = s.to_string();
    out.push_str(&" ".repeat(width - w));
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
    #[test]
    fn pad_to_width_measures_display_not_bytes() {
        // A single hanzi is two columns; padding it toward a three-column
        // target adds one display column'of space, not bytes.
        let padded = pad_to_width("名", 3);
        assert_eq!(UnicodeWidthStr::width(padded.as_str()), 3);
        assert_eq!(padded, "名 ");
        // At or past the target nothing is truncated, nothing added.
        assert_eq!(pad_to_width("名字", 3), "名字");
        assert_eq!(pad_to_width("很长的单元", 2), "很长的单元");
    }

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

    fn all_text(rows: &[Line<'_>]) -> String {
        rows.iter().map(text).collect()
    }

    /// The cells of a row, one display column apart, URL-marked where a run
    /// points — the property `scrollback` relies on when it builds the
    /// OSC 8 escape pair around each cell.
    #[test]
    fn links_map_to_row_columns_and_survive_wrapping() {
        let line = Line::from("see https://example.com/x now");
        // 12 columns wrap the URL before it ends; the run must reappear on
        // both rows, each holding its own slice.
        let (rows, runs) = wrap_line_links(&line, 12);
        let joined: Vec<Vec<String>> = runs
            .iter()
            .map(|r| r.iter().map(|l| l.url.clone()).collect())
            .collect();
        assert_eq!(all_text(&rows).matches("https://example.com/x").count(), 1);
        let total: usize = runs.iter().flatten().map(|r| r.len).sum();
        assert_eq!(total, "https://example.com/x".len(), "no link character is lost");
        assert!(joined
            .iter()
            .flatten()
            .all(|u| u.as_str() == "https://example.com/x"));
        // Columns are row-relative and never exceed the row's width.
        for run in runs.iter().flatten() {
            assert!(run.start + run.len <= 12, "run {run:?} overflows its row");
        }
    }

    #[test]
    fn a_url_before_a_wide_character_starts_at_the_right_column() {
        // `中` takes two columns, so the link's first cell sits after it —
        // the run must start at column 3, not be misaligned by the wide char.
        let line = Line::from("x中 https://example.com y");
        let (rows, runs) = wrap_line_links(&line, 40);
        assert_eq!(rows.len(), 1);
        let run = runs[0].first().expect("the link");
        assert_eq!(run.url, "https://example.com");
        // "x中 " is 1 + 2 + 1 = 4 columns.
        assert_eq!(run.start, 4);
    }

    #[test]
    fn non_urls_are_left_alone() {
        assert!(detect_links("no links here").is_empty());
        assert!(detect_links("a https:// bare scheme has no target").is_empty());
        assert!(detect_links("https://").is_empty());
        // A URL stops at whitespace and quotes, keeping only its own bytes.
        let runs = detect_links("see https://example.com, ok");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].url, "https://example.com,");
        assert_eq!(runs[0].len, runs[0].url.len());
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
