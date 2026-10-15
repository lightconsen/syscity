//! Conversation context management for the Agent
//!
//! Context handles message history, token counting, and pruning
//! to keep conversations within the context window.

use std::collections::HashSet;
use std::time::SystemTime;

use crate::agent::turns::{ToolCallRecord, TurnUsage};
use crate::delegation::DelegationScope;
use crate::providers::Message;

/// Conversation context
#[derive(Debug, Clone)]
pub struct Context {
    /// Conversation ID
    id: String,
    /// System prompt
    system_prompt: String,
    /// Message history (excluding system prompt)
    messages: Vec<Message>,
    /// Maximum tokens allowed
    max_tokens: usize,
    /// Approximate token count
    token_count: usize,
    /// When the context was created
    created_at: SystemTime,
    /// When the context was last accessed
    last_accessed: SystemTime,
    /// Tool call iteration counter (to prevent infinite loops)
    tool_iterations: usize,
    /// Maximum allowed tool iterations
    max_tool_iterations: usize,
    /// Track tool calls to prevent duplicates (tool_name + params_hash)
    executed_tool_calls: HashSet<String>,
    /// Track tool names used in this turn for cache tagging
    tools_used: Vec<String>,
    /// Optional hard cap on the number of turns kept in history.
    /// When set, `add_message` enforces this limit by dropping the oldest
    /// user/assistant pairs before the token-based prune runs.
    max_turns: Option<usize>,
    /// Accumulated tool call records for the current turn.
    /// Transferred to [`Turn::tool_calls`] when the turn completes.
    pub tool_call_records: Vec<ToolCallRecord>,
    /// Accumulated token usage for the current turn.
    /// Transferred to [`Turn::token_usage`] when the turn completes.
    pub turn_token_usage: Option<TurnUsage>,
    /// Active delegation scope, when this conversation runs inside a
    /// delegated child agent.  `None` for ordinary conversations.
    delegation: Option<DelegationScope>,
}

/// Marker name on the framework-injected state-snapshot user message.
pub(crate) const STATE_SNAPSHOT_NAME: &str = "state_snapshot";

/// Build the current-state snapshot user message appended to every request.
///
/// Kept minimal (calendar date / weekday / timezone offset): the model can
/// query the exact current time via the `time` tool. The message is transient
/// — it appears only in the `to_messages()` output, never in history, so it
/// does not pollute persistence, compaction, or undo, and the system-prompt
/// prefix stays byte-stable across threads (provider KV-cache reuse).
fn state_snapshot_message() -> Message {
    let now = chrono::Local::now();
    Message::user_named(
        STATE_SNAPSHOT_NAME,
        format!(
            "[state snapshot] Today is {} ({}), timezone UTC{}. \
             For the exact current time, use the time tool.",
            now.format("%Y-%m-%d"),
            now.format("%A"),
            now.format("%z"),
        ),
    )
}

impl Context {
    /// Default maximum tool iterations to prevent infinite loops
    pub const DEFAULT_MAX_TOOL_ITERATIONS: usize = 10;

    /// Create a new context
    pub fn new(id: impl Into<String>, system_prompt: impl Into<String>, max_tokens: usize) -> Self {
        let now = SystemTime::now();
        Self {
            id: id.into(),
            system_prompt: system_prompt.into(),
            messages: Vec::new(),
            max_tokens,
            token_count: 0,
            created_at: now,
            last_accessed: now,
            tool_iterations: 0,
            max_tool_iterations: Self::DEFAULT_MAX_TOOL_ITERATIONS,
            executed_tool_calls: HashSet::new(),
            tools_used: Vec::new(),
            max_turns: None,
            tool_call_records: Vec::new(),
            turn_token_usage: None,
            delegation: None,
        }
    }

    /// Attach a delegation scope to this context.
    pub fn with_delegation(mut self, scope: DelegationScope) -> Self {
        self.delegation = Some(scope);
        self
    }

    /// Set (or clear) the delegation scope on this context.
    pub fn set_delegation(&mut self, scope: Option<DelegationScope>) {
        self.delegation = scope;
    }

    /// The active delegation scope, if any.
    pub fn delegation(&self) -> Option<&DelegationScope> {
        self.delegation.as_ref()
    }

