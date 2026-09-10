//! Channel abstractions for Syscity
//!
//! Channels are communication interfaces through which users interact
//! with the AI assistant (CLI, Telegram, Discord, Slack, etc.).
// INVARIANTS-NONE: transport adapters; delivery/ordering guarantees are owned by the external channel providers.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};

use crate::core::models::Id;

pub mod formatter;
pub mod health;
pub mod lifecycle;
pub mod metrics;
pub mod state;

/// Channel types supported by Syscity
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelType {
    /// WhatsApp via Meta Business API
    Whatsapp,
    /// Telegram Bot API
    Telegram,
    /// Feishu/Lark Open API
    Feishu,
    /// QQ via go-cqhttp
    Qq,
    /// Discord Gateway
    Discord,
    /// Slack Socket Mode
    Slack,
    /// Custom WebSocket endpoint
    Websocket,
    /// Web terminal (built-in)
    WebTerminal,
    /// Signal via signal-cli daemon
    Signal,
    /// iMessage via BlueBubbles
    Imessage,
    /// WebChat browser interface
    Webchat,
    /// WeChat Official Account (公众号) webhook
    WechatMp,
}

#[cfg(feature = "telegram")]
pub mod telegram;

#[cfg(feature = "discord")]
pub mod discord;

#[cfg(feature = "slack")]
pub mod slack;

#[cfg(feature = "whatsapp")]
pub mod whatsapp;

#[cfg(feature = "qq")]
pub mod qq;

#[cfg(feature = "feishu")]
pub mod lark;

#[cfg(feature = "plugins")]
pub mod plugin_host;

#[cfg(feature = "signal")]
pub mod signal;

#[cfg(feature = "imessage")]
pub mod imessage;

#[cfg(feature = "webchat")]
pub mod webchat;

#[cfg(feature = "wechatmp")]
pub mod wechatmp;

pub mod acp_bridge;
pub mod authorization;
pub mod command_gate;
pub mod envelope;
pub mod extension;
pub mod identity;
pub mod reply_prefix;
pub mod resolver;
pub mod snapshot;
pub mod thread_binding;

#[cfg(feature = "telegram")]
pub mod telegram_extension;

pub use acp_bridge::{
    parse_acp_command, AcpCommandRequest, AcpForwardResult, ChannelAcpBinding, ChannelAcpBridge,
};
pub use command_gate::{
    parse_command, AccessGroup, AuthContext, Authorizer, AuthorizerMode, CommandGate,
    CommandGateConfig, GateResult,
};
pub use envelope::{SessionEnvelopeContext, SessionEnvelopeManager};
pub use extension::{
    ChannelExtension, ChannelExtensionConfig, ChannelExtensionRegistry, ChannelSenderBridge,
};
pub use formatter::{
    DiscordFormatter, MessageFormatter, PlainTextFormatter, SlackFormatter, TelegramHtmlFormatter,
};
pub use identity::{
    discord_identity, slack_identity, telegram_identity, IdentityValidationError,
    IdentityValidator, IdentityValidatorConfig, SenderIdentity,
};
pub use reply_prefix::{
    cost_aware_template, minimal_model_template, model_tag_template, timestamp_model_template,
    ReplyPrefixEngine, ReplyPrefixTemplate, TemplateContext,
};
pub use resolver::{
    resolve_conversation, ArtifactBindingProvider, CommandProvider, ConversationResolution,
    ConversationResolver, FallbackProvider, FocusedBindingProvider, InboundProvider,
    PluginBindingProvider, ResolutionProvider, ResolutionSource,
};
pub use snapshot::{
    error_snapshot, healthy_snapshot, muted_snapshot, warning_snapshot, AccountSnapshot,
    AccountSnapshotStore, DisplayTone,
};
#[cfg(feature = "telegram")]
pub use telegram_extension::TelegramChannelExtension;
pub use thread_binding::{
    acp_policy, branching_policy, strict_policy, PlacementDecision, PlacementHint, SpawnTarget,
    ThreadBindingManager, ThreadBindingPolicy, TrackedThreadBinding,
};

// ── Input Provenance
// ──────────────────────────────────────────────────────────

/// Describes the origin of an incoming message.
///
/// Provenance lets the agent apply different trust levels and policies
/// depending on where the message came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputProvenance {
    /// Message sent by a human user over an external channel (Telegram,
    /// Discord, …).
    ExternalUser {
        /// The channel type (e.g. `"telegram"`, `"discord"`).
        channel: String,
        /// Whether this was a direct message (not a group/room).
        is_direct: bool,
    },
    /// Message injected from another agent session (inter-agent communication).
    InterSession {
        /// The source session ID.
        source_session: String,
    },
    /// Message generated internally by the system (scheduled tasks, hooks, …).
    InternalSystem {
        /// A label identifying the internal source (e.g. `"cron"`,
        /// `"webhook"`).
        source: String,
    },
}

impl InputProvenance {
    /// Return `true` if the message originated from a human external user.
    pub fn is_external(&self) -> bool {
        matches!(self, InputProvenance::ExternalUser { .. })
    }

