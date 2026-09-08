//! Stream Family — Composable wrappers for provider-specific stream processing
//!
//! Each Provider belongs to a "stream family" that determines which wrappers
//! are applied to its `CompletionStream`.  Wrappers are composable functions
//! `CompletionStream -> CompletionStream` similar to `tower::Layer`.
//!
//! ```rust,ignore
//! let registry = StreamFamilyRegistry::default();
//! let stream = provider.stream(req).await?;
//! let wrapped = registry.apply(ProviderStreamFamily::OpenAi, stream);
//! ```

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::providers::{CompletionChunk, CompletionStream, ToolCall};

/// A composable stream wrapper — transforms a `CompletionStream` into another.
pub type StreamWrapper = Arc<dyn Fn(CompletionStream) -> CompletionStream + Send + Sync>;

/// Stream family classification for providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderStreamFamily {
    /// Standard OpenAI streaming (SSE with delta format)
    OpenAi,
    /// Anthropic streaming (SSE with content_block_delta)
    Anthropic,
    /// OpenAI with reasoning (o1/o3 series)
    OpenAiReasoning,
    /// Claude with thinking mode (thinking + ephemeral cache)
    AnthropicThinking,
    /// Google Gemini with thinkingConfig
    GoogleThinking,
    /// OpenRouter with provider routing metadata
    OpenRouter,
    /// Moonshot/Kimi OpenAI-compatible API
    Moonshot,
    /// MiniMax OpenAI-compatible API
    Minimax,
    /// Catch-all for unknown providers
    Generic,
}

/// Registry of stream families — each family has a chain of wrappers.
pub struct StreamFamilyRegistry {
    families: std::collections::HashMap<ProviderStreamFamily, Vec<StreamWrapper>>,
}

impl Default for StreamFamilyRegistry {
    fn default() -> Self {
        let default_chain = default_wrappers();
        let mut registry = Self::empty();
        registry.register(ProviderStreamFamily::OpenAi, default_chain.clone());
        registry.register(ProviderStreamFamily::Anthropic, default_chain.clone());
        registry.register(ProviderStreamFamily::OpenAiReasoning, default_chain.clone());
        registry.register(ProviderStreamFamily::AnthropicThinking, default_chain.clone());
        registry.register(ProviderStreamFamily::OpenRouter, default_chain.clone());
        registry.register(ProviderStreamFamily::GoogleThinking, default_chain.clone());
        registry.register(ProviderStreamFamily::Moonshot, default_chain.clone());
        registry.register(ProviderStreamFamily::Minimax, default_chain);
        registry
    }
}

/// Default wrapper chain shared by all stream families.
fn default_wrappers() -> Vec<StreamWrapper> {
    vec![
        thinking_tag_extractor_wrapper(),
        tool_call_accumulator_wrapper(),
        html_entity_decoder_wrapper(),
        json_repair_wrapper(),
        usage_extractor_wrapper(),
    ]
}

impl StreamFamilyRegistry {
    pub fn empty() -> Self {
        Self {
            families: std::collections::HashMap::new(),
        }
    }

    pub fn register(&mut self, family: ProviderStreamFamily, wrappers: Vec<StreamWrapper>) {
        self.families.insert(family, wrappers);
    }

    /// Apply the wrapper chain for a family to a stream.
    pub fn apply(
        &self,
        family: ProviderStreamFamily,
        stream: CompletionStream,
    ) -> CompletionStream {
        match self.families.get(&family) {
            Some(wrappers) => wrappers.iter().fold(stream, |s, w| w(s)),
            None => stream,
        }
    }
}

// ------------------------------------------------------------------
// Built-in wrappers
// ------------------------------------------------------------------

/// Wrapper that accumulates partial tool_call deltas into complete calls.
pub fn tool_call_accumulator_wrapper() -> StreamWrapper {
    Arc::new(|stream| {
        let wrapped = ToolCallAccumulator {
            inner: stream,
            buffer: Vec::new(),
        };
        Box::pin(wrapped)
    })
}

