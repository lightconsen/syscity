//! Turning transcript entries into renderable lines.
//!
//! Two directions live here: `TranscriptLine` → styled [`Line`] for rendering,
//! and gateway history → `TranscriptLine` so a resumed session's past reads
//! like the conversation that produced it.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::tui::gateway_calls::HistoryMessage;
use crate::tui::transcript::{LineKind, TranscriptLine};
use crate::tui::ui::Theme;

/// The style a transcript line kind renders with.
pub fn kind_style(theme: &Theme, kind: LineKind) -> Style {
    match kind {
        LineKind::User => theme.user_style(),
        LineKind::Assistant => theme.assistant_style(),
        LineKind::Reasoning => theme.reasoning_style(),
        LineKind::Tool => theme.tool_call_style(),
        LineKind::ToolResult => theme.tool_call_style().add_modifier(Modifier::DIM),
        LineKind::Code => theme.code_style(),
        LineKind::Notice => theme.system_style(),
        LineKind::Separator => Style::default(),
    }
}

/// Render one transcript line.
pub fn to_line(entry: &TranscriptLine, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(entry.text.clone(), kind_style(theme, entry.kind)))
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

/// Prose → transcript lines, tagging fenced regions as code.
pub fn text_lines(text: &str) -> Vec<TranscriptLine> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut in_fence = false;
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push(TranscriptLine::new(LineKind::Code, line.to_string()));
        } else if in_fence {
            out.push(TranscriptLine::new(LineKind::Code, line.to_string()));
        } else {
            out.push(TranscriptLine::new(LineKind::Assistant, line.to_string()));
        }
    }
    out
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
}