    /// Whether the named tool may be executed in this context.
    ///
    /// With no active delegation scope every tool is allowed; inside a
    /// delegated child the scope's allowlist is enforced.
    pub fn is_tool_allowed(&self, name: &str) -> bool {
        match &self.delegation {
            None => true,
            Some(scope) => scope.is_tool_allowed(name),
        }
    }

    /// Set a hard cap on conversation turns (user+assistant pairs).
    ///
    /// When the history grows beyond `turns` pairs the oldest pair is dropped
    /// before the normal token-based pruning runs.  A "turn" is counted as one
    /// user message (tool messages are not counted as separate turns).
    pub fn with_max_turns(mut self, turns: usize) -> Self {
        self.max_turns = Some(turns);
        self
    }

    /// Enforce the turn limit by dropping oldest messages until the
    /// user-message count is within `max_turns`.  Tool (result) messages
    /// paired with an assistant message are removed together to keep the
    /// conversation coherent.
    pub fn limit_turns(&mut self) {
        let Some(max) = self.max_turns else { return };

        loop {
            // Count user-role messages as turn markers.
            let user_count = self
                .messages
                .iter()
                .filter(|m| m.role == crate::providers::Role::User)
                .count();

            if user_count <= max {
                break;
            }

            // Find and remove the oldest user message plus any immediately
            // following tool/assistant messages that belong to that turn.
            if let Some(oldest_user) = self
                .messages
                .iter()
                .position(|m| m.role == crate::providers::Role::User)
            {
                let removed = self.messages.remove(oldest_user);
                self.token_count = self.token_count.saturating_sub(removed.content.len() / 4);

                // Also remove the assistant reply that follows (if present).
                if oldest_user < self.messages.len() {
                    let next = &self.messages[oldest_user];
                    if next.role == crate::providers::Role::Assistant {
                        let removed = self.messages.remove(oldest_user);
                        self.token_count =
                            self.token_count.saturating_sub(removed.content.len() / 4);
                    }
                }
            } else {
                break;
            }
        }
    }

    /// Check if a tool call with these parameters was already executed
    pub fn is_tool_call_duplicate(&self, tool_name: &str, params: &str) -> bool {
        let key = format!("{}:{}", tool_name, params);
        self.executed_tool_calls.contains(&key)
    }

    /// Record a tool call as executed
    pub fn record_tool_call(&mut self, tool_name: &str, params: &str) {
        let key = format!("{}:{}", tool_name, params);
        self.executed_tool_calls.insert(key);
        // Also track unique tool names for cache tagging
        if !self.tools_used.contains(&tool_name.to_string()) {
            self.tools_used.push(tool_name.to_string());
        }
    }

    /// Get the list of tool names used in this turn
    pub fn tools_used(&self) -> &[String] {
        &self.tools_used
    }

    /// Clear the tools_used list (call at start of new turn)
    pub fn clear_tools_used(&mut self) {
        self.tools_used.clear();
    }

    /// Forget all executed-tool-call history.
    ///
    /// Called after a filesystem-mutating tool succeeds: the on-disk state has
    /// changed, so an earlier identical `shell`/`file_read`/`web_fetch` call may
    /// now legitimately be re-run (e.g. edit a script, then re-run the same test
    /// command). Whole-conversation dedup would otherwise block that re-run as a
    /// "duplicate".
    pub fn clear_executed_tool_calls(&mut self) {
        self.executed_tool_calls.clear();
    }

    /// Record a tool call and its result in the turn's accumulator.
    pub fn push_tool_call_record(&mut self, record: ToolCallRecord) {
        self.tool_call_records.push(record);
    }

    /// Take all accumulated tool call records (leaving the accumulator empty).
    pub fn take_tool_call_records(&mut self) -> Vec<ToolCallRecord> {
        std::mem::take(&mut self.tool_call_records)
    }

    /// Set the accumulated token usage for this turn.
    pub fn set_turn_token_usage(&mut self, usage: TurnUsage) {
        self.turn_token_usage = Some(usage);
    }

    /// Accumulate token counts into the turn's usage tracker.
    pub fn accumulate_turn_token_usage(&mut self, usage: &crate::providers::Usage) {
        match self.turn_token_usage {
            Some(ref mut acc) => {
                acc.prompt_tokens += usage.prompt_tokens;
                acc.completion_tokens += usage.completion_tokens;
                acc.total_tokens += usage.total_tokens;
                acc.cache_read_tokens += usage.cache_read_tokens;
                acc.cache_creation_tokens += usage.cache_creation_tokens;
            }
            None => {
                self.turn_token_usage = Some(TurnUsage {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_creation_tokens: usage.cache_creation_tokens,
                });
            }
        }
    }