    /// Return `true` if the message is a direct message (not a group).
    pub fn is_direct(&self) -> bool {
        matches!(self, InputProvenance::ExternalUser { is_direct: true, .. })
    }

    /// Return `true` if the message is from an internal system source.
    pub fn is_internal(&self) -> bool {
        matches!(self, InputProvenance::InternalSystem { .. })
    }
}

impl Default for InputProvenance {
    fn default() -> Self {
        InputProvenance::ExternalUser {
            channel: "unknown".to_string(),
            is_direct: false,
        }
    }
}

// ── Mention Gating
// ────────────────────────────────────────────────────────────

/// Mention state for a message received in a group/room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionState {
    /// The message was sent in a direct conversation (mention not required).
    DirectMessage,
    /// The bot was mentioned in this group message.
    Mentioned,
    /// The bot was not mentioned in this group message.
    NotMentioned,
}

impl MentionState {
    /// Return `true` if the message should be processed (either DM or explicit
    /// mention).
    pub fn should_process(&self, require_mention: bool) -> bool {
        match self {
            MentionState::DirectMessage => true,
            MentionState::Mentioned => true,
            MentionState::NotMentioned => !require_mention,
        }
    }
}

// ── User identifier
// ───────────────────────────────────────────────────────────

/// A user identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserId(pub String);

impl UserId {
    /// Create a new user ID
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A conversation/session identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationId(pub String);

impl ConversationId {
    /// Create a new conversation ID
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Generate a new unique conversation ID
    pub fn generate() -> Self {
        Self(crate::core::models::Id::new().to_string())
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Metadata about a message
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageMetadata {
    /// When the message was sent
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Channel-specific metadata
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl MessageMetadata {
    /// Create new metadata with current timestamp
    pub fn new() -> Self {
        Self {
            timestamp: chrono::Utc::now(),
            extra: HashMap::new(),
        }
    }

    /// Add extra metadata
    pub fn with_extra(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }

    /// Mark this message as containing a detected command.
    pub fn with_detected_command(
        mut self,
        result: &crate::tools::command_detector::CommandDetectionResult,
    ) -> Self {
        self.extra.insert(
            "detected_command".to_string(),
            serde_json::json!({
                "layer": format!("{:?}", result.layer),
                "command": result.command,
                "args": result.args,
                "raw_match": result.raw_match,
            }),
        );
        self
    }

    /// Attach an authorization context to this message's metadata.
    pub fn with_auth_context(mut self, ctx: &crate::channels::AuthContext) -> Self {
        match serde_json::to_value(ctx) {
            Ok(value) => {
                self.extra.insert("auth_context".to_string(), value);
            }
            Err(e) => {
                tracing::warn!("Failed to serialize AuthContext: {}", e);
            }
        }
        self
    }
}

/// Entity chips attached by a web-composer client to a single user message:
/// skill names and agent ids the user explicitly referenced.
///
/// Chips ride [`MessageMetadata::extra`] under the `"mentions"` key so the
/// whole pipeline (chat.rs → inbound stages → router) forwards them without
/// any signature changes; only [`AgentDispatch`](crate::gateway::dispatch)
/// re-builds the message and re-inserts the key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mentions {
    /// Skill names to load via the `skill` tool before answering.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Agent ids to consult via the `delegate` tool.
    #[serde(default)]
    pub agents: Vec<String>,
}

impl Mentions {
    /// `true` when neither list carries an entry.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.agents.is_empty()
    }
}

/// Parse the `mentions` key out of a message's extra metadata.
///
/// Returns `None` when the key is absent, malformed, or deserializes to an
/// empty [`Mentions`].
pub fn mentions_from_extra(extra: &HashMap<String, serde_json::Value>) -> Option<Mentions> {
    extra
        .get("mentions")
        .and_then(|v| serde_json::from_value::<Mentions>(v.clone()).ok())
        .filter(|m| !m.is_empty())
}

/// Hidden per-turn context block appended to the model-facing user message.
///
/// Returns `None` when the mentions are empty; empty sections are omitted.
/// The block is appended at context-add time only — it never reaches
/// persistence, the transcript, or the response-cache eligibility check.
pub fn format_mentions_block(m: &Mentions) -> Option<String> {
    if m.is_empty() {
        return None;
    }

    // Dedup while preserving the client's order.
    fn dedup(items: &[String]) -> Vec<&str> {
        let mut seen = std::collections::HashSet::new();
        items
            .iter()
            .map(String::as_str)
            .filter(|s| seen.insert(*s))
            .collect()
    }

    let mut block = String::from(
        "<attached-mentions>\n\
         The user attached the following to this message (this note is not visible \
         to the user; do not quote it):\n",
    );

    let skills = dedup(&m.skills);
    if !skills.is_empty() {
        block.push_str("<skills>\n");
        for name in &skills {
            // ids/names are untrusted-ish text; escape so the block stays well-formed.
            let name = name
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            block.push_str(&format!("  <skill name=\"{name}\" />\n"));
        }
        block.push_str(
            "</skills>\n\
             If relevant to the request, call the `skill` tool with the skill name \
             to load its full instructions before answering.\n",
        );
    }

    let agents = dedup(&m.agents);
    if !agents.is_empty() {
        block.push_str("<agents>\n");
        for id in &agents {
            let id = id
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            block.push_str(&format!("  <agent id=\"{id}\" />\n"));
        }
        block.push_str(
            "</agents>\n\
             To involve these agents, call the `delegate` tool with target_agent set \
             to the agent id, give it a clear self-contained task, and integrate its \
             results into your reply.\n",
        );
    }

    block.push_str("</attached-mentions>");
    Some(block)
}

/// An incoming message from a user
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingMessage {
    /// Unique message ID
    pub id: Id,
    /// The user who sent the message
    pub user_id: UserId,
    /// The conversation this message belongs to
    pub conversation_id: ConversationId,
    /// The content of the message
    pub content: String,
    /// Optional attachments (files, images, etc.)
    pub attachments: Vec<Attachment>,
    /// Message metadata
    pub metadata: MessageMetadata,
    /// Where this message originated from.
    pub provenance: InputProvenance,
    /// Mention state (relevant for group channels).
    pub mention: MentionState,
}

impl IncomingMessage {
    /// Create a new incoming message
    pub fn new(
        user_id: impl Into<String>,
        conversation_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: Id::new(),
            user_id: UserId::new(user_id),
            conversation_id: ConversationId::new(conversation_id),
            content: content.into(),
            attachments: Vec::new(),
            metadata: MessageMetadata::new(),
            provenance: InputProvenance::default(),
            mention: MentionState::DirectMessage,
        }
    }

