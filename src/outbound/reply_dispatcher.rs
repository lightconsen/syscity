//! Reply Dispatcher
//!
//! Routes agent responses back to the correct channel endpoint.
//! This replaces the ad-hoc `channel.send_message()` calls scattered
//! through the agent loop with a unified dispatch layer.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{debug, error, info};

use crate::channels::{Channel, OutgoingMessage};

/// Configuration for reply dispatch.
#[derive(Debug, Clone)]
pub struct ReplyDispatchConfig {
    /// Whether to split long messages into chunks.
    pub chunk_long_messages: bool,
    /// Maximum message length before chunking.
    pub max_chunk_length: usize,
    /// Whether to suppress empty replies.
    pub suppress_empty: bool,
}

impl Default for ReplyDispatchConfig {
    fn default() -> Self {
        Self {
            chunk_long_messages: true,
            max_chunk_length: 4000,
            suppress_empty: true,
        }
    }
}

/// Reply dispatcher routes outbound messages to channels.
pub struct ReplyDispatcher {
    config: ReplyDispatchConfig,
    /// channel_name -> Channel handle
    channels: RwLock<HashMap<String, Arc<dyn Channel>>>,
}

impl ReplyDispatcher {
    pub fn new(config: ReplyDispatchConfig) -> Self {
        Self {
            config,
            channels: RwLock::new(HashMap::new()),
        }
    }

    /// Register a channel for dispatch.
    pub async fn register_channel(&self, name: &str, channel: Arc<dyn Channel>) {
        let mut channels = self.channels.write().await;
        channels.insert(name.to_string(), channel);
        info!("Registered channel '{}' for reply dispatch", name);
    }

    /// Remove a previously registered channel.
    pub async fn unregister_channel(&self, name: &str) {
        let mut channels = self.channels.write().await;
        channels.remove(name);
        info!("Unregistered channel '{}' from reply dispatch", name);
    }

    /// Dispatch an outgoing message to its target channel.
    /// Long messages are split into chunks at word boundaries.
    pub async fn dispatch(
        &self,
        channel_name: &str,
        message: OutgoingMessage,
    ) -> Result<(), ReplyDispatchError> {
        if self.config.suppress_empty && message.content.trim().is_empty() {
            debug!("Suppressing empty reply to {}", channel_name);
            return Ok(());
        }

        let channels = self.channels.read().await;
        let channel = channels
            .get(channel_name)
            .ok_or_else(|| ReplyDispatchError::ChannelNotFound(channel_name.to_string()))?;

        let chunks = if self.config.chunk_long_messages
            && message.content.len() > self.config.max_chunk_length
        {
            chunk_content(&message.content, self.config.max_chunk_length)
        } else {
            vec![message.content.clone()]
        };

        for content in chunks {
            let msg = OutgoingMessage { content, ..message.clone() };
            debug!(
                "Dispatching reply to channel {} (conversation {})",
                channel_name, msg.conversation_id.0
            );
            if let Err(e) = channel.send(msg).await {
                error!("Failed to dispatch reply to {}: {}", channel_name, e);
                return Err(ReplyDispatchError::SendFailed(e.to_string()));
            }
        }
        Ok(())
    }

    /// List registered channels.
    pub async fn list_channels(&self) -> Vec<String> {
        let channels = self.channels.read().await;
        channels.keys().cloned().collect()
    }
}

/// Errors from the reply dispatcher.
#[derive(Debug, thiserror::Error)]
pub enum ReplyDispatchError {
    #[error("Channel not found: {0}")]
    ChannelNotFound(String),
    #[error("Failed to send message: {0}")]
    SendFailed(String),
}