/// Wrapper that ensures the final chunk carries usage metadata.
pub fn usage_extractor_wrapper() -> StreamWrapper {
    Arc::new(|stream| {
        let wrapped = UsageExtractor {
            inner: stream,
            seen_usage: false,
            held_done: None,
            queued: None,
        };
        Box::pin(wrapped)
    })
}

/// Wrapper that extracts `<thinking>` / `</thinking>` tags from content
/// into the dedicated `reasoning_content` field.
///
/// Useful for providers that embed reasoning inside regular content.
pub fn thinking_tag_extractor_wrapper() -> StreamWrapper {
    Arc::new(|stream| {
        let wrapped = ThinkingTagExtractor {
            inner: stream,
            reasoning_buffer: String::new(),
            in_thinking: false,
        };
        Box::pin(wrapped)
    })
}

/// Wrapper that decodes HTML entity-encoded tool call arguments.
///
/// Some providers (e.g. certain OpenRouter proxies) return tool arguments
/// with HTML-encoded characters like `&quot;` instead of `"`.
pub fn html_entity_decoder_wrapper() -> StreamWrapper {
    Arc::new(|stream| {
        let wrapped = HtmlEntityDecoder { inner: stream };
        Box::pin(wrapped)
    })
}

/// Wrapper that attempts to repair truncated JSON in tool call arguments
/// on the final chunk.
pub fn json_repair_wrapper() -> StreamWrapper {
    Arc::new(|stream| {
        let wrapped = JsonRepair {
            inner: stream,
            tool_call_buffers: std::collections::HashMap::new(),
        };
        Box::pin(wrapped)
    })
}

// ------------------------------------------------------------------
// Wrapper implementations
// ------------------------------------------------------------------

struct ToolCallAccumulator {
    inner: CompletionStream,
    buffer: Vec<ToolCall>,
}

impl Stream for ToolCallAccumulator {
    type Item = CompletionChunk;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(mut chunk)) => {
                if let Some(ref calls) = chunk.tool_calls {
                    for call in calls {
                        if let Some(existing) = self.buffer.iter_mut().find(|c| c.id == call.id) {
                            // Accumulate partial arguments
                            if !call.function.arguments.is_empty() {
                                existing
                                    .function
                                    .arguments
                                    .push_str(&call.function.arguments);
                            }
                        } else {
                            self.buffer.push(call.clone());
                        }
                    }
                }
                if chunk.is_done && !self.buffer.is_empty() {
                    // Replace tool_calls with fully accumulated versions on final chunk
                    chunk.tool_calls = Some(self.buffer.clone());
                }
                Poll::Ready(Some(chunk))
            }
            other => other,
        }
    }
}

struct UsageExtractor {
    inner: CompletionStream,
    seen_usage: bool,
    /// A held-back `is_done` chunk whose usage is still `None`: the provider
    /// may follow it with a trailing usage-only frame (OpenAI
    /// `stream_options.include_usage`), which must be merged into it before
    /// it is emitted — consumers stop at the first `is_done` chunk, so a
    /// separate trailing frame would never be seen.
    held_done: Option<CompletionChunk>,
    /// One-slot outbox preserving chunk order when a held chunk is flushed
    /// while another chunk was just dequeued.
    queued: Option<CompletionChunk>,
}

impl UsageExtractor {
    /// Flush a held chunk at end-of-stream, synthesizing zero usage when the
    /// provider never reported any.
    fn flush_held(&mut self) -> Poll<Option<CompletionChunk>> {
        match self.held_done.take() {
            Some(mut held) => {
                if held.usage.is_none() && !self.seen_usage {
                    held.usage = Some(crate::providers::Usage::default());
                }
                Poll::Ready(Some(held))
            }
            None => Poll::Ready(None),
        }
    }
}