    /// Set the input provenance.
    pub fn with_provenance(mut self, provenance: InputProvenance) -> Self {
        self.provenance = provenance;
        self
    }

    /// Set the mention state.
    pub fn with_mention(mut self, mention: MentionState) -> Self {
        self.mention = mention;
        self
    }

    /// Return `true` if this message should be processed given the channel's
    /// mention requirement setting.
    pub fn should_process(&self, require_mention_in_groups: bool) -> bool {
        self.mention.should_process(require_mention_in_groups)
    }

    /// Add an attachment
    pub fn with_attachment(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// Set metadata
    pub fn with_metadata(mut self, metadata: MessageMetadata) -> Self {
        self.metadata = metadata;
        self
    }
}

/// An outgoing message to a user
#[derive(Debug, Clone, Serialize)]
pub struct OutgoingMessage {
    /// The conversation to send to
    pub conversation_id: ConversationId,
    /// The content to send
    pub content: String,
    /// Optional reasoning / thinking content (from reasoning models)
    pub reasoning_content: Option<String>,
    /// Optional tool calls made by the assistant
    pub tool_calls: Option<Vec<crate::providers::ToolCall>>,
    /// Optional formatted content (for rich formatting)
    pub formatted_content: Option<FormattedContent>,
    /// Optional attachments
    pub attachments: Vec<Attachment>,
    /// Whether this is a reply to a specific message
    pub reply_to: Option<Id>,
    /// Message options
    pub options: MessageOptions,
    /// Token usage (prompt, completion, total) if tracked
    pub usage: Option<crate::providers::Usage>,
}

impl OutgoingMessage {
    /// Create a new outgoing message
    pub fn new(conversation_id: ConversationId, content: impl Into<String>) -> Self {
        Self {
            conversation_id,
            content: content.into(),
            reasoning_content: None,
            tool_calls: None,
            formatted_content: None,
            attachments: Vec::new(),
            reply_to: None,
            options: MessageOptions::default(),
            usage: None,
        }
    }

    /// Add reasoning content
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning_content = Some(reasoning.into());
        self
    }

    /// Add tool calls
    pub fn with_tool_calls(mut self, calls: Vec<crate::providers::ToolCall>) -> Self {
        self.tool_calls = Some(calls);
        self
    }

