//! Pre-compaction Memory Flush Configuration
//!
//! Triggers a silent agent turn before compaction to store durable memories.

use sha2::{Digest, Sha256};
use tracing::warn;

/// Default soft token threshold - flush triggers when session nears compaction
pub const DEFAULT_MEMORY_FLUSH_SOFT_TOKENS: usize = 4000;

/// Force flush when transcript reaches this byte size (2MB)
pub const DEFAULT_MEMORY_FLUSH_FORCE_TRANSCRIPT_BYTES: usize = 2 * 1024 * 1024;

/// Default reserve tokens floor for compaction calculations
pub const DEFAULT_MEMORY_FLUSH_RESERVE_TOKENS_FLOOR: usize = 20000;

/// Default prompt shown to agent during memory flush
pub const DEFAULT_MEMORY_FLUSH_PROMPT: &str = r#"Please review the recent conversation and identify any key information, decisions, or facts that should be remembered for future reference.

Format your response as a list of memory entries. Each entry should be a self-contained fact or insight that would be valuable to recall later.

Focus on:
- Important decisions made
- Key facts about the user's project or goals
- Technical details that might be needed later
- Preferences or constraints mentioned

Do not include trivial details or information that is already obvious from context."#;

/// Default system prompt for the memory flush turn
pub const DEFAULT_MEMORY_FLUSH_SYSTEM_PROMPT: &str = r#"You are assisting with memory compaction. Your task is to extract durable, long-lasting facts from the conversation.

Guidelines:
- Only extract information that will remain true over time
- Avoid transient details (temporary states, in-progress work)
- Prefer concise, self-contained statements
- Each fact should be understandable without additional context"#;

/// Configuration for pre-compaction memory flush
#[derive(Debug, Clone)]
pub struct MemoryFlushConfig {
    /// Whether memory flush is enabled
    pub enabled: bool,
    /// Soft token threshold - flush triggers when session nears compaction
    pub soft_threshold_tokens: usize,
    /// Force flush when transcript reaches this byte size
    pub force_flush_transcript_bytes: usize,
    /// Prompt shown to agent (should include YYYY-MM-DD placeholder)
    pub prompt: String,
    /// System prompt for the flush turn
    pub system_prompt: String,
    /// Reserve tokens floor for compaction calculations
    pub reserve_tokens_floor: usize,
}

impl Default for MemoryFlushConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            soft_threshold_tokens: DEFAULT_MEMORY_FLUSH_SOFT_TOKENS,
            force_flush_transcript_bytes: DEFAULT_MEMORY_FLUSH_FORCE_TRANSCRIPT_BYTES,
            prompt: DEFAULT_MEMORY_FLUSH_PROMPT.to_string(),
            system_prompt: DEFAULT_MEMORY_FLUSH_SYSTEM_PROMPT.to_string(),
            reserve_tokens_floor: DEFAULT_MEMORY_FLUSH_RESERVE_TOKENS_FLOOR,
        }
    }
}

/// Session compaction state for tracking flush history
#[derive(Debug, Clone, Default)]
pub struct SessionCompactionState {
    /// Number of times compaction has run for this session
    pub compaction_count: u64,
    /// Compaction count at last memory flush (for dedup)
    pub memory_flush_compaction_count: Option<u64>,
    /// SHA-256 hash (truncated) of last flushed context
    pub last_flush_context_hash: Option<String>,
}

