//! Context Compression for managing long conversations
//!
//! This module implements context window management by compressing
//! messages when approaching token limits.
//!
//! In addition to the heuristic strategies (`OldestFirst`, `Summarize`,
//! `SlidingWindow`), a `compact_with_llm` helper is provided that asks an
//! LLM provider to write a concise summary of the mid-section of the history
//! so recent context is preserved while tokens are freed.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::observe::record::CompressionObservation;
use crate::providers::{Message, Provider, Role};

/// Estimated tokens per character (approximation)
const TOKENS_PER_CHAR: f32 = 0.25;

/// Priority levels for messages during compression
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessagePriority {
    /// Critical - never remove (system prompts, todos)
    Critical = 3,
    /// High - prefer to keep (recent messages, tool results)
    High = 2,
    /// Normal - can be summarized (assistant responses)
    Normal = 1,
    /// Low - can be removed (old user messages)
    Low = 0,
}

/// A message with priority metadata
#[derive(Debug, Clone)]
pub struct PrioritizedMessage {
    /// The message
    pub message: Message,
    /// Priority level
    pub priority: MessagePriority,
    /// Original index
    pub index: usize,
    /// Whether this message has been summarized
    pub summarized: bool,
}

impl PrioritizedMessage {
    /// Create a new prioritized message
    pub fn new(message: Message, index: usize, total: usize) -> Self {
        let priority = Self::calculate_priority(&message, index, total);
        Self {
            message,
            priority,
            index,
            summarized: false,
        }
    }

    /// Calculate priority based on message content and position.
    ///
    /// The last `RECENT_THRESHOLD` messages are always High priority.
    const RECENT_THRESHOLD: usize = 4;

    fn calculate_priority(message: &Message, index: usize, total: usize) -> MessagePriority {
        // Recent messages (last RECENT_THRESHOLD) are high priority
        if index >= total.saturating_sub(Self::RECENT_THRESHOLD) {
            return MessagePriority::High;
        }

        match message.role {
            Role::System => MessagePriority::Critical,
            Role::Tool => MessagePriority::High,
            Role::Assistant => {
                // Check if contains important markers
                if message.content.contains("Task") || message.content.contains("todo") {
                    MessagePriority::High
                } else {
                    MessagePriority::Normal
                }
            }
            Role::User => {
                // Recent user messages are high priority
                if index >= total.saturating_sub(Self::RECENT_THRESHOLD) {
                    MessagePriority::High
                } else {
                    MessagePriority::Low
                }
            }
        }
    }

    /// Estimate token count
    pub fn estimated_tokens(&self) -> usize {
        (self.message.content.len() as f32 * TOKENS_PER_CHAR) as usize + 4
    }
}

/// Context compressor for managing token budget
#[derive(Debug, Clone)]
pub struct ContextCompressor {
    /// Target token count after compression
    target_tokens: usize,
    /// Minimum tokens to trigger compression
    compression_threshold: usize,
    /// Strategy for compression
    strategy: CompressionStrategy,
}

/// Compression strategies
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionStrategy {
    /// Remove oldest low-priority messages first
    ///
    /// NOTE: pair-unsafe. Selection here is by priority, not by adjacency, so
    /// it can keep a tool result and drop the assistant `tool_calls` it
    /// answers (or the reverse). Nothing in production calls `compress()` with
    /// this strategy — `compact_context` asks for `Summarize`, and automatic
    /// compaction goes through `compact_with_llm` / `Context::summarize`, both
    /// of which keep pairs together. Give it a pair check before using it.
    OldestFirst,
    /// Summarize groups of messages
    Summarize,
    /// Sliding window (keep only recent messages)
    SlidingWindow,
}

impl ContextCompressor {
    /// Create a new compressor with target token count
    pub fn new(target_tokens: usize) -> Self {
        Self {
            target_tokens,
            compression_threshold: (target_tokens as f32 * 1.2) as usize,
            strategy: CompressionStrategy::OldestFirst,
        }
    }

    /// Set compression threshold (as percentage of target)
    pub fn with_threshold(mut self, threshold_percent: f32) -> Self {
        self.compression_threshold = (self.target_tokens as f32 * threshold_percent) as usize;
        self
    }

    /// Set compression strategy
    pub fn with_strategy(mut self, strategy: CompressionStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Check if compression is needed
    pub fn needs_compression(&self, messages: &[Message]) -> bool {
        self.estimate_tokens(messages) > self.compression_threshold
    }

    /// Estimate total tokens for a set of messages
    pub fn estimate_tokens(&self, messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|m| (m.content.len() as f32 * TOKENS_PER_CHAR) as usize + 4)
            .sum()
    }

    /// Compress messages to target token count
    pub fn compress(&self, messages: &[Message]) -> Vec<Message> {
        let current_tokens = self.estimate_tokens(messages);

        if current_tokens <= self.target_tokens {
            debug!("No compression needed: {} <= {} tokens", current_tokens, self.target_tokens);
            return messages.to_vec();
        }

        info!("Compressing context: {} -> ~{} tokens", current_tokens, self.target_tokens);

        match self.strategy {
            CompressionStrategy::OldestFirst => self.compress_oldest_first(messages),
            CompressionStrategy::Summarize => self.compress_summarize(messages),
            CompressionStrategy::SlidingWindow => self.compress_sliding_window(messages),
        }
    }