    /// Add token usage information
    pub fn with_usage(mut self, usage: crate::providers::Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Add formatted content
    pub fn with_formatted(mut self, content: FormattedContent) -> Self {
        self.formatted_content = Some(content);
        self
    }

    /// Add an attachment
    pub fn with_attachment(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// Set reply-to message
    pub fn reply_to(mut self, message_id: Id) -> Self {
        self.reply_to = Some(message_id);
        self
    }

    /// Set message options
    pub fn with_options(mut self, options: MessageOptions) -> Self {
        self.options = options;
        self
    }
}

/// Formatted content for rich messages
#[derive(Debug, Clone, Serialize)]
pub enum FormattedContent {
    /// Markdown formatted text
    Markdown(String),
    /// HTML formatted text
    Html(String),
    /// Slack mrkdwn format
    SlackMrkdwn(String),
    /// Discord embed
    DiscordEmbed(DiscordEmbed),
}

/// Discord embed structure
#[derive(Debug, Clone, Default, Serialize)]
pub struct DiscordEmbed {
    pub title: Option<String>,
    pub description: Option<String>,
    pub color: Option<u32>,
    pub fields: Vec<EmbedField>,
}

/// A field in a Discord embed
#[derive(Debug, Clone, Serialize)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

/// Message sending options
#[derive(Debug, Clone, Default, Serialize)]
pub struct MessageOptions {
    /// Whether to send silently (no notification)
    pub silent: bool,
    /// Whether to expect a typing indicator first
    pub show_typing: bool,
    /// Custom metadata for the channel
    pub custom: HashMap<String, String>,
}

/// An attachment to a message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// Unique ID for this attachment
    pub id: Id,
    /// The filename
    pub filename: String,
    /// MIME type
    pub content_type: String,
    /// File size in bytes
    pub size: usize,
    /// The actual data (optional, may be URL-based)
    pub data: Option<Vec<u8>>,
    /// URL to access the attachment (if hosted)
    pub url: Option<String>,
}

impl Attachment {
    /// Create a new attachment
    pub fn new(filename: impl Into<String>, content_type: impl Into<String>) -> Self {
        Self {
            id: Id::new(),
            filename: filename.into(),
            content_type: content_type.into(),
            size: 0,
            data: None,
            url: None,
        }
    }

    /// Set the attachment data
    pub fn with_data(mut self, data: Vec<u8>) -> Self {
        self.size = data.len();
        self.data = Some(data);
        self
    }

    /// Set the attachment URL
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }
}

/// Chat types supported by channels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatType {
    /// Direct/private message
    Direct,
    /// Group chat
    Group,
    /// Channel (broadcast)
    Channel,
    /// Thread/reply
    Thread,
}

/// Channel capabilities
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelCapabilities {
    /// Supported chat types (direct, group, channel, thread)
    pub chat_types: Vec<ChatType>,
    /// Supports formatted text (markdown, HTML, etc.)
    pub supports_formatting: bool,
    /// Supports file attachments
    pub supports_attachments: bool,
    /// Supports inline images
    pub supports_images: bool,
    /// Supports message threading/replies
    pub supports_threads: bool,
    /// Supports typing indicators
    pub supports_typing: bool,
    /// Supports reaction buttons
    pub supports_buttons: bool,
    /// Supports slash commands
    pub supports_commands: bool,
    /// Supports message reactions (emoji reactions)
    pub supports_reactions: bool,
    /// Supports editing messages
    pub supports_edit: bool,
    /// Supports deleting/unsending messages
    pub supports_unsend: bool,
    /// Supports special effects (confetti, etc.)
    pub supports_effects: bool,
}

impl Default for ChannelCapabilities {
    fn default() -> Self {
        Self {
            chat_types: vec![ChatType::Direct],
            supports_formatting: true,
            supports_attachments: true,
            supports_images: true,
            supports_threads: false,
            supports_typing: true,
            supports_buttons: false,
            supports_commands: false,
            supports_reactions: false,
            supports_edit: false,
            supports_unsend: false,
            supports_effects: false,
        }
    }
}

/// Trait for message channels
#[async_trait]
pub trait Channel: Send + Sync {
    /// Get the name of this channel
    fn name(&self) -> &str;

    /// Get the capabilities of this channel
    fn capabilities(&self) -> ChannelCapabilities;

    /// Start the channel (begin listening for messages)
    async fn start(&self) -> crate::Result<()>;

    /// Stop the channel
    async fn stop(&self) -> crate::Result<()>;

    /// Send a message
    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id>;

    /// Send a typing indicator
    async fn send_typing(&self, conversation_id: &ConversationId) -> crate::Result<()>;

    /// Edit a previously sent message
    async fn edit_message(&self, message_id: Id, new_content: String) -> crate::Result<()>;

    /// Delete a message
    async fn delete_message(&self, message_id: Id) -> crate::Result<()>;

    /// Check if the channel is healthy
    async fn health_check(&self) -> crate::Result<bool>;

    /// Get current state for persistence (optional)
    async fn get_state(&self) -> Option<state::ChannelState> {
        None // Default: no state to persist
    }

    /// Restore state on startup (optional)
    async fn restore_state(&self, _state: state::ChannelState) -> crate::Result<()> {
        Ok(()) // Default: no state to restore
    }

    /// Get detailed health status (optional)
    async fn health_status(&self) -> health::HealthStatus {
        // Default implementation uses health_check
        match self.health_check().await {
            Ok(true) => health::HealthStatus::Healthy,
            Ok(false) => health::HealthStatus::Unhealthy,
            Err(_) => health::HealthStatus::Unhealthy,
        }
    }

    // ── Advanced message actions (default = unsupported) ───────────────────

    /// Add a reaction (emoji) to a message.
    async fn add_reaction(&self, _message_id: Id, _emoji: String) -> crate::Result<()> {
        Err(crate::SyscityError::Unsupported("reactions not supported".into()))
    }

