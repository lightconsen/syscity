//! Turning transcript entries into renderable lines.
//!
//! Two directions live here: `TranscriptLine` → styled [`Line`] for rendering,
//! and gateway history → `TranscriptLine` so a resumed session's past reads
//! like the conversation that produced it.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::tui::gateway_calls::HistoryMessage;
use crate::tui::transcript::{LineKind, TranscriptLine};
use crate::tui::ui::{wrap, Theme};

/// The style a transcript line kind renders with.
pub fn kind_style(theme: &Theme, kind: LineKind) -> Style {
    match kind {
        LineKind::User => theme.user_style(),
        LineKind::Assistant => theme.assistant_style(),
        LineKind::Reasoning => theme.reasoning_style(),
        LineKind::Tool => theme.tool_call_style(),
        LineKind::ToolResult => theme.tool_call_style().add_modifier(Modifier::DIM),
        LineKind::Code => theme.code_style(),
        LineKind::Blockquote => theme.dim_style(),
        LineKind::ListItem => theme.assistant_style(),
        LineKind::Heading => theme.assistant_style(),
        LineKind::Notice => theme.system_style(),
        LineKind::Separator => Style::default(),
    }
}

/// Render one transcript line.
pub fn to_line(entry: &TranscriptLine, theme: &Theme) -> Line<'static> {
    let style = kind_style(theme, entry.kind);
    let spans = match entry.kind {
        // A block quote draws its own dim `│` gutter ahead of the prose.
        LineKind::Blockquote => vec![
            Span::styled("│ ", theme.dim_style()),
            Span::styled(entry.text.clone(), theme.assistant_style()),
        ],
        // A list item renders its marker accented and its text as prose —
        // bullets become `•`, ordered numbers are kept as typed. The text
        // after the marker still gets backtick spans.
        LineKind::ListItem => {
            let style = theme.assistant_style();
            let code = style.fg(theme.accent);
            match list_marker_len(&entry.text) {
                Some(prefix) => {
                    let (head, rest) = entry.text.split_at(prefix);
                    let lead = head.len() - head.trim_start().len();
                    let indent = &head[..lead];
                    let marker = head[lead..].trim_end();
                    let glyph = if marker.chars().count() == 1 {
                        "•"
                    } else {
                        marker
                    };
                    let mut spans = Vec::new();
                    if !indent.is_empty() {
                        spans.push(Span::styled(indent.to_string(), style));
                    }
                    spans.push(Span::styled(format!("{glyph} "), code));
                    spans.extend(inline_code_spans(rest, style, code));
                    spans
                }
                None => vec![Span::styled(entry.text.clone(), style)],
            }
        }
        // A heading renders as its title — the `#` marker is dropped, the
        // text goes bold (h1 underlined) — with backtick spans preserved.
        LineKind::Heading => {
            let trimmed = entry.text.trim_start();
            let level = heading_level(trimmed).unwrap_or(1);
            let title = &trimmed[level..];
            let mut heading = theme.assistant_style().add_modifier(Modifier::BOLD);
            if level == 1 {
                heading = heading.add_modifier(Modifier::UNDERLINED);
            }
            // The code span derives from the heading style so a heading keeps
            // one weight throughout; only the foreground flips to the accent.
            let code = heading.fg(theme.accent);
            inline_code_spans(title.trim_start(), heading, code)
        }
        // Prose lines get backtick spans. A fenced block is already tagged
        // `LineKind::Code` and must not be re-tokenized.
        LineKind::Assistant | LineKind::User => {
            inline_code_spans(&entry.text, style, style.fg(theme.accent))
        }
        _ => vec![Span::styled(entry.text.clone(), style)],
    };
    Line::from(spans)
}

