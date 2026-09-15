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
use crate::tui::ui::{
    assistant_style, code_style, reasoning_style, system_style, tool_call_style, user_style,
};

/// The style a transcript line kind renders with.
pub fn kind_style(kind: LineKind) -> Style {
    match kind {
        LineKind::User => user_style(),
        LineKind::Assistant => assistant_style(),
        LineKind::Reasoning => reasoning_style(),
        LineKind::Tool => tool_call_style(),
        LineKind::ToolResult => tool_call_style().add_modifier(Modifier::DIM),
        LineKind::Code => code_style(),
        LineKind::Notice => system_style(),
        LineKind::Separator => Style::default(),
    }
}

/// Render one transcript line.
pub fn to_line(entry: &TranscriptLine) -> Line<'static> {
    Line::from(Span::styled(entry.text.clone(), kind_style(entry.kind)))
}

/// Render a batch of transcript lines.
pub fn to_lines(entries: &[TranscriptLine]) -> Vec<Line<'static>> {
    entries.iter().map(to_line).collect()
}

/// Pretty-print tool arguments, trimmed to a few lines so one enormous call
/// cannot take over the screen.
fn args_lines(args: &Value, indent: &str, max_lines: usize) -> Vec<String> {
    let text = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
    let mut lines: Vec<String> = text
        .lines()
        .take(max_lines)
        .map(|l| format!("{indent}{l}"))
        .collect();
    if text.lines().count() > max_lines {
        lines.push(format!("{indent}…"));
    }
    lines
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
                for line in args_lines(&args, "  ", 8) {
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