    /// Compress by removing oldest low-priority messages
    fn compress_oldest_first(&self, messages: &[Message]) -> Vec<Message> {
        // Create prioritized messages
        let mut prioritized: Vec<PrioritizedMessage> = messages
            .iter()
            .enumerate()
            .map(|(i, m)| PrioritizedMessage::new(m.clone(), i, messages.len()))
            .collect();

        // Sort by priority (desc) then index (desc) to keep recent high-priority
        prioritized.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| b.index.cmp(&a.index))
        });

        // Select messages until we hit the target
        let mut result = Vec::new();
        let mut total_tokens = 0;

        for pm in prioritized {
            let tokens = pm.estimated_tokens();
            if total_tokens + tokens <= self.target_tokens
                || pm.priority == MessagePriority::Critical
            {
                result.push(pm);
                total_tokens += tokens;
            }
        }

        // Sort back by original index
        result.sort_by_key(|pm| pm.index);

        info!(
            "Compressed from {} to {} messages (~{} tokens)",
            messages.len(),
            result.len(),
            total_tokens
        );

        result.into_iter().map(|pm| pm.message).collect()
    }

    /// Compress by summarizing message groups
    fn compress_summarize(&self, messages: &[Message]) -> Vec<Message> {
        // Keep system and recent messages. The tail boundary moves forward past
        // any tool result that would start it: a `tool` message whose assistant
        // `tool_calls` ended up inside the summary is a request providers
        // refuse. `compact_with_llm` snaps its boundaries the same way.
        const KEEP_TAIL: usize = 4;
        let mut tail_start = messages.len().saturating_sub(KEEP_TAIL);
        while tail_start < messages.len() && messages[tail_start].role == Role::Tool {
            tail_start += 1;
        }

        let mut result = Vec::new();
        let mut to_summarize = Vec::new();

        for (i, msg) in messages.iter().enumerate() {
            if msg.role == Role::System || i >= tail_start {
                result.push(msg.clone());
            } else {
                to_summarize.push(msg.clone());
            }
        }

        // If we have messages to summarize, create a summary
        if !to_summarize.is_empty() {
            let summary = self.create_summary(&to_summarize);
            result.insert(1, summary);
        }

        info!("Summarized {} messages into {} messages", messages.len(), result.len());

        result
    }

    /// Compress using sliding window (keep only recent)
    fn compress_sliding_window(&self, messages: &[Message]) -> Vec<Message> {
        // Always keep system messages
        let system_messages: Vec<_> = messages
            .iter()
            .filter(|m| m.role == Role::System)
            .cloned()
            .collect();

        let system_tokens: usize = system_messages
            .iter()
            .map(|m| (m.content.len() as f32 * TOKENS_PER_CHAR) as usize)
            .sum();

        let available_tokens = self.target_tokens.saturating_sub(system_tokens);

        // Add recent messages until we hit the limit
        let mut recent = Vec::new();
        let mut total_tokens = 0;

        for msg in messages.iter().rev().filter(|m| m.role != Role::System) {
            let tokens = (msg.content.len() as f32 * TOKENS_PER_CHAR) as usize + 4;
            if total_tokens + tokens <= available_tokens {
                recent.push(msg.clone());
                total_tokens += tokens;
            } else {
                break;
            }
        }

        recent.reverse();

        // Same boundary rule as `compress_summarize`: a tool result left
        // stranded at the window's edge (its assistant `tool_calls` fell
        // outside) is a request providers refuse, so the pair boundary wins
        // over the token budget.
        let first_kept = recent
            .iter()
            .position(|m| m.role != Role::Tool)
            .unwrap_or(recent.len());
        recent.drain(..first_kept);

        let mut result = system_messages;
        result.extend(recent);

        info!("Sliding window: kept {} of {} messages", result.len(), messages.len());

        result
    }

    /// Create a heuristic summary message from a set of messages.
    ///
    /// Groups turns into user-request / assistant-response pairs and produces a
    /// compact digest that preserves the key intent of each exchange.  This is
    /// a best-effort sync fallback; for LLM-quality summarization use
    /// [`compact_with_llm`].
    fn create_summary(&self, messages: &[Message]) -> Message {
        let mut lines: Vec<String> = Vec::new();

        let non_empty: Vec<&Message> = messages.iter().filter(|m| !m.content.is_empty()).collect();

        let mut i = 0;
        while i < non_empty.len() {
            let msg = non_empty[i];
            match msg.role {
                Role::User => {
                    // User turn: capture the request intent (up to 150 chars)
                    let preview: String = msg.content.chars().take(150).collect();
                    let ellipsis = if msg.content.len() > 150 { "…" } else { "" };
                    lines.push(format!("Q: {}{}", preview, ellipsis));

                    // Peek at the following assistant turn if present
                    if i + 1 < non_empty.len() && non_empty[i + 1].role == Role::Assistant {
                        let resp = non_empty[i + 1];
                        let preview: String = resp.content.chars().take(250).collect();
                        let ellipsis = if resp.content.len() > 250 { "…" } else { "" };
                        lines.push(format!("A: {}{}", preview, ellipsis));
                        i += 2;
                        continue;
                    }
                }
                Role::Assistant => {
                    let preview: String = msg.content.chars().take(250).collect();
                    let ellipsis = if msg.content.len() > 250 { "…" } else { "" };
                    lines.push(format!("A: {}{}", preview, ellipsis));
                }
                _ => {
                    // Skip tool calls and other roles in the summary
                }
            }
            i += 1;
        }

        let content =
            format!("[Summary of {} previous messages]\n{}", messages.len(), lines.join("\n"));

        Message {
            role: Role::System,
            content,
            content_blocks: None,
            reasoning_content: None,
            name: Some("summary".to_string()),
            tool_calls: None,
            tool_call_id: None,
            metadata: None,
        }
    }

    /// Compact `messages` using an LLM to summarise the mid-section.
    ///
    /// Keeps the first `keep_head` and last `keep_tail` messages intact and
    /// asks `provider` to summarise everything in-between.  Returns the
    /// compacted list on success; on any provider error the original messages
    /// are returned unchanged (graceful degradation).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use std::sync::Arc;
    /// # use syscity::agent::compressor::ContextCompressor;
    /// # async fn example(provider: Arc<dyn syscity::providers::Provider>, messages: Vec<syscity::providers::Message>) {
    /// let compressor = ContextCompressor::new(4096);
    /// let compacted = compressor.compact_with_llm(&messages, &provider, None, 2, 6).await;
    /// # }
    /// ```
    pub async fn compact_with_llm(
        &self,
        messages: &[Message],
        provider: &Arc<dyn Provider>,
        model: Option<&str>,
        keep_head: usize,
        keep_tail: usize,
    ) -> Vec<Message> {
        let n = messages.len();

        // Nothing to summarise if the history is too short.
        let mut mid_start = keep_head;
        let mut mid_end = n.saturating_sub(keep_tail);
        if mid_start >= mid_end {
            debug!("compact_with_llm: history too short to summarise, returning as-is");
            return messages.to_vec();
        }

        // Tool-pair boundary safety: the kept tail must never start on a tool
        // result (its assistant tool-call would be summarised away) and the cut
        // point must never leave an assistant tool-call without its results.
        while mid_start < mid_end && messages[mid_start].role == Role::Tool {
            mid_start += 1;
        }
        while mid_end < n && messages[mid_end].role == Role::Tool {
            mid_end += 1;
        }
        if mid_start >= mid_end {
            debug!("compact_with_llm: history too short after pair snapping, returning as-is");
            return messages.to_vec();
        }

        let head = &messages[..mid_start];
        let mid = &messages[mid_start..mid_end];
        let tail = &messages[mid_end..];

        // Build a compact transcript of the mid section for the LLM prompt.
        let transcript: String = mid
            .iter()
            .filter(|m| !m.content.is_empty())
            .map(|m| format!("{}: {}", m.role, m.content.chars().take(400).collect::<String>()))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "Summarise the following conversation excerpt in ≤150 words, preserving key facts, \
             decisions, and named entities. Output only the summary text.\n\n{}",
            transcript
        );

        let req = crate::providers::CompletionRequest {
            model: model.map(str::to_string),
            messages: vec![Message::user(prompt)],
            max_tokens: Some(300),
            temperature: Some(0.3),
            tools: None,
            stream: false,
            stop: None,
            extra: None,
            ..Default::default()
        };

        match provider.complete(req).await {
            Ok(response) => {
                let summary_text = response.message.content;
                if summary_text.is_empty() {
                    warn!("compact_with_llm: provider returned empty summary, skipping");
                    return messages.to_vec();
                }

                info!(
                    "compact_with_llm: summarised {} messages into {} chars",
                    mid.len(),
                    summary_text.len()
                );

                let summary_msg = Message {
                    role: Role::System,
                    content: format!(
                        "[Summary of {} earlier messages]\n{}",
                        mid.len(),
                        summary_text
                    ),
                    content_blocks: None,
                    reasoning_content: None,
                    name: Some("compaction_summary".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    metadata: None,
                };

                let mut result = head.to_vec();
                result.push(summary_msg);
                result.extend_from_slice(tail);
                result
            }
            Err(e) => {
                warn!("compact_with_llm: provider error (returning original): {}", e);
                messages.to_vec()
            }
        }
    }

    /// Get compression statistics
    pub fn stats(&self, before: &[Message], after: &[Message]) -> CompressionStats {
        let before_tokens = self.estimate_tokens(before);
        let after_tokens = self.estimate_tokens(after);

        CompressionStats {
            before_messages: before.len(),
            after_messages: after.len(),
            before_tokens,
            after_tokens,
            reduction_percent: if before_tokens > 0 {
                ((before_tokens - after_tokens) as f32 / before_tokens as f32) * 100.0
            } else {
                0.0
            },
        }
    }
}