/// Split prose on backtick pairs so `` `code` `` renders as inline code.
///
/// Width-neutral by design: every source character survives, styled or not,
/// because the scrollback's wrap alignment is measured from the line's width.
/// The code span inherits the line's style and overrides the foreground with
/// the accent — so a code span inside a user turn keeps that turn's
/// background pill. A lone backtick with no closer, or an empty pair, is
/// ordinary text.
fn inline_code_spans(text: &str, plain: Style, code: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            // Find the matching closer. The backticks themselves are ordinary
            // text before and after the code span — only the content between
            // them is accented, and every source character survives.
            if let Some(j) = (i + 1..chars.len()).find(|&j| chars[j] == '`') {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), plain));
                }
                buf.push('`');
                spans.push(Span::styled(std::mem::take(&mut buf), plain));
                let code_text: String = chars[i + 1..j].iter().collect();
                if !code_text.is_empty() {
                    spans.push(Span::styled(code_text, code));
                }
                // The closing backtick stays in the buffer, joining whatever
                // follows into the next plain run.
                buf.push('`');
                i = j + 1;
                continue;
            }
            // No closing backtick: ordinary text.
            buf.push('`');
        } else {
            buf.push(chars[i]);
        }
        i += 1;
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, plain));
    }
    spans
}

/// Render a batch of transcript lines.
pub fn to_lines(entries: &[TranscriptLine], theme: &Theme) -> Vec<Line<'static>> {
    entries.iter().map(|e| to_line(e, theme)).collect()
}

/// Argument rows the *live* preview may show.
///
/// The live region is `LIVE_HEIGHT` rows minus the status row and the
/// composer, so at most six — and the stream preview competes for the same
/// space. The cap is five so that the `⚙ tool` header fits the region too:
/// six argument rows plus the header is seven, and the seventh would be the
/// line the preview drops.
pub const TOOL_ARG_LINES_LIVE: usize = 5;

/// Argument rows a reprint from history may show.
///
/// A reprint has the whole screen to itself and no live text to crowd out,
/// so it can afford more than the preview.
pub const TOOL_ARG_LINES_HISTORY: usize = 8;

/// Pretty-print tool arguments, trimmed to a few lines so one enormous call
/// cannot take over the screen.
///
/// One string per row: the caller turns each into its own transcript line,
/// because the cell renderer drops a `\n` inside a line and a packed value
/// would come out as a single run-together row.
pub fn args_lines(args: &Value, indent: &str, max_rows: usize) -> Vec<String> {
    let text = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
    let all: Vec<&str> = text.lines().collect();
    // At most `max_rows` rows: the ellipsis is the last *of* them, not an
    // extra one, so a caller can size the region to the cap and be sure
    // nothing is dropped off the end.
    if all.len() <= max_rows {
        return all.into_iter().map(|l| format!("{indent}{l}")).collect();
    }
    let mut rows: Vec<String> = all
        .iter()
        .take(max_rows.saturating_sub(1))
        .map(|l| format!("{indent}{l}"))
        .collect();
    rows.push(format!("{indent}…"));
    rows
}

/// Result rows the live preview may show, for the same reason as
/// [`TOOL_ARG_LINES_LIVE`].
pub const TOOL_RESULT_LINES_LIVE: usize = 6;

/// Rows for a tool result: the first carries the `↳ tool:` marker, the rest
/// are indented under it. One string per row — a packed value loses its
/// newlines at render time and comes out as one run-together row.
pub fn result_lines(tool: &str, result: Option<&Value>) -> Vec<String> {
    let Some(result) = result else {
        return vec![format!("  ↳ {tool}: done")];
    };
    let rows = args_lines(result, "  ", TOOL_RESULT_LINES_LIVE);
    rows.into_iter()
        .enumerate()
        .map(|(i, row)| {
            if i == 0 {
                format!("  ↳ {tool}: {}", row.trim_start())
            } else {
                row
            }
        })
        .collect()
}