    /// Remove a reaction (emoji) from a message.
    async fn remove_reaction(&self, _message_id: Id, _emoji: String) -> crate::Result<()> {
        Err(crate::SyscityError::Unsupported("reactions not supported".into()))
    }

    /// Pin a message in the conversation.
    async fn pin_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(crate::SyscityError::Unsupported("pin not supported".into()))
    }

    /// Unpin a message.
    async fn unpin_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(crate::SyscityError::Unsupported("unpin not supported".into()))
    }

    /// Create a thread from a message.
    async fn create_thread(
        &self,
        _message_id: Id,
        _title: Option<String>,
    ) -> crate::Result<ConversationId> {
        Err(crate::SyscityError::Unsupported("threads not supported".into()))
    }

    /// Send a poll to a conversation.
    async fn send_poll(
        &self,
        _conversation_id: ConversationId,
        _question: String,
        _options: Vec<String>,
    ) -> crate::Result<Id> {
        Err(crate::SyscityError::Unsupported("polls not supported".into()))
    }
}

/// A boxed channel for storage
pub type BoxedChannel = Box<dyn Channel>;

/// A shared (Arc-wrapped) channel for cloneable storage
pub type SharedChannel = Arc<dyn Channel>;

/// Registry of channels
#[derive(Default)]
pub struct ChannelRegistry {
    channels: HashMap<String, SharedChannel>,
}

impl std::fmt::Debug for ChannelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelRegistry")
            .field("channels", &self.channels.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ChannelRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self { channels: HashMap::new() }
    }

    /// Register a channel
    pub fn register(&mut self, channel: BoxedChannel) {
        let name = channel.name().to_string();
        self.channels.insert(name, Arc::from(channel));
    }

    /// Get a channel by name
    pub fn get(&self, name: &str) -> Option<SharedChannel> {
        self.channels.get(name).cloned()
    }

    /// List available channel names
    pub fn list(&self) -> Vec<&str> {
        self.channels.keys().map(|s| s.as_str()).collect()
    }

    /// Check if a channel exists
    pub fn has(&self, name: &str) -> bool {
        self.channels.contains_key(name)
    }

    /// Start all channels
    pub async fn start_all(&self) -> Vec<crate::Result<()>> {
        let mut results = Vec::new();
        for channel in self.channels.values() {
            results.push(channel.start().await);
        }
        results
    }

    /// Stop all channels
    pub async fn stop_all(&self) -> Vec<crate::Result<()>> {
        let mut results = Vec::new();
        for channel in self.channels.values() {
            results.push(channel.stop().await);
        }
        results
    }
}

/// Input validation and sanitization for messages
pub mod validation {

    /// Default maximum message length (10,000 characters)
    pub const DEFAULT_MAX_MESSAGE_LENGTH: usize = 10_000;

    /// Minimum message length (non-empty)
    pub const MIN_MESSAGE_LENGTH: usize = 1;

    /// Validation error for incoming messages
    #[derive(Debug, Clone, PartialEq)]
    pub enum ValidationError {
        /// Message is too long
        TooLong { max: usize, actual: usize },
        /// Message is too short (empty)
        TooShort { min: usize, actual: usize },
        /// Contains potentially dangerous content
        SuspiciousContent(String),
        /// Contains control characters
        ControlCharacters(String),
    }