impl Stream for UsageExtractor {
    type Item = CompletionChunk;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Flush the outbox first to keep chunk order.
        if let Some(chunk) = self.queued.take() {
            return Poll::Ready(Some(chunk));
        }
        loop {
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(chunk)) => {
                    if chunk.usage.is_some() {
                        self.seen_usage = true;
                        // A trailing usage frame: merge it into the held
                        // final chunk (if any) so it is not lost.
                        if let Some(mut held) = self.held_done.take() {
                            held.usage = chunk.usage;
                            return Poll::Ready(Some(held));
                        }
                        return Poll::Ready(Some(chunk));
                    }
                    if chunk.is_done {
                        // Hold back; displace any previously held chunk
                        // (pathological double-done) into the outbox.
                        if let Some(mut held) = self.held_done.take() {
                            if held.usage.is_none() && !self.seen_usage {
                                held.usage = Some(crate::providers::Usage::default());
                                self.seen_usage = true;
                            }
                            self.queued = Some(held);
                        }
                        self.held_done = Some(chunk);
                        continue;
                    }
                    // Regular content chunk while a done chunk is held
                    // (out of protocol, but preserve order).
                    if let Some(held) = self.held_done.take() {
                        self.queued = Some(chunk);
                        return Poll::Ready(Some(held));
                    }
                    return Poll::Ready(Some(chunk));
                }
                Poll::Ready(None) => return self.flush_held(),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ------------------------------------------------------------------
// Thinking tag extractor — splits <thinking>...</thinking> from content
// ------------------------------------------------------------------

struct ThinkingTagExtractor {
    inner: CompletionStream,
    reasoning_buffer: String,
    in_thinking: bool,
}

impl Stream for ThinkingTagExtractor {
    type Item = CompletionChunk;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(mut chunk)) => {
                if let Some(ref content) = chunk.content {
                    let mut buffer = std::mem::take(&mut self.reasoning_buffer);
                    let mut in_thinking = self.in_thinking;
                    let (text, reasoning) =
                        extract_thinking_tags(content, &mut buffer, &mut in_thinking);
                    self.reasoning_buffer = buffer;
                    self.in_thinking = in_thinking;
                    if !text.is_empty() {
                        chunk.content = Some(text);
                    } else {
                        chunk.content = None;
                    }
                    if !reasoning.is_empty() {
                        chunk.reasoning_content = Some(reasoning);
                    }
                }
                Poll::Ready(Some(chunk))
            }
            other => other,
        }
    }
}

fn extract_thinking_tags(
    input: &str,
    buffer: &mut String,
    in_thinking: &mut bool,
) -> (String, String) {
    let mut text_parts = Vec::new();
    let mut reasoning_parts = Vec::new();

    // If we were inside a thinking tag, continue appending to buffer
    if *in_thinking {
        if let Some(end_pos) = input.find("</thinking>") {
            buffer.push_str(&input[..end_pos]);
            reasoning_parts.push(std::mem::take(buffer));
            *in_thinking = false;
            let after = &input[end_pos + "</thinking>".len()..];
            if !after.is_empty() {
                text_parts.push(after.to_string());
            }
        } else {
            buffer.push_str(input);
            return (String::new(), String::new());
        }
    } else {
        let mut remaining = input;
        while let Some(start_pos) = remaining.find("<thinking>") {
            let before = &remaining[..start_pos];
            if !before.is_empty() {
                text_parts.push(before.to_string());
            }
            let after_start = &remaining[start_pos + "<thinking>".len()..];
            if let Some(end_pos) = after_start.find("</thinking>") {
                reasoning_parts.push(after_start[..end_pos].to_string());
                remaining = &after_start[end_pos + "</thinking>".len()..];
            } else {
                buffer.push_str(after_start);
                *in_thinking = true;
                remaining = "";
                break;
            }
        }
        if !remaining.is_empty() {
            text_parts.push(remaining.to_string());
        }
    }

    (text_parts.join(""), reasoning_parts.join(""))
}

// ------------------------------------------------------------------
// HTML entity decoder — fixes &quot; etc. in tool call arguments
// ------------------------------------------------------------------

struct HtmlEntityDecoder {
    inner: CompletionStream,
}

impl Stream for HtmlEntityDecoder {
    type Item = CompletionChunk;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(mut chunk)) => {
                if let Some(ref mut calls) = chunk.tool_calls {
                    for call in calls.iter_mut() {
                        call.function.arguments = decode_html_entities(&call.function.arguments);
                    }
                }
                Poll::Ready(Some(chunk))
            }
            other => other,
        }
    }
}

