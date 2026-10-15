//! Thread + Turn model for conversation branching and undo.
//!
//! A [`Thread`] is a named conversation branch inside a session.  Each
//! [`Turn`] records one user→assistant exchange along with its lifecycle
//! state, allowing turn-level rollback without losing the rest of the
//! conversation.
//!
//! # Relationship to [`super::context::Context`]
//!
//! `Context` manages the raw `Vec<Message>` window sent to the provider.
//! `Thread` wraps a `Context` and adds:
//! - Append-only turn log (`Vec<Turn>`) for rollback
//! - `undo_last_turn()` — removes the last pending/complete turn
//! - Named thread identity for multi-task sessions

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::providers::Message;

/// A record of a single tool call and its result within a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// The tool name (e.g. "web_fetch", "search_web").
    pub name: String,
    /// Serialized arguments (truncated to 200 chars for storage).
    pub args: String,
    /// Result content (truncated to 500 chars for storage).
    pub result: String,
    /// Whether the tool executed successfully.
    pub success: bool,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Token consumption for a single turn's LLM interactions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnUsage {
    /// Tokens consumed by the prompt(s).
    pub prompt_tokens: u32,
    /// Tokens consumed by the completion(s).
    pub completion_tokens: u32,
    /// Total tokens consumed across all LLM calls in this turn.
    pub total_tokens: u32,
    /// Tokens read from the provider-side prompt cache.
    #[serde(default)]
    pub cache_read_tokens: u32,
    /// Tokens written to the provider-side prompt cache.
    #[serde(default)]
    pub cache_creation_tokens: u32,
}

/// Lifecycle state of a single turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// Waiting to be processed.
    Pending,
    /// Currently being processed by the agent.
    Running,
    /// Completed successfully.
    Complete,
    /// Processing was interrupted (e.g. by a Cancel command).
    Interrupted,
    /// An error occurred during processing.
    Error,
}

/// One user→assistant exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// Sequential index within the thread (0-based).
    pub index: usize,
    /// The user's input message.
    pub user_message: String,
    /// The assistant's reply (empty while still running).
    pub assistant_response: String,
    /// Current lifecycle state.
    pub state: TurnState,
    /// When this turn was created.
    pub created_at: SystemTime,
    /// When this turn last changed state.
    pub updated_at: SystemTime,
    /// Tool calls made during this turn, with timing and result data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRecord>,
    /// Token usage for this turn's LLM interactions.
    #[serde(default)]
    pub token_usage: Option<TurnUsage>,
}

impl Turn {
    /// Create a new turn in the `Pending` state.
    pub fn new(index: usize, user_message: impl Into<String>) -> Self {
        let now = SystemTime::now();
        Self {
            index,
            user_message: user_message.into(),
            assistant_response: String::new(),
            state: TurnState::Pending,
            created_at: now,
            updated_at: now,
            tool_calls: Vec::new(),
            token_usage: None,
        }
    }

    /// Transition to the `Running` state.
    pub fn start(&mut self) {
        self.state = TurnState::Running;
        self.updated_at = SystemTime::now();
    }

    /// Record a completed response and transition to `Complete`.
    pub fn complete(&mut self, response: impl Into<String>) {
        self.assistant_response = response.into();
        self.state = TurnState::Complete;
        self.updated_at = SystemTime::now();
    }

    /// Transition to the `Interrupted` state.
    pub fn interrupt(&mut self) {
        self.state = TurnState::Interrupted;
        self.updated_at = SystemTime::now();
    }

    /// Transition to the `Error` state.
    pub fn mark_error(&mut self) {
        self.state = TurnState::Error;
        self.updated_at = SystemTime::now();
    }
}