    /// Take the accumulated token usage (leaving it as None).
    pub fn take_turn_token_usage(&mut self) -> Option<TurnUsage> {
        self.turn_token_usage.take()
    }

    /// Increment tool iteration counter
    /// Returns false if limit reached
    pub fn increment_tool_iteration(&mut self) -> bool {
        self.tool_iterations += 1;
        self.tool_iterations < self.max_tool_iterations
    }

    /// Calculate dynamic tool limit based on task complexity
    /// Can be overridden by SYSCITY_MAX_TOOL_ITERATIONS env var
    pub fn calculate_dynamic_limit(message_content: &str) -> usize {
        // Check for env var override first
        if let Ok(limit) = std::env::var("SYSCITY_MAX_TOOL_ITERATIONS") {
            if let Ok(parsed) = limit.parse::<usize>() {
                return parsed;
            }
        }

        // Base limit: 10
        let base_limit = 10;

        // Scale based on message complexity indicators
        let mut complexity = 0;

        // Longer queries may need more steps
        if message_content.len() > 200 {
            complexity += 5;
        }

        // Multi-part tasks (indicated by "and", "then", commas)
        let parts = message_content.split([',', ';']).count();
        complexity += parts.saturating_sub(1) * 2;

        // Tasks with explicit multiple steps
        if message_content.to_lowercase().contains("steps")
            || message_content.to_lowercase().contains("step by step")
        {
            complexity += 5;
        }

        // Cap at reasonable maximum (30)
        std::cmp::min(30, base_limit + complexity)
    }

    /// Get current tool iteration count
    pub fn tool_iterations(&self) -> usize {
        self.tool_iterations
    }

    /// Check if tool iteration limit is reached
    pub fn is_tool_limit_reached(&self) -> bool {
        self.tool_iterations >= self.max_tool_iterations
    }

    /// Set maximum tool iterations
    pub fn set_max_tool_iterations(&mut self, max: usize) {
        self.max_tool_iterations = max;
    }

    /// Get the current tool-iteration budget (actual, not the constant default).
    pub fn max_tool_iterations(&self) -> usize {
        self.max_tool_iterations
    }

    /// Get the context ID
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Rename the context — used when a thread is forked, since the copy holds
    /// the same conversation under a different identity.
    pub fn set_id(&mut self, id: impl Into<String>) {
        self.id = id.into();
    }

    /// Get the system prompt
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Add a message to the context
    pub fn add_message(&mut self, message: Message) {
        // Estimate tokens (rough approximation: 4 chars per token)
        let estimated_tokens = message.content.len() / 4;
        self.token_count += estimated_tokens;
        self.messages.push(message);
        self.last_accessed = SystemTime::now();

        // Enforce turn limit before token-based pruning.
        self.limit_turns();

        // Prune if necessary
        self.prune_if_needed();
    }

    /// Add multiple messages atomically, then prune once.
    ///
    /// Use this when adding a logical group of messages (e.g. an assistant
    /// message with tool_calls followed by its tool results) to prevent
    /// intermediate pruning from removing the assistant before its results
    /// are added — the prune step sees all messages in the group and can
    /// correctly determine which tool_calls have matching results.
    pub fn add_batch(&mut self, messages: Vec<Message>) {
        if messages.is_empty() {
            return;
        }
        for msg in &messages {
            self.token_count += msg.content.len() / 4;
        }
        self.messages.extend(messages);
        self.last_accessed = SystemTime::now();
        self.limit_turns();
        self.prune_if_needed();
    }

    /// Get all messages including system prompt
    pub fn to_messages(&self) -> Vec<Message> {
        let mut result = Vec::with_capacity(self.messages.len() + 2);
        result.push(Message::system(&self.system_prompt));
        result.extend(self.messages.iter().cloned());
        result.push(state_snapshot_message());
        result
    }

    /// Get message history (excluding system)
    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    /// Clear the conversation history (keep system)
    pub fn clear(&mut self) {
        self.messages.clear();
        self.token_count = 0;
        self.last_accessed = SystemTime::now();
    }