/// Pull the arguments out of a tool call, whichever way the gateway encoded
/// them (an embedded JSON string, an object, or a sibling `arguments` field).
fn tool_call_args(call: &Value) -> Option<Value> {
    let raw = call["function"]["arguments"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .or_else(|| {
            call["function"]["arguments"]
                .as_object()
                .map(|o| Value::Object(o.clone()))
        })
        .or_else(|| call.get("arguments").filter(|v| !v.is_null()).cloned())?;
    if raw.is_null() {
        None
    } else {
        Some(raw)
    }
}

/// Turn one history message into transcript lines.
///
/// This is the shape `chat.history` serves: `content` for prose,
/// `reasoning_content` for the model's thinking, and `tool_calls` as raw JSON.
pub fn history_message_lines(msg: &HistoryMessage) -> Vec<TranscriptLine> {
    let mut out = Vec::new();
    match msg.role.as_str() {
        "user" => {
            for line in msg.content.split('\n') {
                out.push(TranscriptLine::new(LineKind::User, format!("> {line}")));
            }
        }
        "assistant" => {
            if let Some(reasoning) = msg.reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
                out.push(TranscriptLine::new(LineKind::Reasoning, "thinking:"));
                for line in reasoning.lines() {
                    out.push(TranscriptLine::new(LineKind::Reasoning, format!("  {line}")));
                }
            }
            out.extend(text_lines(&msg.content));
        }
        "tool" => {
            for line in msg.content.split('\n') {
                out.push(TranscriptLine::new(LineKind::ToolResult, format!("  {line}")));
            }
        }
        _ => {
            for line in msg.content.split('\n') {
                out.push(TranscriptLine::new(LineKind::Notice, line));
            }
        }
    }

    if let Some(calls) = msg.tool_calls.as_ref().and_then(|v| v.as_array()) {
        for call in calls {
            let name = call["function"]["name"]
                .as_str()
                .or_else(|| call["name"].as_str())
                .unwrap_or("tool");
            out.push(TranscriptLine::new(LineKind::Tool, format!("⚙ {name}")));
            if let Some(args) = tool_call_args(call) {
                for line in args_lines(&args, "  ", TOOL_ARG_LINES_HISTORY) {
                    out.push(TranscriptLine::new(LineKind::Tool, line));
                }
            }
        }
    }
    out
}

/// Prose → transcript lines, tagging fenced regions as code and aligning
/// consecutive pipe rows as a table.
pub fn text_lines(text: &str) -> Vec<TranscriptLine> {
    if text.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();

    let mut out = Vec::new();
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push(TranscriptLine::new(LineKind::Code, line.to_string()));
            i += 1;
            continue;
        }
        if in_fence {
            out.push(TranscriptLine::new(LineKind::Code, line.to_string()));
            i += 1;
            continue;
        }
        // At least two consecutive pipe rows are a table, not prose.
        if let Some((rows, consumed)) = table_block(&lines[i..]) {
            out.extend(table_lines(&rows));
            i += consumed;
            continue;
        }
        // A `>`-leading line is a block quote: the marker is dropped and the
        // renderer draws its own gutter.
        if let Some(body) = line.trim_start().strip_prefix('>') {
            out.push(TranscriptLine::new(LineKind::Blockquote, body.trim_start().to_string()));
            i += 1;
            continue;
        }
        // A markdown list item: the renderer accents the marker, bullets as
        // `•`, numbers as typed.
        if list_marker_len(line).is_some() {
            out.push(TranscriptLine::new(LineKind::ListItem, line.to_string()));
            i += 1;
            continue;
        }
        // An ATX heading renders as its bold title; the `#` marker is dropped.
        if heading_level(line).is_some() {
            out.push(TranscriptLine::new(LineKind::Heading, line.to_string()));
            i += 1;
            continue;
        }
        out.push(TranscriptLine::new(LineKind::Assistant, line.to_string()));
        i += 1;
    }
    out
}

/// A row that could belong to a table: a trimmed line opening with `|` and
/// holding at least one more. A bare `|` slug in prose is not one.
fn is_pipe_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.matches('|').count() >= 2
}