    impl std::fmt::Display for ValidationError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::TooLong { max, actual } => {
                    write!(f, "Message too long: {} characters (max {})", actual, max)
                }
                Self::TooShort { min, actual } => {
                    write!(f, "Message too short: {} characters (min {})", actual, min)
                }
                Self::SuspiciousContent(reason) => {
                    write!(f, "Suspicious content detected: {}", reason)
                }
                Self::ControlCharacters(chars) => {
                    write!(f, "Control characters not allowed: {}", chars)
                }
            }
        }
    }

    impl std::error::Error for ValidationError {}

    /// Message validator with configurable limits
    #[derive(Debug, Clone)]
    pub struct MessageValidator {
        max_length: usize,
        min_length: usize,
        allow_control_chars: bool,
        sanitize_html: bool,
    }

    impl Default for MessageValidator {
        fn default() -> Self {
            Self {
                max_length: DEFAULT_MAX_MESSAGE_LENGTH,
                min_length: MIN_MESSAGE_LENGTH,
                allow_control_chars: false,
                sanitize_html: true,
            }
        }
    }

    impl MessageValidator {
        /// Create a new validator with default settings
        pub fn new() -> Self {
            Self::default()
        }

        /// Set maximum message length
        pub fn with_max_length(mut self, max: usize) -> Self {
            self.max_length = max;
            self
        }

        /// Set minimum message length
        pub fn with_min_length(mut self, min: usize) -> Self {
            self.min_length = min;
            self
        }

        /// Allow control characters
        pub fn allow_control_chars(mut self, allow: bool) -> Self {
            self.allow_control_chars = allow;
            self
        }

        /// Enable/disable HTML sanitization
        pub fn with_html_sanitization(mut self, sanitize: bool) -> Self {
            self.sanitize_html = sanitize;
            self
        }

        /// Validate a message, returning an error if invalid
        pub fn validate(&self, message: &str) -> Result<(), ValidationError> {
            let length = message.chars().count();

            // Check minimum length
            if length < self.min_length {
                return Err(ValidationError::TooShort {
                    min: self.min_length,
                    actual: length,
                });
            }

            // Check maximum length
            if length > self.max_length {
                return Err(ValidationError::TooLong {
                    max: self.max_length,
                    actual: length,
                });
            }

            // Check for control characters
            if !self.allow_control_chars {
                let control_chars: Vec<char> = message
                    .chars()
                    .filter(|c| c.is_control() && !c.is_whitespace())
                    .collect();
                if !control_chars.is_empty() {
                    return Err(ValidationError::ControlCharacters(control_chars.iter().collect()));
                }
            }

            // Check for null bytes
            if message.contains('\0') {
                return Err(ValidationError::SuspiciousContent(
                    "Null bytes not allowed".to_string(),
                ));
            }

            Ok(())
        }

        /// Sanitize a message, removing/replacing dangerous content
        pub fn sanitize(&self, message: &str) -> String {
            let mut sanitized = message.to_string();

            // Remove null bytes
            sanitized = sanitized.replace('\0', "");

            // Remove control characters (except whitespace)
            if !self.allow_control_chars {
                sanitized = sanitized
                    .chars()
                    .filter(|c| !c.is_control() || c.is_whitespace())
                    .collect();
            }

            // Escape HTML if configured
            if self.sanitize_html {
                sanitized = sanitized
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&#39;");
            }

            // Trim leading/trailing whitespace
            sanitized = sanitized.trim().to_string();

            // Limit length if too long
            if sanitized.chars().count() > self.max_length {
                sanitized = sanitized.chars().take(self.max_length).collect();
            }

            sanitized
        }

        /// Validate and sanitize in one step
        pub fn validate_and_sanitize(&self, message: &str) -> Result<String, ValidationError> {
            let sanitized = self.sanitize(message);
            self.validate(&sanitized)?;
            Ok(sanitized)
        }
    }

    /// Quick validation function for simple use cases
    pub fn validate_message(message: &str) -> Result<(), ValidationError> {
        let validator = MessageValidator::new();
        validator.validate(message)
    }

    /// Quick sanitization function for simple use cases
    pub fn sanitize_message(message: &str) -> String {
        let validator = MessageValidator::new();
        validator.sanitize(message)
    }
}

// Re-export channel implementations
#[cfg(feature = "discord")]
pub use discord::{DiscordChannel, DiscordConfig};
#[cfg(feature = "imessage")]
pub use imessage::{ImessageChannel, ImessageConfig};
#[cfg(feature = "feishu")]
pub use lark::{LarkChannel, LarkConfig};
#[cfg(feature = "plugins")]
pub use plugin_host::{PluginChannel, PluginChannelRegistry, PluginManifest};
#[cfg(feature = "qq")]
pub use qq::{QqChannel, QqConfig};
#[cfg(feature = "signal")]
pub use signal::{SignalChannel, SignalConfig};
#[cfg(feature = "slack")]
pub use slack::{SlackChannel, SlackConfig};
#[cfg(feature = "telegram")]
pub use telegram::{TelegramChannel, TelegramConfig};
#[cfg(feature = "webchat")]
pub use webchat::{WebchatChannel, WebchatConfig};
#[cfg(feature = "whatsapp")]
pub use whatsapp::{WhatsappChannel, WhatsappConfig};

/// Extended channel registry that supports both native and WASM plugins
#[cfg(feature = "plugins")]
pub struct ExtendedChannelRegistry {
    /// Native channels
    native: ChannelRegistry,
    /// WASM plugin channels
    plugins: Option<PluginChannelRegistry>,
    /// Cached plugin channel instances for name-based lookup (interior
    /// mutability for &self access)
    plugin_channels: Arc<RwLock<HashMap<String, Arc<PluginChannel>>>>,
    /// Parsed manifests for loaded plugins (name -> manifest)
    manifests: Arc<RwLock<HashMap<String, PluginManifest>>>,
}