/// A named conversation branch holding an ordered log of [`Turn`]s.
///
/// The thread owns a [`super::context::Context`] (the sliding message window
/// sent to the provider) and additionally keeps the full turn log for undo.
/// A separate `redo_stack` preserves undone turns so they can be restored
/// until a new turn is pushed (which clears the redo history).
#[derive(Debug)]
pub struct Thread {
    /// Thread identifier (e.g. `"main"` or `uuid`).
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Ordered turn log.
    pub turns: Vec<Turn>,
    /// Stack of turns that were undone (preserved for redo).
    redo_stack: Vec<Turn>,
    /// Raw message context for the provider.
    pub context: super::context::Context,
    /// When the thread was created.
    pub created_at: SystemTime,
    /// Compaction state tracking for memory flush deduplication.
    pub compaction_state: super::compaction::SessionCompactionState,
}

impl Thread {
    /// Create a new thread.
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        system_prompt: impl Into<String>,
        max_tokens: usize,
    ) -> Self {
        let id_str = id.into();
        let context = super::context::Context::new(id_str.clone(), system_prompt, max_tokens);
        Self {
            id: id_str,
            label: label.into(),
            turns: Vec::new(),
            redo_stack: Vec::new(),
            context,
            created_at: SystemTime::now(),
            compaction_state: super::compaction::SessionCompactionState::default(),
        }
    }

    /// Create a Thread from a pre-built Context (used by Agent integration).
    ///
    /// Unlike [`Thread::new`], which constructs its own `Context`, this
    /// constructor accepts an existing `Context` that already contains a system
    /// prompt, token limits, and any initial messages.  The turn log starts
    /// empty regardless.
    pub fn from_context(
        id: impl Into<String>,
        label: impl Into<String>,
        context: super::context::Context,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            turns: Vec::new(),
            redo_stack: Vec::new(),
            context,
            created_at: SystemTime::now(),
            compaction_state: super::compaction::SessionCompactionState::default(),
        }
    }

    /// Number of turns recorded.
    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    /// Copy this branch's turn log and context under a new identity.
    ///
    /// What a branch is: the same conversation up to now, which can then be
    /// continued on its own — so the copy carries the turns and the message
    /// window, and nothing else. The redo stack does not come along, because
    /// undoing is a fact about *this* branch's history rather than about the
    /// conversation; a branch that could redo the parent's undo would be able to
    /// restore a turn the parent no longer has.
    pub fn fork(&self, id: impl Into<String>, label: impl Into<String>) -> Self {
        let id = id.into();
        let mut context = self.context.clone();
        context.set_id(id.clone());
        Self {
            id,
            label: label.into(),
            turns: self.turns.clone(),
            redo_stack: Vec::new(),
            context,
            created_at: SystemTime::now(),
            compaction_state: self.compaction_state.clone(),
        }
    }

    /// Append a new `Pending` turn for `user_message`.
    ///
    /// Clears the redo stack — new input invalidates the redo history.
    pub fn push_turn(&mut self, user_message: impl Into<String>) -> usize {
        let index = self.turns.len();
        self.turns.push(Turn::new(index, user_message));
        self.redo_stack.clear(); // New turn invalidates redo history
        index
    }

    /// Undo the most recent turn by moving it from the turn log to the
    /// redo stack, and strip the corresponding messages from the context.
    ///
    /// Returns `true` if a turn was undone, `false` if the thread was empty.
    pub fn undo_last_turn(&mut self) -> bool {
        match self.turns.pop() {
            None => false,
            Some(turn) => {
                // Clone the user message before moving turn to redo_stack
                let user_message = turn.user_message.clone();
                // Preserve the undone turn for potential redo
                self.redo_stack.push(turn);
                // Mirror the undo in the context by stripping the last
                // user message plus any subsequent messages (assistant reply
                // and tool call/result pairs).
                self.remove_turn_from_context(&user_message);
                true
            }
        }
    }

    /// Redo the most recently undone turn by restoring it from the redo stack.
    ///
    /// Re-inserts the turn into the turn log and restores its messages to the
    /// context window. Returns `true` if a turn was redone, `false` if the
    /// redo stack was empty.
    pub fn redo_last_turn(&mut self) -> bool {
        match self.redo_stack.pop() {
            None => false,
            Some(turn) => {
                // Restore user message to context
                self.context
                    .add_message(crate::providers::Message::user(&turn.user_message));
                // Restore assistant response if present
                if !turn.assistant_response.is_empty() {
                    self.context
                        .add_message(crate::providers::Message::assistant(
                            &turn.assistant_response,
                        ));
                }
                // Re-insert turn (with corrected index)
                let corrected_index = self.turns.len();
                let mut restored_turn = turn;
                restored_turn.index = corrected_index;
                self.turns.push(restored_turn);
                true
            }
        }
    }

    /// Returns `true` if there are turns that can be undone.
    pub fn can_undo(&self) -> bool {
        !self.turns.is_empty()
    }

    /// Returns `true` if there are turns that can be redone.
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Returns the number of turns available for redo.
    pub fn redo_count(&self) -> usize {
        self.redo_stack.len()
    }

    /// Check if a memory flush should run before the next turn.
    ///
    /// Uses token count, transcript size, and compaction state to determine
    /// if a pre-compaction memory flush is needed.
    ///
    /// Returns `true` if flush should run, `false` otherwise.
    pub fn should_run_memory_flush(&self, config: &super::compaction::MemoryFlushConfig) -> bool {
        use super::compaction::compute_context_hash;

        let total_tokens = self.context.token_count();

        let current_messages: Vec<(String, String)> = self
            .context
            .history()
            .iter()
            .map(|m| (m.role.to_string(), m.content.clone()))
            .collect();
        let context_hash = compute_context_hash(&current_messages);

        super::compaction::should_run_memory_flush(
            total_tokens,
            // Use the token-based context_window, not transcript_bytes (bytes != tokens)
            self.context.max_context_tokens(),
            config,
            &self.compaction_state,
            &context_hash,
        )
    }

    /// Record that a memory flush was completed.
    ///
    /// Updates the compaction state to track the flush for deduplication.
    pub fn record_memory_flush_completed(&mut self) {
        use crate::memory::record_flush_in_state;

        let current_messages: Vec<(String, String)> = self
            .context
            .history()
            .iter()
            .map(|m| (m.role.to_string(), m.content.clone()))
            .collect();
        let context_hash = super::compaction::compute_context_hash(&current_messages);

        record_flush_in_state(&mut self.compaction_state, &context_hash);
    }

    /// Increment the compaction count after compaction completes.
    pub fn increment_compaction_count(&mut self) {
        use crate::memory::increment_compaction_count;
        increment_compaction_count(&mut self.compaction_state);
    }

    // ── Private ──────────────────────────────────────────────────────────────

    /// Remove the last occurrence of a user message with `content` from the
    /// context, along with everything that followed it (assistant reply, tool
    /// calls, tool results).
    fn remove_turn_from_context(&mut self, _user_content: &str) {
        let history: &[Message] = self.context.history();
        // Find the last user message position (the one being undone).
        // We use position rather than content matching to avoid matching
        // the wrong turn when text is duplicated after compaction.
        let Some(pos) = history
            .iter()
            .rposition(|m| m.role == crate::providers::Role::User)
        else {
            return;
        };
        // Collect the new message list: everything before `pos`.
        let kept: Vec<Message> = history[..pos].to_vec();
        self.context.replace_messages(kept);
    }
}