/// The byte length of a markdown list marker at the line's start, if the line
/// is a list item: a `-`/`*`/`+` bullet or up to nine digits closed by `.`
/// or `)`, each followed by whitespace. The whitespace requirement keeps
/// emphasis (`*x*`), decimals (`1.5x`) and horizontal rules (`---`) prose.
fn list_marker_len(text: &str) -> Option<usize> {
    let trimmed = text.trim_start();
    let indent = text.len() - trimmed.len();
    let bytes = trimmed.as_bytes();
    let marker = match bytes.first()? {
        b'-' | b'*' | b'+' => 1usize,
        b if b.is_ascii_digit() => {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            match bytes.get(digits) {
                Some(b'.') | Some(b')') if digits <= 9 => digits + 1,
                _ => return None,
            }
        }
        _ => return None,
    };
    match bytes.get(marker) {
        Some(b) if b.is_ascii_whitespace() => Some(indent + marker + 1),
        _ => None,
    }
}

/// The level of an ATX heading at the line's start: a run of one to six `#`
/// followed by whitespace. A run without a following space (`#hashtag`) or
/// longer than six (`#######` is not a heading in markdown) stays prose.
fn heading_level(text: &str) -> Option<usize> {
    let trimmed = text.trim_start();
    let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    match trimmed.as_bytes().get(hashes) {
        Some(b' ') | Some(b'\t') => Some(hashes),
        _ => None,
    }
}

/// The maximal run of pipe rows starting at the front of `lines`, when there
/// are at least two — a single pipe row is ordinary text.
fn table_block<'a>(lines: &[&'a str]) -> Option<(Vec<&'a str>, usize)> {
    let count = lines.iter().take_while(|l| is_pipe_row(l)).count();
    if count < 2 {
        return None;
    }
    Some((lines[..count].to_vec(), count))
}

/// A table separaator cell: `---`, `:--:`, `-:`, with optional colons.
fn is_separator_cell(cell: &str) -> bool {
    let c = cell.trim().trim_start_matches(':').trim_end_matches(':');
    !c.is_empty() && c.bytes().all(|b| b == b'-')
}

/// Align the rows of a markdown table.
///
/// Every column is padded to its widest cell's display width (`unicode-width`,
/// so a hanzi's two columns are counted, not its bytes), and each row is
/// rebuilt with the shared column boundaries. Separator rows (`---`) render
/// dim, data rows as ordinary prose.
fn table_lines(rows: &[&str]) -> Vec<TranscriptLine> {
    let cells: Vec<Vec<&str>> = rows
        .iter()
        .map(|row| {
            row.trim()
                .trim_matches('|')
                .split('|')
                .map(|c| c.trim())
                .collect()
        })
        .collect();
    let columns = cells.iter().map(|r| r.len()).max().unwrap_or(1).max(1);
    // The widest display width per column; short or ragged rows are padded.
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            cells
                .iter()
                .filter_map(|r| r.get(c))
                .map(|s| UnicodeWidthStr::width(*s))
                .max()
                .unwrap_or(0)
        })
        .collect();

    cells
        .iter()
        .map(|row| {
            // A row is a separator when every cell is one — and no fewer:
            // `| a | - |` is a data row that happens to mention a dash.
            let is_separator = row.iter().all(|c| is_separator_cell(c));
            let rendered: Vec<String> = (0..columns)
                .map(|c| match row.get(c) {
                    Some(cell) => wrap::pad_to_width(cell, widths[c]),
                    None => " ".repeat(widths[c]),
                })
                .collect();
            let kind = if is_separator {
                LineKind::Notice
            } else {
                LineKind::Assistant
            };
            let text = format!("| {} |", rendered.join(" | "));
            TranscriptLine::new(kind, text)
        })
        .collect()
}