fn decode_html_entities(input: &str) -> String {
    let mut result = input.to_string();
    let entities: [(&str, &str); 5] = [
        ("&quot;", "\""),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&#39;", "'"),
    ];
    for (encoded, decoded) in &entities {
        result = result.replace(encoded, decoded);
    }
    result
}

// ------------------------------------------------------------------
// JSON repair — attempts to fix truncated JSON tool arguments
// ------------------------------------------------------------------

struct JsonRepair {
    inner: CompletionStream,
    tool_call_buffers: std::collections::HashMap<String, String>,
}

impl Stream for JsonRepair {
    type Item = CompletionChunk;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(mut chunk)) => {
                if let Some(ref calls) = chunk.tool_calls {
                    for call in calls {
                        if !call.function.arguments.is_empty() {
                            self.tool_call_buffers
                                .insert(call.id.clone(), call.function.arguments.clone());
                        }
                    }
                }
                if chunk.is_done {
                    // Attempt to repair buffered JSON on final chunk
                    if let Some(ref mut calls) = chunk.tool_calls {
                        for call in calls.iter_mut() {
                            if let Some(buffered) = self.tool_call_buffers.get(&call.id) {
                                call.function.arguments = repair_json_truncation(buffered);
                            }
                        }
                    }
                }
                Poll::Ready(Some(chunk))
            }
            other => other,
        }
    }
}