    /// Get the approximate token count
    pub fn token_count(&self) -> usize {
        // Include system prompt
        self.token_count + (self.system_prompt.len() / 4)
    }

    /// Get the maximum tokens allowed
    pub fn max_context_tokens(&self) -> usize {
        self.max_tokens
    }

    /// Check if context needs pruning
    pub fn needs_pruning(&self) -> bool {
        self.token_count() > self.max_tokens
    }

    /// Prune messages to fit within token limit
    /// Note: This preserves tool call pairs (assistant message with tool_use +
    /// tool result)
    fn prune_if_needed(&mut self) {
        // Collect tool call IDs that have corresponding tool results
        // These must not be pruned or the API will error
        let pending_tool_call_ids: std::collections::HashSet<String> = self
            .messages
            .iter()
            .filter(|m| m.role == crate::providers::Role::Tool)
            .filter_map(|m| m.tool_call_id.clone())
            .collect();

        // Keep pruning until we're under the limit
        while self.token_count() > self.max_tokens && self.messages.len() > 1 {
            // First try to prune complete tool call groups (assistant + all tool results)
            if let Some((start, count)) = self.find_prunable_tool_group(&pending_tool_call_ids) {
                for _ in 0..count {
                    let removed = self.messages.remove(start);
                    self.token_count = self.token_count.saturating_sub(removed.content.len() / 4);
                }
            } else if let Some(index) = self.find_prunable_message(&pending_tool_call_ids) {
                let removed = self.messages.remove(index);
                self.token_count = self.token_count.saturating_sub(removed.content.len() / 4);
            } else {
                // Can't prune anything safely, break to avoid infinite loop
                break;
            }
        }
    }

    /// Find a complete tool call group (assistant message with tool calls + all
    /// its tool results) that can be pruned as a unit.
    ///
    /// Returns `(start_index, message_count)` if a complete group is found.
    fn find_prunable_tool_group(
        &self,
        pending_tool_call_ids: &std::collections::HashSet<String>,
    ) -> Option<(usize, usize)> {
        for (index, msg) in self.messages.iter().enumerate() {
            // Only consider assistant messages with tool calls
            if msg.role != crate::providers::Role::Assistant {
                continue;
            }
            let Some(ref tool_calls) = msg.tool_calls else {
                continue;
            };
            if tool_calls.is_empty() {
                continue;
            }

            // Check if ALL tool calls in this group have results (no pending ones)
            let has_pending = tool_calls
                .iter()
                .any(|tc| pending_tool_call_ids.contains(&tc.id));
            if has_pending {
                continue;
            }

            // Count how many messages belong to this tool group (assistant + tool results)
            let mut group_size = 1; // starts with the assistant message
            let tool_call_ids: std::collections::HashSet<String> =
                tool_calls.iter().map(|tc| tc.id.clone()).collect();

            for subsequent in self.messages.iter().skip(index + 1) {
                if subsequent.role == crate::providers::Role::Tool {
                    if let Some(ref tid) = subsequent.tool_call_id {
                        if tool_call_ids.contains(tid) {
                            group_size += 1;
                            continue;
                        }
                    }
                }
                // Stop at the next non-tool message or unrelated tool message
                break;
            }

            // Don't prune if this would remove all messages
            if group_size >= self.messages.len() {
                continue;
            }

            return Some((index, group_size));
        }

        None
    }

    /// Find the index of a message that can be safely pruned
    /// Returns None if no message can be pruned (all are protected)
    fn find_prunable_message(
        &self,
        pending_tool_call_ids: &std::collections::HashSet<String>,
    ) -> Option<usize> {
        // Try to find the oldest message that is not a protected tool call pair
        for (index, msg) in self.messages.iter().enumerate() {
            // Never prune the last message
            if index == self.messages.len() - 1 {
                return None;
            }

            // Never prune the first user message when there are tool calls in the
            // conversation — without it the LLM loses the original request and
            // conversation continuity breaks.  For simple text-only conversations
            // (no tool calls) this protection is not needed.
            let has_tool_calls = self.messages.iter().any(|m| m.tool_calls.is_some());
            if index == 0 && msg.role == crate::providers::Role::User && has_tool_calls {
                continue;
            }

            // Check if this is an assistant message with tool calls
            if msg.role == crate::providers::Role::Assistant {
                if let Some(ref tool_calls) = msg.tool_calls {
                    // Check if any of these tool calls have pending results
                    let has_pending_results = tool_calls
                        .iter()
                        .any(|tc| pending_tool_call_ids.contains(&tc.id));

                    if has_pending_results {
                        // Can't prune this message - it has pending tool results
                        continue;
                    }
                }
            }

            // Check if this is a tool result message
            if msg.role == crate::providers::Role::Tool {
                // Can't prune tool results without also pruning the assistant message
                // that made the call (which we check above). For now, keep tool results.
                // A more sophisticated approach would prune both together.
                continue;
            }

            // This message can be pruned
            return Some(index);
        }

        None
    }