/// Turn a loaded history into one block of transcript lines.
pub fn history_lines(messages: &[HistoryMessage]) -> Vec<TranscriptLine> {
    let mut out = Vec::new();
    for msg in messages {
        out.extend(history_message_lines(msg));
        out.push(TranscriptLine::separator());
    }
    out
}

/// A separator rule line, used to mark a session switch in the scrollback.
pub fn rule(text: &str) -> TranscriptLine {
    let label = if text.is_empty() {
        "─".repeat(20)
    } else {
        format!("── {text} {}", "─".repeat(12))
    };
    TranscriptLine::new(LineKind::Notice, label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use unicode_width::UnicodeWidthChar;

    fn texts(entries: &[TranscriptLine]) -> Vec<String> {
        entries.iter().map(|e| e.text.clone()).collect()
    }

    #[test]
    fn empty_content_produces_no_lines() {
        assert!(text_lines("").is_empty());
    }

    #[test]
    fn user_messages_are_echoed_with_a_prompt_marker() {
        let msg = HistoryMessage {
            role: "user".into(),
            content: "hello\nthere".into(),
            ..Default::default()
        };
        assert_eq!(texts(&history_message_lines(&msg)), vec!["> hello", "> there"]);
    }

    #[test]
    fn reasoning_precedes_the_answer_and_is_indented() {
        let msg = HistoryMessage {
            role: "assistant".into(),
            content: "answer".into(),
            reasoning: Some("weighing options".into()),
            ..Default::default()
        };
        let lines = history_message_lines(&msg);
        assert_eq!(lines[0].kind, LineKind::Reasoning);
        assert_eq!(lines[0].text, "thinking:");
        assert_eq!(lines[1].text, "  weighing options");
        assert_eq!(lines[2].kind, LineKind::Assistant);
        assert_eq!(lines[2].text, "answer");
    }

    #[test]
    fn fenced_blocks_are_tagged_as_code() {
        let msg = HistoryMessage {
            role: "assistant".into(),
            content: "before\n```rust\nlet x = 1;\n```\nafter".into(),
            ..Default::default()
        };
        let lines = history_message_lines(&msg);
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::Assistant,
                LineKind::Code,
                LineKind::Code,
                LineKind::Code,
                LineKind::Assistant
            ]
        );
    }

    #[test]
    fn tool_calls_render_their_arguments() {
        let msg = HistoryMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: Some(json!([
                { "id": "c1", "function": { "name": "file_read", "arguments": { "path": "/tmp/x" } } }
            ])),
            ..Default::default()
        };
        let lines = history_message_lines(&msg);
        assert_eq!(lines[0].kind, LineKind::Tool);
        assert_eq!(lines[0].text, "⚙ file_read");
        assert!(lines.iter().any(|l| l.text.contains("/tmp/x")));
    }

    #[test]
    fn tool_arguments_given_as_a_json_string_are_parsed() {
        let msg = HistoryMessage {
            role: "assistant".into(),
            tool_calls: Some(json!([
                { "function": { "name": "shell", "arguments": "{\"cmd\":\"ls\"}" } }
            ])),
            ..Default::default()
        };
        let lines = history_message_lines(&msg);
        assert!(lines.iter().any(|l| l.text.contains("cmd")));
    }

    /// The cap is a row budget, not "cap plus an ellipsis": the preview it
    /// feeds is a fixed height, and one extra row is one row dropped.
    #[test]
    fn the_row_cap_includes_the_ellipsis() {
        let big: Vec<usize> = (0..50).collect();
        let args = json!({ "items": big });
        for cap in [1usize, 3, 5, 8] {
            let rows = args_lines(&args, "  ", cap);
            assert!(rows.len() <= cap, "cap {cap} gave {} rows", rows.len());
        }
        // `{\n  "path": "/tmp/x"\n}` — three rows, nothing to elide.
        let short = json!({ "path": "/tmp/x" });
        let rows = args_lines(&short, "  ", 8);
        assert_eq!(rows.len(), 3, "got {rows:?}");
        assert!(!rows.iter().any(|r| r.contains('…')));
    }

    /// Results split the same way, with the marker on the first row only.
    #[test]
    fn a_result_puts_its_marker_on_the_first_row() {
        let result = json!({ "ok": true, "count": 3 });
        let rows = result_lines("file_read", Some(&result));
        assert!(rows[0].starts_with("  ↳ file_read: "), "got {:?}", rows[0]);
        assert!(rows.len() > 1, "the rows follow: {rows:?}");
        assert!(rows[1..].iter().all(|r| !r.contains('↳')), "the marker appears once: {rows:?}");
        assert_eq!(result_lines("t", None), vec!["  ↳ t: done"]);
    }

    #[test]
    fn huge_arguments_are_trimmed_to_a_handful_of_lines() {
        // A genuinely tall payload: pretty-printing a long array spans lines.
        let big: Vec<usize> = (0..50).collect();
        let msg = HistoryMessage {
            role: "assistant".into(),
            tool_calls: Some(json!([
                { "function": { "name": "shell", "arguments": { "items": big } } }
            ])),
            ..Default::default()
        };
        let lines = history_message_lines(&msg);
        assert!(lines.len() < 20, "args must be capped, got {}", lines.len());
        assert!(lines.iter().any(|l| l.text.trim_end().ends_with('…')));
    }

    #[test]
    fn inline_backticks_are_accented_and_every_character_survives() {
        let theme = Theme::dark();
        let text = "run `cargo test` then `cargo fmt`";
        let line = to_line(&TranscriptLine::new(LineKind::Assistant, text.to_string()), &theme);
        // Every source character survives, in order, across the spans: the
        // decoration is style-only and must not shift the wrapped width.
        let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, text);
        let code_spans: Vec<_> = line
            .spans
            .iter()
            .filter(|s| s.content == "cargo test" || s.content == "cargo fmt")
            .collect();
        assert_eq!(code_spans.len(), 2);
        for s in code_spans {
            assert_eq!(s.style.fg, Some(theme.accent));
        }
    }

    #[test]
    fn a_lone_backtick_and_an_empty_pair_stay_plain() {
        let theme = Theme::dark();
        let line =
            to_line(&TranscriptLine::new(LineKind::Assistant, "not `closed".to_string()), &theme);
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "not `closed");
        // `` with nothing between keeps both characters, unaccented.
        let empty = to_line(&TranscriptLine::new(LineKind::Assistant, "a``b".to_string()), &theme);
        let joined: String = empty.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "a``b");
        assert!(empty.spans.iter().all(|s| s.style.fg != Some(theme.accent)));
    }

    #[test]
    fn code_spans_in_user_lines_keep_the_users_background() {
        let theme = Theme::dark();
        let line =
            to_line(&TranscriptLine::new(LineKind::User, "use `Command::new`".to_string()), &theme);
        let code = line
            .spans
            .iter()
            .find(|s| s.content == "Command::new")
            .expect("the code span");
        assert_eq!(code.style.fg, Some(theme.accent));
        assert_eq!(code.style.bg, Some(theme.user_bg));
    }

    #[test]
    fn fenced_blocks_are_not_re_tokenized() {
        let theme = Theme::dark();
        let line =
            to_line(&TranscriptLine::new(LineKind::Code, "let `x = 1;`".to_string()), &theme);
        assert_eq!(line.spans.len(), 1, "a code fence renders as one span");
    }

    #[test]
    fn table_rows_align_columns_including_wide_characters() {
        let text = "| 名字 | 大小 |\n| --- | ---: |\n| 中文 | 十二 |";
        let lines = text_lines(text);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].kind, LineKind::Assistant);
        assert_eq!(lines[1].kind, LineKind::Notice, "the --- row is dim");
        assert_eq!(lines[2].kind, LineKind::Assistant);
        // Every row agrees on where each `|` boundary lands in *display columns*:
        // a hanzi is two columns, so pad_to_width must count columns, not
        // characters or bytes, for the boundaries to line up.
        let pipes: Vec<Vec<usize>> = lines
            .iter()
            .map(|l| {
                let (mut cols, mut col) = (Vec::<usize>::new(), 0usize);
                for ch in l.text.chars() {
                    if ch == '|' {
                        cols.push(col);
                    }
                    col += UnicodeWidthChar::width(ch).unwrap_or(0);
                }
                cols
            })
            .collect();
        assert_eq!(pipes[1], pipes[0], "the separator row aligns");
        assert_eq!(pipes[2], pipes[0], "a wide cell must not shift the column");
    }

    #[test]
    fn a_lone_pipe_row_is_prose_not_a_table() {
        let lines = text_lines("| just one row |\nand prose after");
        assert_eq!(lines.len(), 2);
        assert!(
            lines.iter().all(|l| l.kind != LineKind::Notice),
            "no table, no dim separator: {lines:?}"
        );
    }

    #[test]
    fn a_separator_row_after_a_prose_line_is_not_a_table() {
        // The pipe run starts *after* the prose line; only consecutive rows
        // form a table.
        let lines = text_lines("plain text\n| --- |\n| x |");
        assert_eq!(lines[0].kind, LineKind::Assistant);
        assert_eq!(lines[0].text, "plain text");
        // `| --- |` + `| x |` is two rows, so it *is* a table.
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].kind, LineKind::Notice);
        assert_eq!(lines[2].kind, LineKind::Assistant);
    }

    #[test]
    fn ragged_rows_are_padded_not_lost() {
        let lines = text_lines("| a | b |\n| only |");
        assert_eq!(lines.len(), 2);
        // The second row still renders both columns, the missing one padded.
        assert!(lines[1].text.contains("| only |"), "got {}", lines[1].text);
        let c: Vec<char> = lines[1].text.chars().collect();
        assert!(c.len() >= lines[0].text.chars().count());
    }

    #[test]
    fn blockquote_lines_strip_the_marker_and_draw_a_gutter() {
        let theme = Theme::dark();
        let lines = text_lines("before\n> a quoted line\n>also-quoted\nafter");
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].kind, LineKind::Assistant);
        // The `>` is dropped — the renderer supplies its own gutter.
        assert_eq!(lines[1].kind, LineKind::Blockquote);
        assert_eq!(lines[1].text, "a quoted line");
        assert_eq!(lines[2].kind, LineKind::Blockquote);
        assert_eq!(lines[2].text, "also-quoted");
        assert_eq!(lines[3].kind, LineKind::Assistant);

        let rendered = to_line(&lines[1], &theme);
        assert_eq!(rendered.spans.len(), 2);
        assert_eq!(rendered.spans[0].content.as_ref(), "│ ");
        assert_eq!(rendered.spans[0].style.fg, Some(theme.dim));
        assert_eq!(rendered.spans[1].content.as_ref(), "a quoted line");
        assert_eq!(rendered.spans[1].style.fg, Some(theme.text));
    }

    #[test]
    fn history_puts_a_blank_line_between_messages() {
        let messages = vec![
            HistoryMessage {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            },
            HistoryMessage {
                role: "assistant".into(),
                content: "yo".into(),
                ..Default::default()
            },
        ];
        let lines = history_lines(&messages);
        assert_eq!(lines.last().map(|l| l.kind), Some(LineKind::Separator));
        assert!(
            lines
                .iter()
                .filter(|l| l.kind == LineKind::Separator)
                .count()
                == 2
        );
    }

    #[test]
    fn list_items_are_tagged_from_bullets_and_numbers() {
        let lines = text_lines("- first\n* second\n+ third\n1. one\n2) two\n  - nested");
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![LineKind::ListItem; 6],
            "every bullet and number row is a list item: {kinds:?}"
        );
        // The text is kept verbatim; only rendering restyles it.
        assert_eq!(lines[0].text, "- first");
        assert_eq!(lines[5].text, "  - nested");
    }

    #[test]
    fn lookalikes_stay_prose() {
        let lines = text_lines("*emphasis* is not a list\n1.5x is not either\n---\n-\n> - quoted");
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::Assistant,
                LineKind::Assistant,
                LineKind::Assistant,
                LineKind::Assistant,
                LineKind::Blockquote
            ],
            "emphasis, decimals, rules and bare dashes are not list items: {kinds:?}"
        );
    }

    #[test]
    fn a_list_item_renders_an_accented_marker_and_prose_text() {
        let theme = Theme::dark();
        let lines = text_lines("- plain `code` tail\n12. ordered\n  - nested");

        // Every source character survives (the bullet is restyled, not
        // deleted), the glyph is accented, and the tail's backticks still
        // tokenize as inline code.
        let bullet = to_line(&lines[0], &theme);
        let text: String = bullet.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "• plain `code` tail");
        let accented: Vec<&str> = bullet
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(theme.accent))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(accented, vec!["• ", "code"], "got {bullet:?}");

        // Ordered numbers are kept as typed, not renumbered.
        let ordered = to_line(&lines[1], &theme);
        let text: String = ordered.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "12. ordered");
        assert_eq!(ordered.spans[0].content.as_ref(), "12. ");
        assert_eq!(ordered.spans[0].style.fg, Some(theme.accent));

        // Nesting survives as the source indent, rendered plain.
        let nested = to_line(&lines[2], &theme);
        assert_eq!(nested.spans[0].content.as_ref(), "  ");
        assert_eq!(nested.spans[1].content.as_ref(), "• ");
    }

    #[test]
    fn fences_win_over_list_detection() {
        let lines = text_lines("```\n- not a list\n```");
        assert!(
            lines.iter().all(|l| l.kind == LineKind::Code),
            "a dash inside a fence is code, got {lines:?}"
        );
    }

    #[test]
    fn headings_are_tagged_from_one_to_six_hashes() {
        let lines =
            text_lines("# h1\n## h2\n###### h6\n#hashtag stays prose\n####### not a heading");
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::Heading,
                LineKind::Heading,
                LineKind::Heading,
                LineKind::Assistant,
                LineKind::Assistant
            ],
            "one to six hashes are headings, `#tag` and seven are not: {kinds:?}"
        );
        // The text is kept verbatim; only rendering restyles it.
        assert_eq!(lines[2].text, "###### h6");
    }

    #[test]
    fn a_heading_renders_as_a_bold_title_without_the_marker() {
        let theme = Theme::dark();
        let lines = text_lines("# top `topic`\n## section\n### details");

        // h1: marker dropped, title bold and underlined, backticks preserved.
        let h1 = to_line(&lines[0], &theme);
        let text: String = h1.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "top `topic`");
        let accent = h1
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "topic")
            .expect("code span");
        assert_eq!(accent.style.fg, Some(theme.accent));
        assert!(h1
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(
            h1.spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED),
            "h1 is underlined: {:?}",
            h1.spans[0].style
        );

        // h2: bold, not underlined.
        let h2 = to_line(&lines[1], &theme);
        assert_eq!(h2.spans[0].content.as_ref(), "section");
        assert!(h2.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(
            !h2.spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED),
            "h2 is not underlined: {:?}",
            h2.spans[0].style
        );

        // h3 is a heading too; deeper levels stop at six.
        assert_eq!(to_line(&lines[2], &theme).spans[0].content.as_ref(), "details");
    }

    #[test]
    fn quote_and_fence_win_over_heading_detection() {
        let lines = text_lines("> # quoted heading\n```\n# not a heading\n```");
        assert_eq!(lines[0].kind, LineKind::Blockquote);
        assert!(lines[1..].iter().all(|l| l.kind == LineKind::Code));
    }
}