/// The largest index `≤ max_len` that is a character boundary in `s`.
///
/// `str` slicing panics on a non-boundary index, so every byte offset used to
/// cut a string has to come through here first.
fn floor_char_boundary(s: &str, max_len: usize) -> usize {
    if max_len >= s.len() {
        return s.len();
    }
    let mut idx = max_len;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Split content into chunks at word boundaries, each at most `max_len` bytes.
///
/// Each chunk ends at a space character when possible to avoid splitting words.
/// If a single word exceeds `max_len`, it is hard-split at the limit.
fn chunk_content(content: &str, max_len: usize) -> Vec<String> {
    if max_len == 0 {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = content;

    while remaining.len() > max_len {
        // `max_len` is a byte budget, but a chunk may only be cut where a
        // character ends — slicing at an arbitrary byte panics on the first
        // multi-byte character that straddles the limit (CJK, emoji), which is
        // exactly the content this is most likely to see.
        let limit = floor_char_boundary(remaining, max_len);
        let split_at = if limit == 0 {
            // The limit is narrower than a single character: emit that one
            // character whole rather than looping without progress.
            remaining
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(remaining.len())
        } else {
            // Prefer the last word boundary inside the limit.
            let slice = &remaining[..limit];
            slice.rfind(' ').map(|pos| pos + 1).unwrap_or(limit)
        };
        chunks.push(remaining[..split_at].to_string());
        remaining = remaining[split_at..].trim_start();
    }

    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{ConversationId, MessageOptions, OutgoingMessage};

    fn dummy_msg(content: impl Into<String>) -> OutgoingMessage {
        OutgoingMessage {
            conversation_id: ConversationId::new("test"),
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            formatted_content: None,
            attachments: vec![],
            reply_to: None,
            options: MessageOptions {
                silent: false,
                show_typing: false,
                custom: std::collections::HashMap::new(),
            },
            usage: None,
        }
    }

    // ── chunk_content tests ─────────────────────────────────────────────

    #[test]
    fn test_chunk_content_no_split() {
        let chunks = chunk_content("hello world", 100);
        assert_eq!(chunks, vec!["hello world"]);
    }

    #[test]
    fn test_chunk_content_word_boundary() {
        // "hello worl"[..10] = "hello worl", rfind(' ') = 5 → "hello " + "world foo
        // bar"
        let chunks = chunk_content("hello world foo bar", 10);
        assert_eq!(chunks, vec!["hello ", "world foo ", "bar"]);
    }

    #[test]
    fn test_chunk_content_no_space() {
        let chunks = chunk_content("abcdefghijklmnop", 5);
        assert_eq!(chunks, vec!["abcde", "fghij", "klmno", "p"]);
    }

    #[test]
    fn test_chunk_content_empty() {
        let chunks = chunk_content("", 10);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_chunk_content_exact_fit() {
        let chunks = chunk_content("12345", 5);
        assert_eq!(chunks, vec!["12345"]);
    }

    #[test]
    fn test_chunk_content_max_len_zero() {
        let chunks = chunk_content("hello", 0);
        assert_eq!(chunks, vec!["hello"]);
    }

    /// A byte limit that lands mid-character must not panic — CJK and emoji are
    /// where this bites, and they are the common case for this project.
    #[test]
    fn test_chunk_content_multibyte_boundary_does_not_panic() {
        // Each of these is 3 bytes; a 5-byte limit lands inside the second one.
        let text = "中文中文中文";
        let chunks = chunk_content(text, 5);
        assert_eq!(chunks.concat(), text, "no character may be lost or split");
        assert!(chunks.len() > 1, "the text is longer than the limit");
    }

    #[test]
    fn test_chunk_content_emoji_boundary_does_not_panic() {
        // 4-byte characters.
        let text = "🎉🎉🎉";
        let chunks = chunk_content(text, 6);
        assert_eq!(chunks.concat(), text);
    }

    /// A limit narrower than a single character would otherwise loop without
    /// making progress; the character is emitted whole instead.
    #[test]
    fn test_chunk_content_limit_smaller_than_one_character() {
        let chunks = chunk_content("中文", 1);
        assert_eq!(chunks.concat(), "中文");
    }

    /// Mixed content with spaces still breaks on words where it can.
    #[test]
    fn test_chunk_content_multibyte_word_boundary() {
        let chunks = chunk_content("中文 测试 内容", 7);
        assert_eq!(chunks.concat().replace(' ', ""), "中文测试内容");
    }

    #[test]
    fn test_chunk_content_trim_remainder() {
        let chunks = chunk_content("12345 7890", 5);
        // "12345"[..5].rfind(' ') = None → split_at = 5 → "12345" + " 7890"
        // trim_start → "7890"
        assert_eq!(chunks, vec!["12345", "7890"]);
    }

    // ── dispatcher tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_dispatch_empty_suppressed() {
        let dispatcher = ReplyDispatcher::new(ReplyDispatchConfig::default());
        let msg = dummy_msg("   ");
        // No channel registered, but empty should be suppressed first.
        let result = dispatcher.dispatch("test", msg).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_channel_not_found() {
        let dispatcher = ReplyDispatcher::new(ReplyDispatchConfig::default());
        let msg = dummy_msg("hello");
        let result = dispatcher.dispatch("missing", msg).await;
        assert!(matches!(result, Err(ReplyDispatchError::ChannelNotFound(_))));
    }

    #[tokio::test]
    async fn test_list_channels() {
        let dispatcher = ReplyDispatcher::new(ReplyDispatchConfig::default());
        assert!(dispatcher.list_channels().await.is_empty());
    }

    #[tokio::test]
    async fn test_config_default() {
        let config = ReplyDispatchConfig::default();
        assert!(config.chunk_long_messages);
        assert_eq!(config.max_chunk_length, 4000);
        assert!(config.suppress_empty);
    }

    #[tokio::test]
    async fn test_dispatch_empty_not_suppressed() {
        let mut config = ReplyDispatchConfig::default();
        config.suppress_empty = false;
        let dispatcher = ReplyDispatcher::new(config);
        let msg = dummy_msg("   ");
        let result = dispatcher.dispatch("test", msg).await;
        assert!(matches!(result, Err(ReplyDispatchError::ChannelNotFound(_))));
    }
}