    /// Replace the message history with a compacted set (e.g. after
    /// LLM-assisted compaction).
    ///
    /// The system prompt is stored separately and is unaffected.  Token count
    /// is recalculated from the new message list.
    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        self.recalculate_tokens();
        self.last_accessed = SystemTime::now();
    }

    /// Recalculate token count from scratch
    fn recalculate_tokens(&mut self) {
        self.token_count = self.messages.iter().map(|m| m.content.len() / 4).sum();
    }

    /// Get the last message
    pub fn last_message(&self) -> Option<&Message> {
        self.messages.last()
    }

    /// Get when the context was last accessed
    pub fn last_accessed(&self) -> SystemTime {
        self.last_accessed
    }

    /// Get the age of the context
    pub fn age(&self) -> std::time::Duration {
        self.created_at
            .elapsed()
            .unwrap_or(std::time::Duration::from_secs(0))
    }

    /// Check if the context is stale (inactive for too long)
    pub fn is_stale(&self, max_age: std::time::Duration) -> bool {
        self.last_accessed
            .elapsed()
            .map(|elapsed| elapsed > max_age)
            .unwrap_or(true)
    }

    /// Get the number of messages
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Summarize the conversation (for very long contexts)
    ///
    /// Keeps the first `keep_first` and last `keep_last` messages; replaces
    /// the middle section with a heuristic summary that preserves key content.
    /// For LLM-quality summarization use `ContextCompressor::compact_with_llm`.
    pub fn summarize(&mut self) {
        const KEEP_FIRST: usize = 2;
        const KEEP_LAST: usize = 4;

        if self.messages.len() <= KEEP_FIRST + KEEP_LAST + 1 {
            return; // Nothing to summarize
        }

        let mut middle_start = KEEP_FIRST;
        let mut middle_end = self.messages.len() - KEEP_LAST;

        if middle_end <= middle_start {
            return;
        }

        // Tool-pair boundary safety: the kept tail must never start on a tool
        // result (its assistant tool-call would be summarized away) and the cut
        // point must never leave an assistant tool-call without its results.
        while middle_start < middle_end
            && self.messages[middle_start].role == crate::providers::Role::Tool
        {
            middle_start += 1;
        }
        while middle_end < self.messages.len()
            && self.messages[middle_end].role == crate::providers::Role::Tool
        {
            middle_end += 1;
        }
        if middle_end <= middle_start {
            return;
        }

        // Extract middle messages
        let middle: Vec<Message> = self.messages.drain(middle_start..middle_end).collect();

        // Build a concise heuristic summary that preserves key turns
        let mut turns: Vec<String> = Vec::new();
        let mut i = 0;
        while i < middle.len() {
            let msg = &middle[i];
            if msg.content.is_empty() {
                i += 1;
                continue;
            }
            // Emit a compact turn line: role + first 200 chars
            let preview: String = msg.content.chars().take(200).collect();
            let ellipsis = if msg.content.len() > 200 { "…" } else { "" };
            turns.push(format!("{}: {}{}", msg.role, preview, ellipsis));
            i += 1;
        }

        let summary_content =
            format!("[Summary of {} earlier messages]\n{}", middle.len(), turns.join("\n"));

        // Name the summary message so callers can locate the boundary that the
        // durable compaction record anchors on (`compaction_summary` matches
        // the LLM path in compact_with_llm).
        let mut summary = Message::system(summary_content);
        summary.name = Some("compaction_summary".to_string());
        self.messages.insert(middle_start, summary);
        self.recalculate_tokens();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_creation() {
        let ctx = Context::new("test-123", "You are helpful.", 4096);
        assert_eq!(ctx.id(), "test-123");
        assert_eq!(ctx.system_prompt(), "You are helpful.");
        assert_eq!(ctx.message_count(), 0);
    }

    #[test]
    fn test_add_message() {
        let mut ctx = Context::new("test", "System", 1000);
        ctx.add_message(Message::user("Hello"));
        assert_eq!(ctx.message_count(), 1);
        assert!(ctx.last_message().is_some());
    }

    #[test]
    fn test_to_messages() {
        use crate::providers::Role;
        let mut ctx = Context::new("test", "System prompt", 1000);
        ctx.add_message(Message::user("Hello"));
        ctx.add_message(Message::assistant("Hi!"));

        let messages = ctx.to_messages();
        assert_eq!(messages.len(), 4); // System + user + assistant + snapshot
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[3].role, Role::User);
        assert_eq!(messages[3].name.as_deref(), Some(STATE_SNAPSHOT_NAME));
    }

    #[test]
    fn test_state_snapshot_message() {
        let msg = state_snapshot_message();
        assert_eq!(msg.role, crate::providers::Role::User);
        assert_eq!(msg.name.as_deref(), Some(STATE_SNAPSHOT_NAME));
        assert!(
            msg.content
                .contains(&chrono::Local::now().format("%Y-%m-%d").to_string()),
            "snapshot carries today's date: {}",
            msg.content
        );
        assert!(msg.content.contains("time tool"), "hints at the time tool");
    }

    #[test]
    fn test_to_messages_snapshot_after_history() {
        let mut ctx = Context::new("test", "System prompt", 1000);
        ctx.add_message(Message::user("Hello"));
        let messages = ctx.to_messages();
        assert_eq!(messages[1].role, crate::providers::Role::User);
        assert_eq!(messages[1].content, "Hello");
        assert_eq!(messages[2].name.as_deref(), Some(STATE_SNAPSHOT_NAME));
    }

    #[test]
    fn test_clear() {
        let mut ctx = Context::new("test", "System", 1000);
        ctx.add_message(Message::user("Hello"));
        ctx.clear();
        assert_eq!(ctx.message_count(), 0);
    }

    #[test]
    fn test_pruning() {
        // Create context with very small token limit to force pruning
        let mut ctx = Context::new("test", "S", 10); // ~2 tokens for system

        // Add many messages to trigger pruning
        for i in 0..20 {
            ctx.add_message(Message::user(format!("Message {} with some content", i)));
        }

        // Should have pruned to keep under limit
        assert!(ctx.token_count() <= ctx.max_tokens);
        assert!(ctx.message_count() < 20);
    }

    #[test]
    fn test_max_turns_limits_history() {
        let mut ctx = Context::new("test", "System", 100_000).with_max_turns(2);

        // Add 3 user+assistant pairs.
        for i in 0..3 {
            ctx.add_message(Message::user(format!("User {}", i)));
            ctx.add_message(Message::assistant(format!("Assistant {}", i)));
        }

        // Only the 2 most recent user messages should remain.
        let user_count = ctx
            .history()
            .iter()
            .filter(|m| m.role == crate::providers::Role::User)
            .count();
        assert_eq!(user_count, 2);
    }

    #[test]
    fn test_max_turns_none_no_drop() {
        let mut ctx = Context::new("test", "System", 100_000);
        // No max_turns set — all messages kept.
        for i in 0..5 {
            ctx.add_message(Message::user(format!("User {}", i)));
            ctx.add_message(Message::assistant(format!("Assistant {}", i)));
        }
        assert_eq!(ctx.message_count(), 10);
    }

    #[test]
    fn test_summarize() {
        let mut ctx = Context::new("test", "System", 10000);

        // Add many messages
        for i in 0..15 {
            ctx.add_message(Message::user(format!("Message {}", i)));
            ctx.add_message(Message::assistant(format!("Response {}", i)));
        }

        let before_count = ctx.message_count();
        ctx.summarize();

        // Should have fewer messages after summarization
        assert!(ctx.message_count() < before_count);
    }

    #[test]
    fn test_summarize_keeps_tool_pair_and_names_summary() {
        let mut ctx = Context::new("test", "System", 10000);

        let mut call = Message::assistant("");
        call.tool_calls = Some(vec![]);
        ctx.add_message(Message::user("u0"));
        ctx.add_message(call);
        ctx.add_message(Message {
            role: crate::providers::Role::Tool,
            content: "result0".to_string(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: Some("c1".to_string()),
            metadata: None,
        });
        // Enough messages that KEEP_FIRST(2) lands exactly on the tool result.
        for i in 1..6 {
            ctx.add_message(Message::user(format!("u{}", i)));
            ctx.add_message(Message::assistant(format!("a{}", i)));
        }

        ctx.summarize();

        // Summary message is named so the durable-boundary finder can locate it.
        let summary_idx = ctx
            .history()
            .iter()
            .position(|m| m.name.as_deref() == Some("compaction_summary"))
            .expect("summary message present");
        // The tool result must sit before the summary with its call, never cut.
        let roles: Vec<crate::providers::Role> = ctx.history().iter().map(|m| m.role).collect();
        assert_eq!(roles[summary_idx - 1], crate::providers::Role::Tool);
        assert_eq!(roles[summary_idx - 2], crate::providers::Role::Assistant);
        // The message right after the summary is not a tool result.
        assert_ne!(roles[summary_idx + 1], crate::providers::Role::Tool);
    }

    #[test]
    fn test_context_large_input_prunes_safely() {
        // Simulate a 100k-character input (~25k tokens) against a small limit.
        // The context must not panic. Pruning protects the last message, so
        // we add a second small message to trigger pruning of the first.
        let mut ctx = Context::new("test", "System", 100);
        let huge = "x".repeat(100_000);
        ctx.add_message(Message::user(huge));
        ctx.add_message(Message::assistant("ack"));

        assert!(ctx.token_count() <= ctx.max_tokens);
        assert!(ctx.message_count() <= 2); // pruned aggressively
    }

    #[test]
    fn test_tool_iteration_limit_blocks_infinite_loop() {
        let mut ctx = Context::new("test", "System", 1000);
        ctx.set_max_tool_iterations(3);

        assert!(ctx.increment_tool_iteration()); // 1
        assert!(ctx.increment_tool_iteration()); // 2
        assert!(!ctx.increment_tool_iteration()); // 3 -> limit reached
        assert!(ctx.is_tool_limit_reached());
        assert_eq!(ctx.tool_iterations(), 3);
    }

    #[test]
    fn test_duplicate_tool_call_detection() {
        let mut ctx = Context::new("test", "System", 1000);
        assert!(!ctx.is_tool_call_duplicate("file_read", "{\"path\":\"/tmp/test\"}"));
        ctx.record_tool_call("file_read", "{\"path\":\"/tmp/test\"}");
        assert!(ctx.is_tool_call_duplicate("file_read", "{\"path\":\"/tmp/test\"}"));
        assert!(!ctx.is_tool_call_duplicate("file_read", "{\"path\":\"/tmp/other\"}"));
    }

    #[test]
    fn test_is_tool_allowed_no_scope_allows_all() {
        let ctx = Context::new("test", "System", 1000);
        assert!(ctx.is_tool_allowed("file_read"));
        assert!(ctx.is_tool_allowed("delegate"));
        assert!(ctx.delegation().is_none());
    }

    #[test]
    fn test_is_tool_allowed_with_scope_filters() {
        use crate::delegation::DelegationScope;

        let scope = DelegationScope {
            allowed_tools: Some(vec!["file_read".to_string()]),
            ..DelegationScope::new("root", "task", 1, 3)
        };
        let ctx = Context::new("test", "System", 1000).with_delegation(scope);
        assert!(ctx.is_tool_allowed("file_read"));
        assert!(!ctx.is_tool_allowed("file_write"));
        assert!(!ctx.is_tool_allowed("delegate"));
        assert!(ctx.delegation().is_some());
    }

    #[test]
    fn test_delegate_allowed_for_interior_node_but_not_leaf() {
        use crate::delegation::DelegationScope;

        // root → manager → worker → leaf with max_depth 3. Interior nodes may
        // delegate; the leaf may not — even with an open allowlist.
        let manager = Context::new("m", "System", 1000)
            .with_delegation(DelegationScope::new("root", "manager", 1, 3));
        assert!(manager.is_tool_allowed("delegate"));

        let leaf = Context::new("l", "System", 1000)
            .with_delegation(DelegationScope::new("root", "leaf", 3, 3));
        assert!(!leaf.is_tool_allowed("delegate"));

        // Hard-blocked tools stay blocked regardless of depth.
        assert!(!manager.is_tool_allowed("execute_code"));
        assert!(!leaf.is_tool_allowed("send_message"));
    }
}