/// Compression statistics
#[derive(Debug, Clone)]
pub struct CompressionStats {
    /// Messages before compression
    pub before_messages: usize,
    /// Messages after compression
    pub after_messages: usize,
    /// Tokens before compression
    pub before_tokens: usize,
    /// Tokens after compression
    pub after_tokens: usize,
    /// Reduction percentage
    pub reduction_percent: f32,
}

impl std::fmt::Display for CompressionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Compression: {} -> {} messages ({} -> {} tokens, {:.1}% reduction)",
            self.before_messages,
            self.after_messages,
            self.before_tokens,
            self.after_tokens,
            self.reduction_percent
        )
    }
}

/// Build a [`CompressionObservation`] for a compaction, computing the retention
/// quality metrics (§三) from the raw token counts.
///
/// The `min_retention_ratio` threshold is taken from
/// [`DEFAULT_MIN_RETENTION_RATIO`](crate::observe::record::DEFAULT_MIN_RETENTION_RATIO)
/// because the gateway `CompressionQualityConfig` is not threaded into the
/// compression call sites (which live in `agent::engine` / the observe
/// collector).
pub fn build_compression_observation(
    triggered_at_ms: u64,
    tokens_before: usize,
    tokens_after: usize,
    strategy: impl Into<String>,
) -> CompressionObservation {
    CompressionObservation::from_counts(
        triggered_at_ms,
        tokens_before,
        tokens_after,
        strategy,
        crate::observe::record::DEFAULT_MIN_RETENTION_RATIO,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{CompletionRequest, CompletionResponse, CompletionStream};

    fn create_test_messages(count: usize) -> Vec<Message> {
        (0..count)
            .map(|i| Message {
                role: if i == 0 { Role::System } else { Role::User },
                content: format!("Message {} with some content", i),
                content_blocks: None,
                reasoning_content: None,
                name: None,
                tool_calls: None,
                tool_call_id: None,
                metadata: None,
            })
            .collect()
    }

    fn msg(role: Role, content: impl Into<String>) -> Message {
        Message {
            role,
            content: content.into(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            metadata: None,
        }
    }

    // ── MessagePriority ───────────────────────────────────────────────────────

    #[test]
    fn test_message_priority_ordering() {
        assert!(MessagePriority::Critical > MessagePriority::High);
        assert!(MessagePriority::High > MessagePriority::Normal);
        assert!(MessagePriority::Normal > MessagePriority::Low);
    }

    #[test]
    fn test_message_priority_equality() {
        assert_eq!(MessagePriority::Critical, MessagePriority::Critical);
        assert_eq!(MessagePriority::High, MessagePriority::High);
        assert_eq!(MessagePriority::Normal, MessagePriority::Normal);
        assert_eq!(MessagePriority::Low, MessagePriority::Low);
    }

    // ── PrioritizedMessage ────────────────────────────────────────────────────

    #[test]
    fn test_prioritized_message_system_priority() {
        let pm = PrioritizedMessage::new(msg(Role::System, "sys"), 0, 10);
        assert_eq!(pm.priority, MessagePriority::Critical);
    }

    #[test]
    fn test_prioritized_message_tool_priority() {
        let pm = PrioritizedMessage::new(msg(Role::Tool, "tool result"), 0, 10);
        assert_eq!(pm.priority, MessagePriority::High);
    }

    #[test]
    fn test_prioritized_message_assistant_with_task() {
        let pm = PrioritizedMessage::new(msg(Role::Assistant, "Task: do something"), 0, 10);
        assert_eq!(pm.priority, MessagePriority::High);
    }

    #[test]
    fn test_prioritized_message_assistant_with_todo() {
        let pm = PrioritizedMessage::new(msg(Role::Assistant, "Add a todo item"), 0, 10);
        assert_eq!(pm.priority, MessagePriority::High);
    }

    #[test]
    fn test_prioritized_message_assistant_normal() {
        let pm = PrioritizedMessage::new(msg(Role::Assistant, "Hello there"), 0, 10);
        assert_eq!(pm.priority, MessagePriority::Normal);
    }

    #[test]
    fn test_prioritized_message_user_old() {
        let pm = PrioritizedMessage::new(msg(Role::User, "old"), 3, 10);
        assert_eq!(pm.priority, MessagePriority::Low);
    }

    #[test]
    fn test_prioritized_message_user_recent() {
        let pm = PrioritizedMessage::new(msg(Role::User, "recent"), 4, 5);
        assert_eq!(pm.priority, MessagePriority::High);
    }

    #[test]
    fn test_prioritized_message_high_by_index() {
        // Any role with index in the last RECENT_THRESHOLD gets High priority
        let pm = PrioritizedMessage::new(msg(Role::User, "msg"), 7, 10);
        assert_eq!(pm.priority, MessagePriority::High);
    }

    #[test]
    fn test_prioritized_message_estimated_tokens() {
        let pm = PrioritizedMessage::new(msg(Role::User, "a".repeat(100)), 0, 10);
        // 100 chars * 0.25 = 25, + 4 = 29
        assert_eq!(pm.estimated_tokens(), 29);
    }

    #[test]
    fn test_prioritized_message_estimated_tokens_empty() {
        let pm = PrioritizedMessage::new(msg(Role::User, ""), 0, 10);
        // 0 chars * 0.25 = 0, + 4 = 4
        assert_eq!(pm.estimated_tokens(), 4);
    }

    #[test]
    fn test_prioritized_message_fields() {
        let m = msg(Role::User, "hello");
        let pm = PrioritizedMessage::new(m.clone(), 5, 10);
        assert_eq!(pm.index, 5);
        assert!(!pm.summarized);
        assert_eq!(pm.message.content, "hello");
    }

    // ── ContextCompressor builders ────────────────────────────────────────────

    #[test]
    fn test_compressor_creation() {
        let compressor = ContextCompressor::new(1000);
        // Use many messages to ensure we exceed the default 1.2x threshold
        assert!(compressor.needs_compression(&create_test_messages(150)));
    }

    #[test]
    fn test_compressor_with_threshold() {
        let compressor = ContextCompressor::new(100).with_threshold(0.5);
        // threshold = 100 * 0.5 = 50 tokens
        // 5 messages * ~7 tokens each = ~35 tokens, under 50
        assert!(!compressor.needs_compression(&create_test_messages(5)));
    }

    /// The codebase models a tool call and its result by adjacency, so a kept
    /// `Role::Tool` must be preceded by its call (or by the result of the same
    /// call): anything else is a request the provider refuses.
    fn assert_pairs_intact(messages: &[Message]) {
        let roles: Vec<&Role> = messages.iter().map(|m| &m.role).collect();
        for (i, m) in messages.iter().enumerate() {
            if m.role != Role::Tool {
                continue;
            }
            let paired = i.checked_sub(1).is_some_and(|prev| {
                let p = &messages[prev];
                p.role == Role::Tool || (p.role == Role::Assistant && p.tool_calls.is_some())
            });
            assert!(paired, "tool result at {i} has no preceding tool call: {roles:?}");
        }
    }

    /// The kept tail must never begin on a tool result.
    ///
    /// `Summarize` keeps the last few messages verbatim, so a tool result
    /// landing exactly on that boundary used to survive while its assistant
    /// `tool_calls` were folded into the summary — a `tool` message with no
    /// matching call, which providers reject.
    #[test]
    fn summarize_never_keeps_an_orphaned_tool_result() {
        let compressor = ContextCompressor::new(10).with_strategy(CompressionStrategy::Summarize);

        let mut call = msg(Role::Assistant, "");
        call.tool_calls = Some(vec![]);
        let tool_result = Message {
            role: Role::Tool,
            content: "tool result".to_string(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: Some("c1".to_string()),
            metadata: None,
        };
        // len 7 → the 4-message tail would start at index 3, which is the tool
        // result; snapping pushes it to the user message behind it.
        let messages = vec![
            msg(Role::System, "sys"),
            msg(Role::User, "do a thing"),
            call,
            tool_result,
            msg(Role::User, "next question"),
            msg(Role::Assistant, "an answer"),
            msg(Role::User, "and another"),
        ];

        let compressed = compressor.compress(&messages);

        assert!(
            !compressed.iter().any(|m| m.role == Role::Tool),
            "the orphaned tool result must not survive: {:?}",
            compressed.iter().map(|m| &m.role).collect::<Vec<_>>()
        );
        assert_pairs_intact(&compressed);
        assert_eq!(compressed[0].role, Role::System);
        assert_eq!(
            compressed[1].name.as_deref(),
            Some("summary"),
            "the summary follows the system prompt"
        );
        assert_eq!(compressed.len(), 5, "system + summary + three kept");
    }

    /// The same boundary rule for the sliding window: the pair wins over the
    /// budget. With room for the tool result but not for the assistant call it
    /// answers, neither is kept.
    #[test]
    fn sliding_window_never_keeps_an_orphaned_tool_result() {
        let compressor =
            ContextCompressor::new(18).with_strategy(CompressionStrategy::SlidingWindow);

        let mut call = msg(Role::Assistant, "calling");
        call.tool_calls = Some(vec![]);
        let tool_result = Message {
            role: Role::Tool,
            content: "a tool result that is long enough to matter".to_string(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: Some("c1".to_string()),
            metadata: None,
        };
        let messages = vec![
            msg(Role::System, "sys"),
            msg(Role::User, "do a thing"),
            msg(Role::Assistant, "an answer"),
            msg(Role::User, "next question"),
            call,
            tool_result,
        ];

        let compressed = compressor.compress(&messages);

        assert_pairs_intact(&compressed);
        assert!(
            !compressed.iter().any(|m| m.role == Role::Tool),
            "a stranded tool result must not be kept: {:?}",
            compressed.iter().map(|m| &m.role).collect::<Vec<_>>()
        );
        assert_eq!(compressed[0].role, Role::System, "system messages stay");
    }

    #[test]
    fn test_compressor_with_strategy() {
        let compressor =
            ContextCompressor::new(100).with_strategy(CompressionStrategy::SlidingWindow);
        let messages = create_test_messages(20);
        let compressed = compressor.compress(&messages);
        assert!(compressed.len() < messages.len());
    }

    // ── needs_compression / estimate_tokens ───────────────────────────────────

    #[test]
    fn test_needs_compression_false() {
        let compressor = ContextCompressor::new(10000);
        assert!(!compressor.needs_compression(&create_test_messages(5)));
    }

    #[test]
    fn test_needs_compression_true() {
        let compressor = ContextCompressor::new(10);
        // Default threshold = 10 * 1.2 = 12 tokens
        // 5 messages ~ 7 tokens each = ~35 tokens > 12
        assert!(compressor.needs_compression(&create_test_messages(5)));
    }

    #[test]
    fn test_estimate_tokens_empty() {
        let compressor = ContextCompressor::new(100);
        assert_eq!(compressor.estimate_tokens(&[]), 0);
    }

    #[test]
    fn test_estimate_tokens() {
        let compressor = ContextCompressor::new(100);
        let messages = vec![
            msg(Role::System, "a".repeat(100)),
            msg(Role::User, "b".repeat(100)),
        ];
        // Each: 100 * 0.25 = 25 + 4 = 29. Total = 58
        assert_eq!(compressor.estimate_tokens(&messages), 58);
    }

    // ── compress strategies ───────────────────────────────────────────────────

    #[test]
    fn test_compress_under_target_returns_original() {
        let compressor = ContextCompressor::new(10000);
        let messages = create_test_messages(5);
        let compressed = compressor.compress(&messages);
        assert_eq!(compressed.len(), messages.len());
        assert_eq!(compressed[0].content, messages[0].content);
    }

    #[test]
    fn test_sliding_window() {
        let compressor =
            ContextCompressor::new(100).with_strategy(CompressionStrategy::SlidingWindow);
        let messages = create_test_messages(20);
        let compressed = compressor.compress(&messages);

        assert!(compressed.len() < messages.len());
        // System message should be preserved
        assert!(compressed.iter().any(|m| m.role == Role::System));
    }

    #[test]
    fn test_sliding_window_empty() {
        let compressor =
            ContextCompressor::new(100).with_strategy(CompressionStrategy::SlidingWindow);
        let compressed: Vec<Message> = compressor.compress(&[]);
        assert!(compressed.is_empty());
    }

    #[test]
    fn test_sliding_window_keeps_system_only() {
        let compressor =
            ContextCompressor::new(10).with_strategy(CompressionStrategy::SlidingWindow);
        let messages = vec![
            msg(Role::System, "Important system instruction"),
            msg(Role::User, "First user message with lots of content"),
        ];
        let compressed = compressor.compress(&messages);
        assert!(compressed.iter().any(|m| m.role == Role::System));
    }

    #[test]
    fn test_oldest_first() {
        let compressor =
            ContextCompressor::new(150).with_strategy(CompressionStrategy::OldestFirst);
        let messages = create_test_messages(20);
        let compressed = compressor.compress(&messages);

        assert!(compressed.len() <= messages.len());
        // System message should be preserved
        assert!(compressed.iter().any(|m| m.role == Role::System));
    }

    #[test]
    fn test_oldest_first_single_message() {
        let compressor = ContextCompressor::new(10).with_strategy(CompressionStrategy::OldestFirst);
        let messages = vec![msg(Role::System, "sys")];
        let compressed = compressor.compress(&messages);
        // System is Critical so it stays even over target
        assert_eq!(compressed.len(), 1);
    }

    #[test]
    fn test_summarize_strategy() {
        let compressor = ContextCompressor::new(50).with_strategy(CompressionStrategy::Summarize);
        let messages = create_test_messages(10);
        let compressed = compressor.compress(&messages);

        // Should keep system + recent 4 + summary = 6
        assert!(compressed.len() <= 6);
        assert!(compressed.iter().any(|m| m.role == Role::System));
    }

    #[test]
    fn test_summarize_with_user_assistant_pairs() {
        let compressor = ContextCompressor::new(50).with_strategy(CompressionStrategy::Summarize);
        let messages = vec![
            msg(Role::System, "system"),
            msg(Role::User, "What is Rust?"),
            msg(Role::Assistant, "Rust is a systems programming language."),
            msg(Role::User, "How do I install it?"),
            msg(Role::Assistant, "Use rustup."),
            msg(Role::User, "Thanks"),
            msg(Role::Assistant, "You're welcome"),
        ];
        let compressed = compressor.compress(&messages);
        assert!(compressed.iter().any(|m| m.role == Role::System));
        // Should have a summary message inserted
        assert!(compressed
            .iter()
            .any(|m| m.name == Some("summary".to_string())));
    }

    #[test]
    fn test_summarize_empty_middle() {
        let compressor = ContextCompressor::new(50).with_strategy(CompressionStrategy::Summarize);
        let messages = vec![
            msg(Role::System, "system"),
            msg(Role::User, "hi"),
            msg(Role::Assistant, "hello"),
        ];
        let compressed = compressor.compress(&messages);
        // No middle to summarize; keep all recent
        assert_eq!(compressed.len(), 3);
    }

    // ── CompressionStats ──────────────────────────────────────────────────────

    #[test]
    fn test_compression_stats() {
        let compressor = ContextCompressor::new(100);
        let before = create_test_messages(10);
        let after = create_test_messages(5);
        let stats = compressor.stats(&before, &after);

        assert_eq!(stats.before_messages, 10);
        assert_eq!(stats.after_messages, 5);
        assert!(stats.before_tokens > stats.after_tokens);
        assert!(stats.reduction_percent > 0.0);
    }

    #[test]
    fn test_compression_stats_no_reduction() {
        let compressor = ContextCompressor::new(100);
        let messages = create_test_messages(5);
        let stats = compressor.stats(&messages, &messages);

        assert_eq!(stats.before_messages, 5);
        assert_eq!(stats.after_messages, 5);
        assert_eq!(stats.reduction_percent, 0.0);
    }

    #[test]
    fn test_compression_stats_display() {
        let compressor = ContextCompressor::new(100);
        let before = create_test_messages(10);
        let after = create_test_messages(5);
        let stats = compressor.stats(&before, &after);
        let display = format!("{}", stats);

        assert!(display.contains("Compression:"));
        assert!(display.contains("10 -> 5 messages"));
        assert!(display.contains("% reduction"));
    }

    #[test]
    fn test_compression_stats_empty() {
        let compressor = ContextCompressor::new(100);
        let stats = compressor.stats(&[], &[]);
        assert_eq!(stats.before_tokens, 0);
        assert_eq!(stats.after_tokens, 0);
        assert_eq!(stats.reduction_percent, 0.0);
    }

    // ── compact_with_llm ──────────────────────────────────────────────────────

    struct MockProvider {
        response: Option<String>,
        should_fail: bool,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn default_model(&self) -> &str {
            "mock-model"
        }

        fn supports_tools(&self) -> bool {
            false
        }

        fn max_context(&self) -> usize {
            4096
        }

        async fn complete(&self, _request: CompletionRequest) -> crate::Result<CompletionResponse> {
            if self.should_fail {
                return Err(crate::error::SyscityError::Internal("mock error".to_string()));
            }
            Ok(CompletionResponse {
                message: Message {
                    role: Role::Assistant,
                    content: self.response.clone().unwrap_or_default(),
                    content_blocks: None,
                    reasoning_content: None,
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    metadata: None,
                },
                usage: None,
                model: "mock-model".to_string(),
                finish_reason: Some("stop".to_string()),
            })
        }

        async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
            let stream = tokio_stream::iter(vec![]);
            Ok(Box::pin(stream))
        }

        async fn health_check(&self) -> crate::Result<bool> {
            Ok(true)
        }

        async fn set_credential(
            &self,
            _credential: crate::model_router::Credential,
        ) -> crate::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_compact_with_llm_too_short() {
        let compressor = ContextCompressor::new(100);
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: None,
            should_fail: false,
        });
        let messages = create_test_messages(3);
        let compacted = compressor
            .compact_with_llm(&messages, &provider, None, 2, 2)
            .await;
        assert_eq!(compacted.len(), 3);
    }

    #[tokio::test]
    async fn test_compact_with_llm_success() {
        let compressor = ContextCompressor::new(100);
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: Some("Summary of conversation".to_string()),
            should_fail: false,
        });
        let messages = create_test_messages(10);
        let compacted = compressor
            .compact_with_llm(&messages, &provider, None, 2, 2)
            .await;

        // head=2 + summary + tail=2 = 5
        assert_eq!(compacted.len(), 5);
        assert!(compacted
            .iter()
            .any(|m| m.name == Some("compaction_summary".to_string())));
    }

    #[tokio::test]
    async fn test_compact_with_llm_empty_summary() {
        let compressor = ContextCompressor::new(100);
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: Some("".to_string()),
            should_fail: false,
        });
        let messages = create_test_messages(10);
        let compacted = compressor
            .compact_with_llm(&messages, &provider, None, 2, 2)
            .await;
        // Empty summary returns original
        assert_eq!(compacted.len(), 10);
    }

    #[tokio::test]
    async fn test_compact_with_llm_error() {
        let compressor = ContextCompressor::new(100);
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: None,
            should_fail: true,
        });
        let messages = create_test_messages(10);
        let compacted = compressor
            .compact_with_llm(&messages, &provider, None, 2, 2)
            .await;
        // Error returns original
        assert_eq!(compacted.len(), 10);
    }

    #[tokio::test]
    async fn test_compact_with_llm_with_model() {
        let compressor = ContextCompressor::new(100);
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: Some("Model-specific summary".to_string()),
            should_fail: false,
        });
        let messages = create_test_messages(10);
        let compacted = compressor
            .compact_with_llm(&messages, &provider, Some("gpt-4"), 1, 1)
            .await;
        assert_eq!(compacted.len(), 3); // head=1 + summary + tail=1
    }

    #[tokio::test]
    async fn test_compact_with_llm_no_mid_section() {
        let compressor = ContextCompressor::new(100);
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: Some("Summary".to_string()),
            should_fail: false,
        });
        let messages = create_test_messages(4);
        // keep_head=2, keep_tail=2 → mid_start=2, mid_end=2 → no mid section
        let compacted = compressor
            .compact_with_llm(&messages, &provider, None, 2, 2)
            .await;
        assert_eq!(compacted.len(), 4);
    }

    #[tokio::test]
    async fn test_compact_with_llm_tail_never_starts_on_tool_result() {
        // mid_start lands exactly on a tool result; snapping must fold it back
        // into the kept head (with its tool call) so the tail starts on a user.
        let compressor = ContextCompressor::new(100);
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: Some("Summary".to_string()),
            should_fail: false,
        });

        let mut call = msg(Role::Assistant, "");
        call.tool_calls = Some(vec![]);
        let tool_result = Message {
            role: Role::Tool,
            content: "tool result".to_string(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: Some("c1".to_string()),
            metadata: None,
        };
        let messages = vec![
            msg(Role::System, "sys"),
            call,
            tool_result,
            msg(Role::User, "follow up"),
            msg(Role::Assistant, "reply"),
            msg(Role::User, "another"),
            msg(Role::Assistant, "done"),
        ];

        // keep_head=2 → mid_start=2 lands on the tool result. After snapping the
        // tail must begin at "follow up" and the tool pair must stay intact.
        let compacted = compressor
            .compact_with_llm(&messages, &provider, None, 2, 2)
            .await;

        assert!(compacted.len() < messages.len(), "should compact");
        // head(3: System, tool-call, tool-result) + summary + tail(2).
        assert_eq!(
            compacted[3].name.as_deref(),
            Some("compaction_summary"),
            "summary must follow the tool pair"
        );
        let tail_start = &compacted[4];
        assert_eq!(tail_start.role, Role::User);
        assert_eq!(tail_start.content, "another");
        // The tool call + its result both survived in the head, in order.
        let head_roles: Vec<Role> = compacted[..3].iter().map(|m| m.role).collect();
        assert_eq!(head_roles, vec![Role::System, Role::Assistant, Role::Tool]);
        assert!(compacted[1].tool_calls.is_some());
    }

    #[tokio::test]
    async fn test_compact_with_llm_cut_never_orphans_tool_call() {
        // The cut point sits on an assistant tool-call whose results follow in
        // the tail; snapping must fold the results into the mid (summarised).
        let compressor = ContextCompressor::new(100);
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: Some("Summary".to_string()),
            should_fail: false,
        });

        let mut call = msg(Role::Assistant, "");
        call.tool_calls = Some(vec![]);
        let tool_result = Message {
            role: Role::Tool,
            content: "result2".to_string(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: Some("c2".to_string()),
            metadata: None,
        };
        let messages = vec![
            msg(Role::System, "sys"),
            msg(Role::User, "u1"),
            msg(Role::Assistant, "a1"),
            msg(Role::User, "u2"),
            call,
            tool_result,
            msg(Role::User, "u3"),
        ];

        // keep_head=1, keep_tail=2 → mid_end=5 lands after the tool call; the
        // tool result at index 5 must be pulled into the mid so the tail never
        // starts with an orphaned result.
        let compacted = compressor
            .compact_with_llm(&messages, &provider, None, 1, 2)
            .await;

        assert!(compacted.len() < messages.len(), "should compact");
        let tail_start = &compacted[2]; // head(1) + summary + tail
        assert_eq!(tail_start.role, Role::User);
        assert_eq!(tail_start.content, "u3");
    }

    // ── compression quality (§三) ─────────────────────────────────────────────

    fn assert_ratio(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "retention_ratio {} != {}", actual, expected);
    }

    #[test]
    fn build_observation_computes_retention_ratio() {
        let obs = build_compression_observation(0, 200, 100, "llm_summary");
        assert_ratio(obs.retention_ratio, 0.5);
        assert_eq!(obs.freed_tokens, 100);
        // 0.5 is not below the 0.5 default -> no flag.
        assert_eq!(obs.quality_flag, None);

        let full = build_compression_observation(0, 200, 0, "llm_summary");
        assert_ratio(full.retention_ratio, 0.0);
        assert_eq!(full.quality_flag.as_deref(), Some("low_retention"));

        let none = build_compression_observation(0, 200, 200, "llm_summary");
        assert_ratio(none.retention_ratio, 1.0);
        assert_eq!(none.quality_flag, None);

        let zero = build_compression_observation(0, 0, 0, "heuristic_summary");
        assert_ratio(zero.retention_ratio, 0.0);
        assert_eq!(zero.quality_flag.as_deref(), Some("low_retention"));
        assert_eq!(zero.strategy, "heuristic_summary");
    }

    #[tokio::test]
    async fn compacted_history_yields_observation_with_quality_metrics() {
        let compressor = ContextCompressor::new(4096);
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: Some("Summary of the conversation".to_string()),
            should_fail: false,
        });
        let messages = create_test_messages(30);
        let before = compressor.estimate_tokens(&messages);
        let compacted = compressor
            .compact_with_llm(&messages, &provider, None, 2, 6)
            .await;
        let after = compressor.estimate_tokens(&compacted);

        // The compaction must have reclaimed tokens for a meaningful observation.
        assert!(after < before, "compaction should shrink the history");

        let obs = build_compression_observation(0, before, after, "llm_summary");
        assert_eq!(obs.tokens_before, before);
        assert_eq!(obs.tokens_after, after);
        assert_eq!(obs.freed_tokens, before - after);
        assert_ratio(obs.retention_ratio, after as f64 / before as f64);
        // The flag is consistent with the default 0.5 threshold.
        if obs.retention_ratio < crate::observe::record::DEFAULT_MIN_RETENTION_RATIO {
            assert_eq!(obs.quality_flag.as_deref(), Some("low_retention"));
        } else {
            assert_eq!(obs.quality_flag, None);
        }
    }
}