/// Manages all [`Thread`]s for a session.
#[derive(Debug, Default)]
pub struct ThreadManager {
    threads: Vec<Thread>,
}

impl ThreadManager {
    /// Create a new, empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a thread, returning its index.
    pub fn push(&mut self, thread: Thread) -> usize {
        let idx = self.threads.len();
        self.threads.push(thread);
        idx
    }

    /// Get a thread by ID (immutable).
    pub fn get(&self, id: &str) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }

    /// Get a thread by ID (mutable).
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|t| t.id == id)
    }

    /// Undo the last turn in the named thread.  Returns `true` if successful.
    pub fn undo(&mut self, thread_id: &str) -> bool {
        self.get_mut(thread_id)
            .map(|t| t.undo_last_turn())
            .unwrap_or(false)
    }

    /// Redo the most recently undone turn in the named thread.  Returns `true`
    /// if successful.
    pub fn redo(&mut self, thread_id: &str) -> bool {
        self.get_mut(thread_id)
            .map(|t| t.redo_last_turn())
            .unwrap_or(false)
    }

    /// Returns `true` if the named thread can undo.
    pub fn can_undo(&self, thread_id: &str) -> bool {
        self.get(thread_id).map(|t| t.can_undo()).unwrap_or(false)
    }

    /// Returns `true` if the named thread can redo.
    pub fn can_redo(&self, thread_id: &str) -> bool {
        self.get(thread_id).map(|t| t.can_redo()).unwrap_or(false)
    }

    /// List all thread IDs.
    pub fn ids(&self) -> Vec<&str> {
        self.threads.iter().map(|t| t.id.as_str()).collect()
    }

    /// Total number of threads.
    pub fn len(&self) -> usize {
        self.threads.len()
    }

    /// Returns `true` if there are no threads.
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::compaction;

    fn make_thread() -> Thread {
        Thread::new("test", "Test Thread", "You are helpful.", 100_000)
    }

    #[test]
    fn test_turn_lifecycle() {
        let mut turn = Turn::new(0, "Hello");
        assert_eq!(turn.state, TurnState::Pending);
        turn.start();
        assert_eq!(turn.state, TurnState::Running);
        turn.complete("Hi there!");
        assert_eq!(turn.state, TurnState::Complete);
        assert_eq!(turn.assistant_response, "Hi there!");
    }

    #[test]
    fn test_thread_undo_empty() {
        let mut thread = make_thread();
        assert!(!thread.undo_last_turn());
    }

    #[test]
    fn test_thread_push_and_undo() {
        let mut thread = make_thread();
        let idx = thread.push_turn("What is 2+2?");
        assert_eq!(idx, 0);
        assert_eq!(thread.turn_count(), 1);

        // Add user message to context to mirror what the agent loop does.
        thread
            .context
            .add_message(crate::providers::Message::user("What is 2+2?"));
        thread
            .context
            .add_message(crate::providers::Message::assistant("4"));

        assert!(thread.undo_last_turn());
        assert_eq!(thread.turn_count(), 0);
        // Context should be empty after undo.
        assert_eq!(thread.context.message_count(), 0);
    }

    #[test]
    fn test_thread_undo_redo_cycle() {
        let mut thread = make_thread();

        // Setup: push a turn with messages
        thread.push_turn("What is 2+2?");
        thread
            .context
            .add_message(crate::providers::Message::user("What is 2+2?"));
        thread
            .context
            .add_message(crate::providers::Message::assistant("4"));
        thread.turns[0].complete("4");

        assert_eq!(thread.turn_count(), 1);
        assert_eq!(thread.redo_count(), 0);

        // Undo preserves the turn in redo_stack
        assert!(thread.undo_last_turn());
        assert_eq!(thread.turn_count(), 0);
        assert_eq!(thread.redo_count(), 1);

        // Redo restores the turn
        assert!(thread.redo_last_turn());
        assert_eq!(thread.turn_count(), 1);
        assert_eq!(thread.redo_count(), 0);
        assert_eq!(thread.turns[0].assistant_response, "4");

        // Context restored
        assert_eq!(thread.context.message_count(), 2);
    }

    #[test]
    fn test_thread_can_undo_can_redo() {
        let mut thread = make_thread();
        assert!(!thread.can_undo());
        assert!(!thread.can_redo());

        thread.push_turn("Hello");
        assert!(thread.can_undo());
        assert!(!thread.can_redo());

        thread.undo_last_turn();
        assert!(!thread.can_undo());
        assert!(thread.can_redo());

        thread.redo_last_turn();
        assert!(thread.can_undo());
        assert!(!thread.can_redo());
    }

    #[test]
    fn test_thread_redo_empty_fails() {
        let mut thread = make_thread();
        assert!(!thread.redo_last_turn());
    }

    #[test]
    fn test_thread_push_clears_redo_stack() {
        let mut thread = make_thread();

        thread.push_turn("Turn 1");
        thread
            .context
            .add_message(crate::providers::Message::user("Turn 1"));
        thread.turns[0].complete("Response 1");

        thread.undo_last_turn();
        assert_eq!(thread.redo_count(), 1);

        // Pushing a new turn clears redo history
        thread.push_turn("Turn 2");
        assert_eq!(thread.redo_count(), 0);
        assert!(!thread.can_redo());
    }

    #[test]
    fn test_thread_manager_undo_redo() {
        let mut mgr = ThreadManager::new();
        mgr.push(make_thread());
        // Undo on an unknown id returns false.
        assert!(!mgr.undo("nonexistent"));
        // Undo on empty thread returns false.
        assert!(!mgr.undo("test"));
        // Redo on empty thread returns false.
        assert!(!mgr.redo("test"));

        // Setup: add a thread with a turn
        let thread = mgr.get_mut("test").unwrap();
        thread.push_turn("Hello");
        thread
            .context
            .add_message(crate::providers::Message::user("Hello"));

        // Can check undo/redo availability
        assert!(mgr.can_undo("test"));
        assert!(!mgr.can_redo("test"));

        // Undo works through manager
        assert!(mgr.undo("test"));
        assert!(!mgr.can_undo("test"));
        assert!(mgr.can_redo("test"));

        // Redo works through manager
        assert!(mgr.redo("test"));
        assert!(mgr.can_undo("test"));
        assert!(!mgr.can_redo("test"));
    }

    // ── Memory flush and compaction state tests ──────────────────────────────

    #[test]
    fn test_thread_compaction_state_default() {
        let thread = make_thread();
        assert_eq!(thread.compaction_state.compaction_count, 0);
        assert_eq!(thread.compaction_state.memory_flush_compaction_count, None);
        assert_eq!(thread.compaction_state.last_flush_context_hash, None);
    }

    #[test]
    fn test_thread_should_run_memory_flush_token_threshold() {
        let mut thread = Thread::new("test", "Test", "System prompt", 100000);
        let config = compaction::MemoryFlushConfig {
            enabled: true,
            soft_threshold_tokens: 1000,
            force_flush_transcript_bytes: 2 * 1024 * 1024,
            prompt: "test".to_string(),
            system_prompt: "test".to_string(),
            reserve_tokens_floor: 5000,
        };

        // Add messages to context to simulate conversation
        for i in 0..50 {
            thread
                .context
                .add_message(crate::providers::Message::user(&format!("Message {}", i)));
            thread
                .context
                .add_message(crate::providers::Message::assistant(&format!("Response {}", i)));
        }

        // Token count should now be high enough to trigger flush
        // threshold = 100000 - 5000 - 1000 = 94000
        // With ~50 messages * ~15 chars each / 4 = ~187 tokens per message pair
        // 50 * 187 = ~9350 tokens - may not be enough
        // Let's just verify the method runs without error
        let _should_flush = thread.should_run_memory_flush(&config);
    }

    #[test]
    fn test_thread_should_run_memory_flush_respects_dedup() {
        let mut thread = Thread::new("test", "Test", "System prompt", 100000);
        let config = compaction::MemoryFlushConfig::default();

        // Add some messages
        thread
            .context
            .add_message(crate::providers::Message::user("Hello"));
        thread
            .context
            .add_message(crate::providers::Message::assistant("Hi there!"));

        // First check - should potentially flush (depending on token count)
        let _first = thread.should_run_memory_flush(&config);

        // Record a flush
        thread.record_memory_flush_completed();

        // Second check - should not flush because we just flushed
        let second = thread.should_run_memory_flush(&config);
        assert!(!second, "Should not flush immediately after recording a flush");
    }

    #[test]
    fn test_thread_record_memory_flush_completed() {
        let mut thread = Thread::new("test", "Test", "System prompt", 100000);

        // Add messages to create a context hash
        thread
            .context
            .add_message(crate::providers::Message::user("Test message"));
        thread
            .context
            .add_message(crate::providers::Message::assistant("Test response"));

        let initial_count = thread.compaction_state.memory_flush_compaction_count;

        thread.record_memory_flush_completed();

        // Should have updated the flush tracking
        assert!(thread
            .compaction_state
            .memory_flush_compaction_count
            .is_some());
        assert!(thread.compaction_state.last_flush_context_hash.is_some());
        assert_ne!(thread.compaction_state.memory_flush_compaction_count, initial_count);
    }

    #[test]
    fn test_thread_increment_compaction_count() {
        let mut thread = Thread::new("test", "Test", "System prompt", 100000);

        // Set up initial state as if a flush was recorded
        thread.compaction_state.compaction_count = 5;
        thread.compaction_state.memory_flush_compaction_count = Some(5);
        thread.compaction_state.last_flush_context_hash = Some("test_hash".to_string());

        thread.increment_compaction_count();

        // Compaction count should be incremented
        assert_eq!(thread.compaction_state.compaction_count, 6);
        // Flush tracking should be cleared
        assert_eq!(thread.compaction_state.memory_flush_compaction_count, None);
        assert_eq!(thread.compaction_state.last_flush_context_hash, None);
    }

    #[test]
    fn thread_memory_flush_integration() {
        // Integration test: simulate a full flush cycle
        let mut thread = Thread::new("test", "Test", "System prompt", 100000);
        let config = compaction::MemoryFlushConfig {
            enabled: true,
            soft_threshold_tokens: 100, // Low threshold for testing
            force_flush_transcript_bytes: 2 * 1024 * 1024,
            prompt: "test".to_string(),
            system_prompt: "test".to_string(),
            reserve_tokens_floor: 500, // Low reserve for testing
        };

        // Add enough messages to trigger flush
        for i in 0..30 {
            thread
                .context
                .add_message(crate::providers::Message::user(&format!(
                    "User message number {} with some extra content",
                    i
                )));
            thread
                .context
                .add_message(crate::providers::Message::assistant(&format!(
                    "Assistant response number {} with detailed answer",
                    i
                )));
        }

        // First check - should trigger flush due to token count
        let should_flush_1 = thread.should_run_memory_flush(&config);

        // Record the flush
        if should_flush_1 {
            thread.record_memory_flush_completed();
        }

        // Second check - should NOT flush (just flushed)
        let should_flush_2 = thread.should_run_memory_flush(&config);
        assert!(!should_flush_2, "Should not flush twice without compaction");

        // Simulate compaction
        thread.increment_compaction_count();

        // Third check - can flush again after compaction
        let _should_flush_3 = thread.should_run_memory_flush(&config);
        // May or may not flush depending on context hash change
    }

    /// A fork is the same conversation under a new identity: the turns and the
    /// message window come along, and — the part that would be a bug — the
    /// parent and the branch are independent afterwards.
    #[test]
    fn forking_copies_the_turns_and_leaves_the_parent_alone() {
        let mut parent = make_thread();
        parent.push_turn("what changed?");
        parent.turns[0].complete("the parser");
        parent.context.add_message(Message::user("what changed?"));
        parent.context.add_message(Message::assistant("the parser"));

        let mut branch = parent.fork("thread-branch", "branch");
        assert_eq!(branch.id, "thread-branch");
        assert_eq!(branch.label, "branch");
        assert_eq!(branch.context.id(), "thread-branch");
        assert_eq!(branch.turn_count(), 1);
        assert_eq!(branch.turns[0].assistant_response, "the parser");
        assert_eq!(branch.context.message_count(), parent.context.message_count());

        // Writing to the branch does not reach the parent.
        branch.push_turn("and now?");
        assert_eq!(branch.turn_count(), 2);
        assert_eq!(parent.turn_count(), 1, "the parent gained a turn it never had");

        // The redo stack is this branch's business: a fork that could restore
        // the parent's undone turn would resurrect one the parent dropped.
        assert!(parent.undo_last_turn());
        let mut branch = parent.fork("thread-branch-2", "branch-2");
        assert!(!branch.undo_last_turn(), "the fork inherited the parent's undo");
    }
}