#[cfg(feature = "plugins")]
impl ExtendedChannelRegistry {
    /// Create a new extended registry
    pub fn new(message_tx: mpsc::UnboundedSender<IncomingMessage>) -> Self {
        let plugin_dir = crate::dirs::extensions_dir().join("channels");
        Self {
            native: ChannelRegistry::new(),
            plugins: Some(PluginChannelRegistry::new(plugin_dir, message_tx)),
            plugin_channels: Arc::new(RwLock::new(HashMap::new())),
            manifests: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a native channel
    pub fn register_native(&mut self, channel: BoxedChannel) {
        self.native.register(channel);
    }

    /// Get a channel by name (checks native first, then plugin channels)
    pub async fn get(&self, name: &str) -> Option<SharedChannel> {
        // Check native channels first
        if let Some(channel) = self.native.get(name) {
            return Some(channel);
        }

        // Check plugin channels
        let pc = self.plugin_channels.read().await;
        if let Some(channel) = pc.get(name).cloned() {
            return Some(channel as SharedChannel);
        }

        None
    }

    /// List all available channel names
    pub async fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .native
            .list()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        if let Some(ref plugins) = self.plugins {
            names.extend(plugins.list_loaded().await);
        }

        names
    }

    /// Register a plugin channel externally (e.g., from PluginChannelRegistry
    /// callbacks).
    pub async fn register_plugin_channel(
        &self,
        name: &str,
        channel: Arc<PluginChannel>,
        manifest: Option<PluginManifest>,
    ) {
        let mut pc = self.plugin_channels.write().await;
        pc.insert(name.to_string(), channel);
        if let Some(m) = manifest {
            let mut manifests = self.manifests.write().await;
            manifests.insert(name.to_string(), m);
        }
    }

    /// Load a WASM plugin
    pub async fn load_plugin(
        &self,
        name: &str,
        config: Option<serde_json::Value>,
    ) -> crate::Result<()> {
        if let Some(ref plugins) = self.plugins {
            let plugin = plugins.load_plugin(name, config).await?;
            // Store the loaded plugin for name-based lookup
            let mut pc = self.plugin_channels.write().await;
            pc.insert(name.to_string(), plugin.clone());
            // Also fetch and store the manifest if available
            if let Some(manifest) = plugins.get_manifest(name).await {
                let mut m = self.manifests.write().await;
                m.insert(name.to_string(), manifest);
            }
        }
        Ok(())
    }

    /// Unload a WASM plugin
    pub async fn unload_plugin(&self, name: &str) -> crate::Result<()> {
        if let Some(ref plugins) = self.plugins {
            plugins.unload_plugin(name).await?;
        }
        // Clean up cached references
        {
            let mut pc = self.plugin_channels.write().await;
            pc.remove(name);
        }
        {
            let mut m = self.manifests.write().await;
            m.remove(name);
        }
        Ok(())
    }

    /// Discover available WASM plugins
    pub async fn discover_plugins(&self) -> crate::Result<Vec<(String, std::path::PathBuf)>> {
        if let Some(ref plugins) = self.plugins {
            plugins.discover_plugins().await
        } else {
            Ok(Vec::new())
        }
    }

    /// Start all channels (native and plugins)
    pub async fn start_all(&self) -> Vec<crate::Result<()>> {
        let mut results = self.native.start_all().await;

        if let Some(ref plugins) = self.plugins {
            results.extend(plugins.start_all().await);
        }

        results
    }

    /// Stop all channels
    pub async fn stop_all(&self) -> Vec<crate::Result<()>> {
        let mut results = self.native.stop_all().await;

        if let Some(ref plugins) = self.plugins {
            results.extend(plugins.stop_all().await);
        }

        results
    }
}

#[cfg(feature = "plugins")]
impl Default for ExtendedChannelRegistry {
    fn default() -> Self {
        let (message_tx, mut message_rx) = mpsc::unbounded_channel();
        // Drain the receiver in a background task so the channel stays open
        // and plugin channels can send messages without getting a disconnected
        // error. Without this, dropping `message_rx` immediately closes the
        // channel.
        tokio::spawn(async move { while message_rx.recv().await.is_some() {} });
        Self::new(message_tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_id() {
        let id = UserId::new("user123");
        assert_eq!(id.0, "user123");
        assert_eq!(id.to_string(), "user123");
    }

    #[test]
    fn test_conversation_id() {
        let id = ConversationId::new("conv456");
        assert_eq!(id.0, "conv456");

        let generated = ConversationId::generate();
        assert!(!generated.0.is_empty());
    }

    #[test]
    fn test_incoming_message() {
        let msg = IncomingMessage::new("user1", "conv1", "Hello!");
        assert_eq!(msg.user_id.0, "user1");
        assert_eq!(msg.conversation_id.0, "conv1");
        assert_eq!(msg.content, "Hello!");
        assert!(msg.attachments.is_empty());
    }

    #[test]
    fn test_outgoing_message() {
        let conv_id = ConversationId::new("conv1");
        let msg = OutgoingMessage::new(conv_id, "Hi there!");
        assert_eq!(msg.content, "Hi there!");
        assert!(msg.formatted_content.is_none());

        let markdown = OutgoingMessage::new(ConversationId::new("conv1"), "Hello")
            .with_formatted(FormattedContent::Markdown("**Hello**".to_string()));
        assert!(matches!(markdown.formatted_content, Some(FormattedContent::Markdown(_))));
    }

    #[test]
    fn test_attachment() {
        let attachment =
            Attachment::new("test.txt", "text/plain").with_data(b"Hello World".to_vec());
        assert_eq!(attachment.filename, "test.txt");
        assert_eq!(attachment.size, 11);
    }

    #[test]
    fn test_channel_capabilities() {
        let caps = ChannelCapabilities::default();
        assert!(caps.supports_formatting);
        assert!(caps.supports_attachments);
    }

    #[test]
    fn mentions_block_skills_only() {
        let m = Mentions {
            skills: vec!["canvas-design".to_string()],
            agents: vec![],
        };
        let block = format_mentions_block(&m).expect("skills-only should format");
        assert!(block.starts_with("<attached-mentions>\n"));
        assert!(block.contains("<skills>\n  <skill name=\"canvas-design\" />\n</skills>\n"));
        assert!(block.contains("call the `skill` tool with the skill name"));
        assert!(!block.contains("<agents>"));
        assert!(block.ends_with("</attached-mentions>"));
    }

    #[test]
    fn mentions_block_agents_only() {
        let m = Mentions {
            skills: vec![],
            agents: vec!["secretary-xiaowang".to_string()],
        };
        let block = format_mentions_block(&m).expect("agents-only should format");
        assert!(block.contains("<agents>\n  <agent id=\"secretary-xiaowang\" />\n</agents>\n"));
        assert!(block.contains("call the `delegate` tool with target_agent"));
        assert!(!block.contains("<skills>"));
    }

    #[test]
    fn mentions_block_both_sections_and_dedup() {
        let m = Mentions {
            skills: vec!["a".to_string(), "b".to_string(), "a".to_string()],
            agents: vec!["bob".to_string(), "bob".to_string()],
        };
        let block = format_mentions_block(&m).expect("both sections should format");
        assert!(block.contains("<skills>"));
        assert!(block.contains("<agents>"));
        assert_eq!(block.matches("<skill name=").count(), 2, "dup skills dropped");
        assert_eq!(block.matches("<agent id=").count(), 1, "dup agents dropped");
        // Order preserved.
        let skills_section = block.split_once("<skills>").unwrap().1;
        assert!(skills_section.starts_with("\n  <skill name=\"a\" />"));
    }

    #[test]
    fn mentions_block_empty_is_none() {
        assert!(format_mentions_block(&Mentions::default()).is_none());
        assert!(format_mentions_block(&Mentions { skills: vec![], agents: vec![] }).is_none());
    }

    #[test]
    fn mentions_block_escapes_xml_sensitive_chars() {
        let m = Mentions {
            skills: vec!["a<b>&".to_string()],
            agents: vec![],
        };
        let block = format_mentions_block(&m).expect("should format");
        assert!(block.contains("a&lt;b&gt;&amp;"));
        assert!(!block.contains("a<b>&"));
    }

    #[test]
    fn mentions_from_extra_roundtrip() {
        let m = Mentions {
            skills: vec!["s".to_string()],
            agents: vec!["a".to_string()],
        };
        let extra = HashMap::from([("mentions".to_string(), serde_json::to_value(&m).unwrap())]);
        assert_eq!(mentions_from_extra(&extra), Some(m));
    }

    #[test]
    fn mentions_from_extra_rejects_malformed_and_missing() {
        let mut extra = HashMap::new();
        extra.insert("mentions".to_string(), serde_json::json!("not-an-object"));
        assert_eq!(mentions_from_extra(&extra), None);
        assert_eq!(mentions_from_extra(&HashMap::new()), None);

        // Both lists empty -> None (nothing to inject).
        extra.insert("mentions".to_string(), serde_json::json!({"skills": [], "agents": []}));
        assert_eq!(mentions_from_extra(&extra), None);
    }
}

// Re-exports for new channel management modules
pub use health::{ChannelHealth, ChannelHealthMonitor, HealthStatus};
pub use lifecycle::{ChannelLifecycle, ChannelStatus, LifecycleManager, RestartPolicy};
pub use metrics::{ChannelMetrics, LatencyWindow, MetricsManager, MetricsSnapshot};
pub use state::{ChannelState, ChannelStateStore};

/// Shared DM policy state for channel message handlers.
///
/// Groups `pairing_store`, `dm_policy`, and `allow_from` into a single
/// clonable struct so callers pass one argument instead of three.
#[derive(Clone)]
pub struct ChannelPolicy {
    pub pairing_store: std::sync::Arc<
        tokio::sync::RwLock<Option<std::sync::Arc<crate::security::pairing::PairingStore>>>,
    >,
    pub dm_policy: std::sync::Arc<tokio::sync::RwLock<crate::security::pairing::DmPolicy>>,
    pub allow_from: std::sync::Arc<tokio::sync::RwLock<Vec<String>>>,
}

impl ChannelPolicy {
    pub fn new(
        pairing_store: std::sync::Arc<
            tokio::sync::RwLock<Option<std::sync::Arc<crate::security::pairing::PairingStore>>>,
        >,
        dm_policy: std::sync::Arc<tokio::sync::RwLock<crate::security::pairing::DmPolicy>>,
        allow_from: std::sync::Arc<tokio::sync::RwLock<Vec<String>>>,
    ) -> Self {
        Self {
            pairing_store,
            dm_policy,
            allow_from,
        }
    }
}