/// Naive JSON truncation repair: close open braces/brackets/quotes.
fn repair_json_truncation(input: &str) -> String {
    let mut result = input.to_string();
    let mut open_braces = 0i32;
    let mut open_brackets = 0i32;
    let mut in_string = false;
    let mut escape_next = false;

    for ch in result.chars() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => open_braces += 1,
            '}' if !in_string => open_braces -= 1,
            '[' if !in_string => open_brackets += 1,
            ']' if !in_string => open_brackets -= 1,
            _ => {}
        }
    }

    // Close any unclosed string
    if in_string {
        result.push('"');
    }

    // Close objects and arrays (braces before brackets for nested structures)
    while open_braces > 0 {
        result.push('}');
        open_braces -= 1;
    }
    while open_brackets > 0 {
        result.push(']');
        open_brackets -= 1;
    }

    result
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    fn test_chunks() -> Vec<CompletionChunk> {
        vec![
            CompletionChunk {
                content: Some("Hello".to_string()),
                reasoning_content: None,
                tool_calls: None,
                is_done: false,
                usage: None,
            },
            CompletionChunk {
                content: Some(" world".to_string()),
                reasoning_content: None,
                tool_calls: None,
                is_done: true,
                usage: Some(crate::providers::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 2,
                    total_tokens: 12,
                    ..Default::default()
                }),
            },
        ]
    }

    #[tokio::test]
    async fn test_reasoning_wrapper_passes_through() {
        let chunks = test_chunks();
        let stream = Box::pin(futures::stream::iter(chunks.clone()));
        let registry = StreamFamilyRegistry::default();
        let wrapped = registry.apply(ProviderStreamFamily::OpenAi, stream);

        let result: Vec<_> = wrapped.collect().await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, Some("Hello".to_string()));
        assert_eq!(result[1].content, Some(" world".to_string()));
    }

    #[tokio::test]
    async fn test_usage_extractor_adds_default_on_missing() {
        let chunks = vec![CompletionChunk {
            content: Some("done".to_string()),
            reasoning_content: None,
            tool_calls: None,
            is_done: true,
            usage: None,
        }];
        let stream = Box::pin(futures::stream::iter(chunks));
        let wrapped = usage_extractor_wrapper()(stream);

        let result: Vec<_> = wrapped.collect().await;
        assert_eq!(result.len(), 1);
        assert!(result[0].usage.is_some());
    }

    #[tokio::test]
    async fn test_usage_extractor_merges_trailing_usage_frame() {
        // OpenAI include_usage: the finish_reason frame (usage=None) is
        // followed by a usage-only frame. Consumers stop at the first
        // is_done chunk, so the trailing usage must be merged into it.
        let chunks = vec![
            CompletionChunk {
                content: Some("answer".to_string()),
                reasoning_content: None,
                tool_calls: None,
                is_done: false,
                usage: None,
            },
            CompletionChunk {
                content: None,
                reasoning_content: None,
                tool_calls: None,
                is_done: true,
                usage: None,
            },
            CompletionChunk {
                content: None,
                reasoning_content: None,
                tool_calls: None,
                is_done: true,
                usage: Some(crate::providers::Usage {
                    prompt_tokens: 50,
                    completion_tokens: 10,
                    total_tokens: 60,
                    ..Default::default()
                }),
            },
        ];
        let stream = Box::pin(futures::stream::iter(chunks));
        let wrapped = usage_extractor_wrapper()(stream);

        let result: Vec<_> = wrapped.collect().await;
        // The usage-only frame is folded into the held final chunk.
        assert_eq!(result.len(), 2);
        assert!(result[1].is_done);
        let usage = result[1].usage.expect("merged usage");
        assert_eq!(usage.prompt_tokens, 50);
        assert_eq!(usage.total_tokens, 60);
    }

    #[tokio::test]
    async fn test_registry_unknown_family_passes_through() {
        let chunks = test_chunks();
        let stream = Box::pin(futures::stream::iter(chunks));
        let registry = StreamFamilyRegistry::default();
        let wrapped = registry.apply(ProviderStreamFamily::Generic, stream);

        let result: Vec<_> = wrapped.collect().await;
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_extract_thinking_tags_basic() {
        let mut buffer = String::new();
        let mut in_thinking = false;
        let (text, reasoning) = extract_thinking_tags(
            "hello <thinking>thinking here</thinking> end",
            &mut buffer,
            &mut in_thinking,
        );
        assert_eq!(text, "hello  end");
        assert_eq!(reasoning, "thinking here");
        assert!(!in_thinking);
    }

    #[test]
    fn test_extract_thinking_tags_split() {
        let mut buffer = String::new();
        let mut in_thinking = false;
        let (text, reasoning) =
            extract_thinking_tags("start <thinking>continue ", &mut buffer, &mut in_thinking);
        assert_eq!(text, "start ");
        assert_eq!(reasoning, "");
        assert!(in_thinking);

        let (text2, reasoning2) = extract_thinking_tags("more ", &mut buffer, &mut in_thinking);
        assert_eq!(text2, "");
        assert_eq!(reasoning2, "");
        assert!(in_thinking);

        let (text3, reasoning3) =
            extract_thinking_tags("end</thinking> after", &mut buffer, &mut in_thinking);
        assert_eq!(text3, " after");
        assert_eq!(reasoning3, "continue more end");
        assert!(!in_thinking);
    }

    #[test]
    fn test_decode_html_entities() {
        assert_eq!(decode_html_entities("&quot;hello&quot;"), "\"hello\"");
        assert_eq!(decode_html_entities("&lt;div&gt;"), "<div>");
        assert_eq!(decode_html_entities("&amp;"), "&");
    }

    #[test]
    fn test_repair_json_truncation() {
        assert_eq!(repair_json_truncation("{\"a\":1"), "{\"a\":1}");
        assert_eq!(repair_json_truncation("[{\"a\":1"), "[{\"a\":1}]");
        assert_eq!(repair_json_truncation("{\"a\":\"b"), "{\"a\":\"b\"}");
    }

    #[tokio::test]
    async fn test_thinking_tag_extractor_wrapper() {
        let chunks = vec![
            CompletionChunk {
                content: Some("Hello <thinking>thinking here</thinking>".to_string()),
                reasoning_content: None,
                tool_calls: None,
                is_done: false,
                usage: None,
            },
            CompletionChunk {
                content: Some(" world".to_string()),
                reasoning_content: None,
                tool_calls: None,
                is_done: true,
                usage: None,
            },
        ];
        let stream = Box::pin(futures::stream::iter(chunks));
        let wrapped = thinking_tag_extractor_wrapper()(stream);

        let result: Vec<_> = wrapped.collect().await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, Some("Hello ".to_string()));
        assert_eq!(result[0].reasoning_content, Some("thinking here".to_string()));
        assert_eq!(result[1].content, Some(" world".to_string()));
    }
}