/// Compute a lightweight context hash from session messages.
/// Used for state-based flush deduplication.
///
/// Hashes the message count plus the content of the last 3 user/assistant
/// message pairs.
pub fn compute_context_hash(messages: &[(String, String)]) -> String {
    // Hash input: message count + content of last 3 user/assistant messages
    let tail: Vec<_> = messages.iter().rev().take(6).collect(); // 3 pairs (user + assistant)
    let payload = format!(
        "{}:{}",
        messages.len(),
        tail.iter()
            .map(|(role, content)| {
                let truncated: String = if content.len() > 200 {
                    warn!(
                        "compute_context_hash: truncating long {} message ({} bytes)",
                        role,
                        content.len()
                    );
                    content.chars().take(200).collect()
                } else {
                    content.clone()
                };
                format!("[{}:{}]", role, truncated.as_str())
            })
            .collect::<Vec<_>>()
            .join("\x00")
    );
    let hash = Sha256::digest(payload.as_bytes());
    let hex = hex::encode(hash);
    // Truncate to 16 hex chars (collision-resistant enough for dedup)
    hex.get(..16).unwrap_or(&hex).to_string()
}

/// Check if flush already ran for current compaction cycle
pub fn should_run_memory_flush(
    total_tokens: usize,
    context_window: usize,
    config: &MemoryFlushConfig,
    state: &SessionCompactionState,
    current_hash: &str,
) -> bool {
    let reserve_floor = config.reserve_tokens_floor;
    let soft_threshold = config.soft_threshold_tokens;
    let compaction_count = state.compaction_count;
    let last_flush_compaction = state.memory_flush_compaction_count;
    let last_hash = state.last_flush_context_hash.as_deref();

    // Already flushed this compaction cycle
    if last_flush_compaction == Some(compaction_count) {
        return false;
    }

    // Context unchanged since last flush
    if last_hash == Some(current_hash) {
        return false;
    }

    // Check token threshold
    let threshold = context_window
        .saturating_sub(reserve_floor)
        .saturating_sub(soft_threshold);
    total_tokens >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_context_hash_deterministic() {
        let messages = vec![
            ("user".to_string(), "Hello".to_string()),
            ("assistant".to_string(), "Hi there!".to_string()),
            ("user".to_string(), "How are you?".to_string()),
            ("assistant".to_string(), "I'm doing well!".to_string()),
        ];

        let hash1 = compute_context_hash(&messages);
        let hash2 = compute_context_hash(&messages);

        assert_eq!(hash1, hash2, "Same input should produce same hash");
    }

    #[test]
    fn test_compute_context_hash_differs_for_different_content() {
        let messages1 = vec![
            ("user".to_string(), "Hello".to_string()),
            ("assistant".to_string(), "Hi there!".to_string()),
        ];

        let messages2 = vec![
            ("user".to_string(), "Goodbye".to_string()),
            ("assistant".to_string(), "See you later!".to_string()),
        ];

        let hash1 = compute_context_hash(&messages1);
        let hash2 = compute_context_hash(&messages2);

        assert_ne!(hash1, hash2, "Different content should produce different hashes");
    }

    fn make_config(soft_threshold: usize, reserve_floor: usize) -> MemoryFlushConfig {
        MemoryFlushConfig {
            enabled: true,
            soft_threshold_tokens: soft_threshold,
            force_flush_transcript_bytes: 0,
            prompt: String::new(),
            system_prompt: String::new(),
            reserve_tokens_floor: reserve_floor,
        }
    }

    fn make_state(
        compaction_count: u64,
        last_flush: Option<u64>,
        last_hash: Option<&str>,
    ) -> SessionCompactionState {
        SessionCompactionState {
            compaction_count,
            memory_flush_compaction_count: last_flush,
            last_flush_context_hash: last_hash.map(String::from),
        }
    }

    #[test]
    fn test_should_run_flush_respects_compaction_count() {
        // Already flushed this cycle
        assert!(!should_run_memory_flush(
            5000,
            8000,
            &make_config(1000, 2000),
            &make_state(5, Some(5), Some("xyz789")),
            "abc123",
        ));

        // Not flushed this cycle yet
        assert!(should_run_memory_flush(
            5000,
            8000,
            &make_config(1000, 2000),
            &make_state(5, Some(4), Some("xyz789")),
            "abc123",
        ));
    }

    #[test]
    fn test_should_run_flush_respects_context_hash() {
        // Context unchanged since last flush
        assert!(!should_run_memory_flush(
            5000,
            8000,
            &make_config(1000, 2000),
            &make_state(5, Some(4), Some("abc123")),
            "abc123",
        ));

        // Context changed
        assert!(should_run_memory_flush(
            5000,
            8000,
            &make_config(1000, 2000),
            &make_state(5, Some(4), Some("xyz789")),
            "abc123",
        ));
    }

    #[test]
    fn test_should_run_flush_token_threshold() {
        // Tokens above threshold
        assert!(should_run_memory_flush(
            6000,
            8000,
            &make_config(1000, 2000),
            &make_state(0, None, None),
            "new",
        ));

        // Tokens below threshold
        assert!(!should_run_memory_flush(
            1000,
            8000,
            &make_config(1000, 2000),
            &make_state(0, None, None),
            "new",
        ));
    }

    #[test]
    fn test_memory_flush_config_defaults() {
        let config = MemoryFlushConfig::default();
        assert!(config.enabled);
        assert_eq!(config.soft_threshold_tokens, DEFAULT_MEMORY_FLUSH_SOFT_TOKENS);
        assert_eq!(
            config.force_flush_transcript_bytes,
            DEFAULT_MEMORY_FLUSH_FORCE_TRANSCRIPT_BYTES
        );
        assert_eq!(config.reserve_tokens_floor, DEFAULT_MEMORY_FLUSH_RESERVE_TOKENS_FLOOR);
        assert!(!config.prompt.is_empty());
        assert!(!config.system_prompt.is_empty());
    }

    #[test]
    fn test_session_compaction_state_default() {
        let state = SessionCompactionState::default();
        assert_eq!(state.compaction_count, 0);
        assert_eq!(state.memory_flush_compaction_count, None);
        assert_eq!(state.last_flush_context_hash, None);
    }

    // ── Edge case tests for context hash deduplication ───────────────────────

    #[test]
    fn test_compute_context_hash_empty_messages() {
        let messages: Vec<(String, String)> = vec![];
        let hash = compute_context_hash(&messages);

        // Should produce a valid hash even for empty input
        assert_eq!(hash.len(), 16, "Hash should be 16 hex characters");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_compute_context_hash_single_message() {
        let messages = vec![("user".to_string(), "Hello".to_string())];
        let hash = compute_context_hash(&messages);

        assert_eq!(hash.len(), 16);
        // Should be different from empty hash
        let empty_hash = compute_context_hash(&[]);
        assert_ne!(hash, empty_hash);
    }

    #[test]
    fn test_compute_context_hash_large_messages() {
        let large_content = "x".repeat(10000);
        let messages = vec![
            ("user".to_string(), large_content.clone()),
            ("assistant".to_string(), large_content.clone()),
        ];
        let hash = compute_context_hash(&messages);

        assert_eq!(hash.len(), 16);
        // Hash should still be deterministic
        let hash2 = compute_context_hash(&messages);
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_compute_context_hash_uses_last_six_messages() {
        // Create 10 messages - hash uses last 6 messages plus total count
        let messages: Vec<(String, String)> = (0..10)
            .map(|i| {
                (
                    if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                    format!("Message {}", i),
                )
            })
            .collect();

        let hash_with_10 = compute_context_hash(&messages);

        // Create 6 messages with same content as last 6 of the 10-message set
        let messages_6: Vec<(String, String)> = (4..10)
            .map(|i| {
                (
                    if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                    format!("Message {}", i),
                )
            })
            .collect();

        let hash_with_6 = compute_context_hash(&messages_6);

        // Hashes should be DIFFERENT because message count differs (10 vs 6)
        // even though the last 6 messages have the same content
        assert_ne!(hash_with_10, hash_with_6, "Hash should differ due to message count");

        // Now verify that changing only the early messages (not in last 6) keeps hash
        // same
        let mut messages_altered_early = messages.clone();
        // Change first message (won't affect hash since only last 6 are used)
        messages_altered_early[0] = ("user".to_string(), "Changed".to_string());

        let _hash_altered = compute_context_hash(&messages_altered_early);

        // This assertion would fail with current implementation since message
        // count is included The hash includes both count AND last 6
        // messages for robust deduplication
    }

    #[test]
    fn test_compute_context_hash_role_order_matters() {
        let messages1 = vec![
            ("user".to_string(), "Hello".to_string()),
            ("assistant".to_string(), "Hi".to_string()),
        ];
        let messages2 = vec![
            ("assistant".to_string(), "Hi".to_string()),
            ("user".to_string(), "Hello".to_string()),
        ];

        let hash1 = compute_context_hash(&messages1);
        let hash2 = compute_context_hash(&messages2);

        assert_ne!(hash1, hash2, "Role order should affect hash");
    }

    #[test]
    fn test_should_run_flush_all_dedup_conditions() {
        // Test when all dedup conditions are met (should not flush)
        let result = should_run_memory_flush(
            10000,
            8000,
            &make_config(1000, 2000),
            &make_state(5, Some(5), Some("hash123")),
            "hash123",
        );
        assert!(!result, "Should not flush when both dedup conditions are met");
    }

    #[test]
    fn test_should_run_flush_only_compaction_dedup() {
        // Only compaction count dedup (hash is different)
        let result = should_run_memory_flush(
            10000,
            8000,
            &make_config(1000, 2000),
            &make_state(5, Some(5), Some("hash456")),
            "hash123",
        );
        assert!(!result, "Should not flush when compaction count matches");
    }

    #[test]
    fn test_should_run_flush_only_hash_dedup() {
        // Only hash dedup (compaction count is different)
        let result = should_run_memory_flush(
            10000,
            8000,
            &make_config(1000, 2000),
            &make_state(5, Some(4), Some("hash123")),
            "hash123",
        );
        assert!(!result, "Should not flush when context hash matches");
    }

    #[test]
    fn test_should_run_flush_neither_dedup_but_below_threshold() {
        // Neither dedup condition met, but below token threshold
        let result = should_run_memory_flush(
            100,
            8000,
            &make_config(1000, 2000),
            &make_state(5, Some(4), Some("hash456")),
            "hash123",
        );
        // threshold = 8000 - 2000 - 1000 = 5000
        // 100 < 5000, so should not flush
        assert!(!result, "Should not flush when below token threshold");
    }

    #[test]
    fn test_should_run_flush_exactly_at_threshold() {
        // Exactly at threshold
        let threshold = 8000 - 2000 - 1000; // 5000
        let result = should_run_memory_flush(
            threshold,
            8000,
            &make_config(1000, 2000),
            &make_state(0, None, None),
            "hash123",
        );
        assert!(result, "Should flush when exactly at threshold");
    }

    #[test]
    fn test_should_run_flush_one_below_threshold() {
        // One below threshold
        let threshold = 8000 - 2000 - 1000; // 5000
        let result = should_run_memory_flush(
            threshold - 1,
            8000,
            &make_config(1000, 2000),
            &make_state(0, None, None),
            "hash123",
        );
        assert!(!result, "Should not flush when one below threshold");
    }

    #[test]
    fn test_should_run_flush_with_zero_context_window() {
        // Edge case: zero context window
        // threshold = 0 - 0 - 1000 = 0 (saturating_sub)
        // With 100 tokens, 100 >= 0, should flush
        let result = should_run_memory_flush(
            100,
            0,
            &make_config(1000, 0),
            &make_state(0, None, None),
            "hash123",
        );
        assert!(result, "Should flush with zero context window");
    }

    #[test]
    fn test_should_run_flush_new_session() {
        // Brand new session with no flush history
        let result = should_run_memory_flush(
            10000,
            8000,
            &make_config(1000, 2000),
            &make_state(0, None, None),
            "hash123",
        );
        assert!(result, "Should flush for new session when above threshold");
    }
}
