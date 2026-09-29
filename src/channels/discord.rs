//! Discord Channel Implementation
//!
//! This module implements the Channel trait for Discord using serenity.

use std::sync::Arc;

use async_trait::async_trait;
#[cfg(feature = "discord")]
use serenity::{
    async_trait as serenity_async_trait,
    builder::{CreateEmbed, CreateMessage},
    client::{Context, EventHandler},
    model::{
        channel::{Message, ReactionType},
        gateway::Ready,
        id::ChannelId,
    },
    prelude::GatewayIntents,
    Client,
};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

use crate::channels::{
    Channel, ChannelCapabilities, ConversationId, DiscordEmbed, FormattedContent, IncomingMessage,
    MessageMetadata, OutgoingMessage,
};
use crate::core::models::Id;
use crate::security::pairing::{DmPolicy, PairingStore, RequestAccessResult};

/// Discord channel configuration
#[derive(Debug, Clone)]
pub struct DiscordConfig {
    /// Bot token
    pub token: String,
    /// Optional allowed user IDs (empty = allow all)
    pub allowed_user_ids: Vec<u64>,
    /// Message handler channel
    pub message_tx: Option<mpsc::UnboundedSender<IncomingMessage>>,
    /// Command prefix (e.g., "!")
    pub command_prefix: String,
}

impl DiscordConfig {
    /// Create new config with token
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            allowed_user_ids: Vec::new(),
            message_tx: None,
            command_prefix: "!".to_string(),
        }
    }

    /// Set allowed user IDs
    pub fn allow_user_ids(mut self, user_ids: Vec<u64>) -> Self {
        self.allowed_user_ids = user_ids;
        self
    }

    /// Set message handler
    pub fn with_message_handler(mut self, tx: mpsc::UnboundedSender<IncomingMessage>) -> Self {
        self.message_tx = Some(tx);
        self
    }

    /// Set command prefix
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.command_prefix = prefix.into();
        self
    }
}

/// Discord channel implementation
pub struct DiscordChannel {
    config: DiscordConfig,
    /// Discord client (stored for future use)
    #[cfg(feature = "discord")]
    _client: Option<Arc<tokio::sync::Mutex<Client>>>,
    #[cfg(feature = "discord")]
    http: Option<Arc<serenity::http::Http>>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Syscity message ID -> (Discord message ID, Discord channel ID) mapping
    #[cfg(feature = "discord")]
    message_map: Arc<tokio::sync::RwLock<std::collections::HashMap<Id, (u64, u64)>>>,
    /// Session mapping: channel_id -> session_uuid (for /new command support)
    #[cfg(feature = "discord")]
    session_map: Arc<tokio::sync::RwLock<std::collections::HashMap<u64, String>>>,
    /// Pairing store for DM access control
    pairing_store: Arc<RwLock<Option<Arc<PairingStore>>>>,
    /// DM policy for access control
    dm_policy: Arc<RwLock<DmPolicy>>,
    /// Allowlist for users (used with Allowlist policy)
    allow_from: Arc<RwLock<Vec<String>>>,
}

impl std::fmt::Debug for DiscordChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordChannel")
            .field("config", &self.config)
            .field("running", &self.running)
            .finish()
    }
}

impl DiscordChannel {
    /// Create a new Discord channel
    pub fn new(config: DiscordConfig) -> Self {
        #[cfg(feature = "discord")]
        let http = Some(Arc::new(serenity::http::Http::new(&config.token)));

        Self {
            config,
            #[cfg(feature = "discord")]
            _client: None,
            #[cfg(feature = "discord")]
            http,
            running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(feature = "discord")]
            message_map: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            #[cfg(feature = "discord")]
            session_map: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            pairing_store: Arc::new(RwLock::new(None)),
            dm_policy: Arc::new(RwLock::new(DmPolicy::Open)),
            allow_from: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Set the pairing store for DM access control
    pub async fn set_pairing_store(&self, store: Arc<PairingStore>) {
        let mut ps = self.pairing_store.write().await;
        *ps = Some(store);
    }

    /// Set the DM policy
    pub async fn set_dm_policy(&self, policy: DmPolicy) {
        let mut policy_guard = self.dm_policy.write().await;
        *policy_guard = policy;
    }

    /// Set the allowlist
    pub async fn set_allow_from(&self, allow_from: Vec<String>) {
        let mut af = self.allow_from.write().await;
        *af = allow_from;
    }

    /// Track a message ID to channel ID mapping
    #[cfg(feature = "discord")]
    async fn track_message(&self, syscity_id: Id, discord_msg_id: u64, channel_id: u64) {
        let mut map = self.message_map.write().await;
        map.insert(syscity_id, (discord_msg_id, channel_id));
    }

    /// Get (Discord message ID, channel ID) for a Syscity message ID
    #[cfg(feature = "discord")]
    async fn get_message_info(&self, syscity_id: Id) -> Option<(u64, u64)> {
        let map = self.message_map.read().await;
        map.get(&syscity_id).copied()
    }

    /// Convert markdown to Discord markdown (Discord uses standard markdown
    /// mostly)
    fn format_for_discord(text: &str) -> String {
        // Discord supports standard markdown well, but we need to handle some specifics

        // Discord uses triple backticks for code blocks with language
        // Already supported in standard markdown

        // Spoiler tags: ||text|| (Discord specific)
        // We don't convert these as they're Discord-specific

        // Mentions: @user or @role - Discord handles these automatically

        text.to_string()
    }

    /// Create a serenity embed from our DiscordEmbed
    #[cfg(feature = "discord")]
    fn create_serenity_embed(embed: &DiscordEmbed) -> CreateEmbed {
        let mut e = CreateEmbed::new();

        if let Some(title) = &embed.title {
            e = e.title(title);
        }

        if let Some(description) = &embed.description {
            e = e.description(description);
        }

        if let Some(color) = embed.color {
            e = e.color(color);
        }

        for field in &embed.fields {
            e = e.field(&field.name, &field.value, field.inline);
        }

        e
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![
                crate::channels::ChatType::Direct,
                crate::channels::ChatType::Group,
                crate::channels::ChatType::Channel,
                crate::channels::ChatType::Thread,
            ],
            supports_formatting: true,
            supports_attachments: true,
            supports_images: true,
            supports_threads: true,
            supports_typing: true,
            supports_buttons: true,
            supports_commands: true,
            supports_reactions: true,
            supports_edit: true,
            supports_unsend: true,
            supports_effects: false,
        }
    }

    async fn start(&self) -> crate::Result<()> {
        #[cfg(feature = "discord")]
        {
            info!("Starting Discord channel");

            let intents = GatewayIntents::GUILD_MESSAGES
                | GatewayIntents::DIRECT_MESSAGES
                | GatewayIntents::GUILDS
                | GatewayIntents::MESSAGE_CONTENT
                | GatewayIntents::GUILD_MESSAGE_REACTIONS
                | GatewayIntents::DIRECT_MESSAGE_REACTIONS;

            let session_map = self.session_map.clone();
            let pairing_store = self.pairing_store.clone();
            let dm_policy = self.dm_policy.clone();
            let allow_from = self.allow_from.clone();
            let mut client = Client::builder(&self.config.token, intents)
                .event_handler(DiscordHandler {
                    config: self.config.clone(),
                    session_map,
                    pairing_store,
                    dm_policy,
                    allow_from,
                })
                .await
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!("Discord client error: {}", e))
                })?;

            self.running
                .store(true, std::sync::atomic::Ordering::SeqCst);

            tokio::spawn(async move {
                if let Err(why) = client.start().await {
                    error!("Discord client error: {:?}", why);
                }
            });

            info!("Discord channel started");
            Ok(())
        }

        #[cfg(not(feature = "discord"))]
        {
            Err(crate::error::SyscityError::Internal("Discord feature not enabled".to_string()))
        }
    }

    async fn stop(&self) -> crate::Result<()> {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        info!("Discord channel stopped");
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        #[cfg(feature = "discord")]
        {
            let channel_id: u64 = message.conversation_id.0.parse().map_err(|_| {
                crate::error::SyscityError::Validation("Invalid channel ID".to_string())
            })?;

            let http = self.http.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Internal("HTTP client not initialized".to_string())
            })?;

            let channel_id = ChannelId::new(channel_id);

            // Build the message content
            let content = match &message.formatted_content {
                Some(FormattedContent::Markdown(md)) => Self::format_for_discord(md),
                Some(FormattedContent::DiscordEmbed(embed)) => {
                    // Send embed message
                    let embed = Self::create_serenity_embed(embed);
                    let builder = CreateMessage::new().add_embed(embed);
                    let sent = channel_id.send_message(http, builder).await.map_err(|e| {
                        crate::error::SyscityError::ExternalService {
                            source: format!("Discord send failed: {}", e),
                            cause: None,
                        }
                    })?;
                    let syscity_id = Id::new();
                    self.track_message(syscity_id, sent.id.get(), channel_id.get())
                        .await;
                    return Ok(syscity_id);
                }
                _ => message.content,
            };

            // Send text message
            let builder = CreateMessage::new().content(content);
            let sent = channel_id.send_message(http, builder).await.map_err(|e| {
                // Log rate-limit events when the HTTP layer reports them.
                if matches!(&e, serenity::Error::Http(_)) {
                    warn!("Discord send failed (HTTP error, possibly rate limited)");
                }
                crate::error::SyscityError::ExternalService {
                    source: format!("Discord send failed: {}", e),
                    cause: None,
                }
            })?;

            // Track the message for edit/delete operations
            let syscity_id = Id::new();
            self.track_message(syscity_id, sent.id.get(), channel_id.get())
                .await;

            Ok(syscity_id)
        }

        #[cfg(not(feature = "discord"))]
        {
            let _ = message;
            Err(crate::error::SyscityError::Internal("Discord feature not enabled".to_string()))
        }
    }

    async fn send_typing(&self, conversation_id: &ConversationId) -> crate::Result<()> {
        #[cfg(feature = "discord")]
        {
            let channel_id: u64 = conversation_id.0.parse().map_err(|_| {
                crate::error::SyscityError::Validation("Invalid channel ID".to_string())
            })?;

            let http = self.http.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Internal("HTTP client not initialized".to_string())
            })?;

            let channel_id = ChannelId::new(channel_id);

            // Trigger typing indicator
            channel_id.broadcast_typing(http).await.map_err(|e| {
                crate::error::SyscityError::ExternalService {
                    source: format!("Discord typing indicator failed: {}", e),
                    cause: None,
                }
            })?;

            Ok(())
        }

        #[cfg(not(feature = "discord"))]
        {
            let _ = conversation_id;
            Err(crate::error::SyscityError::Internal("Discord feature not enabled".to_string()))
        }
    }

    async fn edit_message(&self, message_id: Id, new_content: String) -> crate::Result<()> {
        #[cfg(feature = "discord")]
        {
            let (discord_msg_id, channel_id_num) = self
                .get_message_info(message_id)
                .await
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!(
                        "Message {} not found in tracking (may have been sent before bot started)",
                        message_id
                    ),
                })?;

            let http = self.http.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Internal("HTTP client not initialized".to_string())
            })?;

            let channel_id = ChannelId::new(channel_id_num);
            let serenity_msg_id = serenity::model::id::MessageId::new(discord_msg_id);

            // Edit the message
            channel_id
                .edit_message(
                    http,
                    serenity_msg_id,
                    serenity::builder::EditMessage::new().content(new_content),
                )
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Discord edit failed: {}", e),
                    cause: None,
                })?;

            Ok(())
        }

        #[cfg(not(feature = "discord"))]
        {
            let _ = (message_id, new_content);
            Err(crate::error::SyscityError::Internal("Discord feature not enabled".to_string()))
        }
    }

    async fn delete_message(&self, message_id: Id) -> crate::Result<()> {
        #[cfg(feature = "discord")]
        {
            let (discord_msg_id, channel_id_num) = self
                .get_message_info(message_id)
                .await
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!(
                        "Message {} not found in tracking (may have been sent before bot started)",
                        message_id
                    ),
                })?;

            let http = self.http.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Internal("HTTP client not initialized".to_string())
            })?;

            let channel_id = ChannelId::new(channel_id_num);
            let serenity_msg_id = serenity::model::id::MessageId::new(discord_msg_id);

            // Delete the message
            channel_id
                .delete_message(http, serenity_msg_id)
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Discord delete failed: {}", e),
                    cause: None,
                })?;

            // Remove from tracking
            let mut map = self.message_map.write().await;
            map.remove(&message_id);

            Ok(())
        }

        #[cfg(not(feature = "discord"))]
        {
            let _ = message_id;
            Err(crate::error::SyscityError::Internal("Discord feature not enabled".to_string()))
        }
    }

    async fn health_check(&self) -> crate::Result<bool> {
        #[cfg(feature = "discord")]
        {
            if !self.running.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(false);
            }

            // Check HTTP client is available and can fetch current user
            if let Some(http) = &self.http {
                match http.get_current_user().await {
                    Ok(_) => Ok(true),
                    Err(e) => {
                        warn!("Discord health check failed: {}", e);
                        Ok(false)
                    }
                }
            } else {
                Ok(false)
            }
        }

        #[cfg(not(feature = "discord"))]
        {
            Ok(false)
        }
    }

    // ── Advanced actions ───────────────────────────────────────────────────

    async fn add_reaction(&self, message_id: Id, emoji: String) -> crate::Result<()> {
        #[cfg(feature = "discord")]
        {
            let (discord_msg_id, channel_id_num) = self
                .get_message_info(message_id)
                .await
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Message {} not found in tracking", message_id),
                })?;

            let http = self.http.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Internal("HTTP client not initialized".to_string())
            })?;

            let channel_id = ChannelId::new(channel_id_num);
            let serenity_msg_id = serenity::model::id::MessageId::new(discord_msg_id);
            let reaction = serenity::model::channel::ReactionType::Unicode(emoji);

            channel_id
                .create_reaction(http, serenity_msg_id, reaction)
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Discord add reaction failed: {}", e),
                    cause: None,
                })?;

            Ok(())
        }

        #[cfg(not(feature = "discord"))]
        {
            let _ = (message_id, emoji);
            Err(crate::error::SyscityError::Internal("Discord feature not enabled".to_string()))
        }
    }

    async fn remove_reaction(&self, message_id: Id, emoji: String) -> crate::Result<()> {
        #[cfg(feature = "discord")]
        {
            let (discord_msg_id, channel_id_num) = self
                .get_message_info(message_id)
                .await
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Message {} not found in tracking", message_id),
                })?;

            let http = self.http.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Internal("HTTP client not initialized".to_string())
            })?;

            let channel_id = ChannelId::new(channel_id_num);
            let serenity_msg_id = serenity::model::id::MessageId::new(discord_msg_id);
            let reaction = serenity::model::channel::ReactionType::Unicode(emoji);

            channel_id
                .delete_reaction(http, serenity_msg_id, None, reaction)
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Discord remove reaction failed: {}", e),
                    cause: None,
                })?;

            Ok(())
        }

        #[cfg(not(feature = "discord"))]
        {
            let _ = (message_id, emoji);
            Err(crate::error::SyscityError::Internal("Discord feature not enabled".to_string()))
        }
    }

    async fn pin_message(&self, message_id: Id) -> crate::Result<()> {
        #[cfg(feature = "discord")]
        {
            let (discord_msg_id, channel_id_num) = self
                .get_message_info(message_id)
                .await
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Message {} not found in tracking", message_id),
                })?;

            let http = self.http.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Internal("HTTP client not initialized".to_string())
            })?;

            let channel_id = ChannelId::new(channel_id_num);
            let serenity_msg_id = serenity::model::id::MessageId::new(discord_msg_id);

            channel_id.pin(http, serenity_msg_id).await.map_err(|e| {
                crate::error::SyscityError::ExternalService {
                    source: format!("Discord pin failed: {}", e),
                    cause: None,
                }
            })?;

            Ok(())
        }

        #[cfg(not(feature = "discord"))]
        {
            let _ = message_id;
            Err(crate::error::SyscityError::Internal("Discord feature not enabled".to_string()))
        }
    }

    async fn unpin_message(&self, message_id: Id) -> crate::Result<()> {
        #[cfg(feature = "discord")]
        {
            let (discord_msg_id, channel_id_num) = self
                .get_message_info(message_id)
                .await
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Message {} not found in tracking", message_id),
                })?;

            let http = self.http.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Internal("HTTP client not initialized".to_string())
            })?;

            let channel_id = ChannelId::new(channel_id_num);
            let serenity_msg_id = serenity::model::id::MessageId::new(discord_msg_id);

            channel_id.unpin(http, serenity_msg_id).await.map_err(|e| {
                crate::error::SyscityError::ExternalService {
                    source: format!("Discord unpin failed: {}", e),
                    cause: None,
                }
            })?;

            Ok(())
        }

        #[cfg(not(feature = "discord"))]
        {
            let _ = message_id;
            Err(crate::error::SyscityError::Internal("Discord feature not enabled".to_string()))
        }
    }

    async fn create_thread(
        &self,
        message_id: Id,
        title: Option<String>,
    ) -> crate::Result<ConversationId> {
        #[cfg(feature = "discord")]
        {
            let (discord_msg_id, channel_id_num) = self
                .get_message_info(message_id)
                .await
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Message {} not found in tracking", message_id),
                })?;

            let http = self.http.as_ref().ok_or_else(|| {
                crate::error::SyscityError::Internal("HTTP client not initialized".to_string())
            })?;

            let channel_id = ChannelId::new(channel_id_num);
            let serenity_msg_id = serenity::model::id::MessageId::new(discord_msg_id);
            let thread_name = title.unwrap_or_else(|| "Thread".to_string());

            let thread = channel_id
                .create_thread_from_message(
                    http,
                    serenity_msg_id,
                    serenity::builder::CreateThread::new(thread_name),
                )
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Discord create thread failed: {}", e),
                    cause: None,
                })?;

            Ok(ConversationId::new(thread.id.get().to_string()))
        }

        #[cfg(not(feature = "discord"))]
        {
            let _ = (message_id, title);
            Err(crate::error::SyscityError::Internal("Discord feature not enabled".to_string()))
        }
    }
}

#[cfg(feature = "discord")]
struct DiscordHandler {
    config: DiscordConfig,
    /// Session mapping: channel_id -> session_uuid (for /new command support)
    session_map: Arc<tokio::sync::RwLock<std::collections::HashMap<u64, String>>>,
    /// Pairing store for DM access control
    pairing_store: Arc<RwLock<Option<Arc<PairingStore>>>>,
    /// DM policy for access control
    dm_policy: Arc<RwLock<DmPolicy>>,
    /// Allowlist for users (used with Allowlist policy)
    allow_from: Arc<RwLock<Vec<String>>>,
}

#[cfg(feature = "discord")]
impl DiscordHandler {
    /// Get or create a session UUID for a channel
    async fn get_or_create_session(&self, channel_id: u64) -> String {
        {
            let sessions = self.session_map.read().await;
            if let Some(session_id) = sessions.get(&channel_id) {
                return session_id.clone();
            }
        }
        // Create new session
        let new_session = uuid::Uuid::new_v4().to_string();
        let mut sessions = self.session_map.write().await;
        sessions.insert(channel_id, new_session.clone());
        new_session
    }

    /// Reset session for a channel (when /new is used)
    async fn reset_session(&self, channel_id: u64) -> String {
        let new_session = uuid::Uuid::new_v4().to_string();
        let mut sessions = self.session_map.write().await;
        sessions.insert(channel_id, new_session.clone());
        new_session
    }
}

#[cfg(feature = "discord")]
#[serenity_async_trait]
impl EventHandler for DiscordHandler {
    async fn message(&self, ctx: Context, msg: Message) {
        // Ignore bot messages
        if msg.author.bot {
            return;
        }

        // Reject webhook-originated messages when webhook mode is configured
        // but signature verification infrastructure is present. This prevents
        // unverified webhook payloads from being processed when the bot is
        // configured for interaction-based webhooks.
        if msg.webhook_id.is_some() {
            warn!(
                "Discord webhook message from {} rejected — webhook-based messages require \
                 explicit signature verification via X-Signature-Ed25519",
                msg.author.name
            );
            return;
        }

        let user_id = msg.author.id.get().to_string();
        let username = msg.author.name.clone();
        let is_dm = msg.guild_id.is_none();

        // Check DM policy
        let policy = *self.dm_policy.read().await;

        match policy {
            DmPolicy::Open => {
                // Allow all - no checks needed
            }
            DmPolicy::Allowlist => {
                // Check if user is in allowlist
                let allow_list = self.allow_from.read().await;
                let is_allowed = allow_list
                    .iter()
                    .any(|a| a == &user_id || a.eq_ignore_ascii_case(&username));

                if !is_allowed {
                    warn!("User @{} ({}) is not in allowlist", username, user_id);
                    let _ = msg
                        .channel_id
                        .say(&ctx.http, "🔒 This bot is private. You're not authorized to use it.")
                        .await;
                    return;
                }
            }
            DmPolicy::Pairing => {
                // Only enforce pairing in DMs
                if is_dm {
                    if let Some(store) = self.pairing_store.read().await.as_ref() {
                        if !store.is_authorized("discord", &user_id).await {
                            // Not authorized - check if they already have a pending request
                            match store
                                .request_access("discord", &user_id, Some(&username))
                                .await
                            {
                                Ok(RequestAccessResult::AlreadyAuthorized) => {
                                    // Shouldn't happen since we just checked,
                                    // but allow through
                                }
                                Ok(RequestAccessResult::NewRequest { code }) => {
                                    info!(
                                        "New pairing request from @{} ({}): code={}",
                                        username, user_id, code
                                    );
                                    let _ = msg
                                        .channel_id
                                        .say(
                                            &ctx.http,
                                            format!(
                                                "🔒 This bot requires pairing.\n\nYour pairing \
                                                 code: **{}**\n\nPlease share this code with an \
                                                 admin to get access.\nOr ask an admin to \
                                                 run:\n`syscity pairing approve discord {}`",
                                                code, code
                                            ),
                                        )
                                        .await;
                                    return;
                                }
                                Ok(RequestAccessResult::AlreadyPending { code, created_at: _ }) => {
                                    let _ = msg
                                        .channel_id
                                        .say(
                                            &ctx.http,
                                            format!(
                                                "⏳ Your pairing request is still \
                                                 pending.\n\nCode: **{}**\n\nPlease wait for an \
                                                 admin to approve your request.",
                                                code
                                            ),
                                        )
                                        .await;
                                    return;
                                }
                                Ok(RequestAccessResult::RateLimited { .. }) => {
                                    let _ = msg
                                        .channel_id
                                        .say(
                                            &ctx.http,
                                            "⏳ Too many pairing requests. Please try again later.",
                                        )
                                        .await;
                                    return;
                                }
                                Err(_) => {
                                    let _ = msg
                                        .channel_id
                                        .say(
                                            &ctx.http,
                                            "❌ Error processing pairing request. Please try \
                                             again later.",
                                        )
                                        .await;
                                    return;
                                }
                            }
                        }
                    } else {
                        warn!("Pairing policy set but no pairing store configured");
                        let _ = msg
                            .channel_id
                            .say(
                                &ctx.http,
                                "🔒 Pairing is not configured. Please contact the admin.",
                            )
                            .await;
                        return;
                    }
                }
            }
        }

        // Legacy: Also check allowed_user_ids if not empty (for backward compatibility)
        if !self.config.allowed_user_ids.is_empty()
            && !self.config.allowed_user_ids.contains(&msg.author.id.get())
        {
            return;
        }

        debug!("Received Discord message from {}: {}", msg.author.name, msg.content);

        // Handle DMs and mentions
        let is_dm = msg.guild_id.is_none();
        let is_mentioned = msg.mentions.iter().any(|u| u.bot);
        let has_prefix = msg.content.starts_with(&self.config.command_prefix);
        let channel_id = msg.channel_id.get();

        if is_dm || is_mentioned || has_prefix {
            let content = if has_prefix {
                msg.content[self.config.command_prefix.len()..]
                    .trim()
                    .to_string()
            } else {
                msg.content.clone()
            };

            // Handle /new command to start a fresh session
            if content.trim() == "/new" {
                let new_session = self.reset_session(channel_id).await;
                info!("🆕 New session started for {}: {}", msg.author.name, new_session);
                let _ = msg
                    .channel_id
                    .say(
                        &ctx.http,
                        format!(
                            "🆕 Started new session:\n`{}`\n\nYour conversation history is now \
                             fresh.",
                            new_session
                        ),
                    )
                    .await;
                return;
            }

            // Get or create session UUID for this channel
            let session_id = self.get_or_create_session(channel_id).await;

            let detected = crate::tools::command_detector::detect_command(&content);

            let mut metadata = MessageMetadata::new()
                .with_extra("message_id", msg.id.get())
                .with_extra("username", msg.author.name.clone())
                .with_extra("is_dm", is_dm)
                .with_extra("discord_channel_id", channel_id);

            if let Some(ref result) = detected {
                metadata = metadata.with_detected_command(result);
            }

            let policy = crate::channels::ChannelPolicy::new(
                self.pairing_store.clone(),
                self.dm_policy.clone(),
                self.allow_from.clone(),
            );

            let provenance = crate::channels::InputProvenance::ExternalUser {
                channel: "discord".to_string(),
                is_direct: is_dm,
            };

            let mut incoming = IncomingMessage::new(
                msg.author.id.get().to_string(),
                &session_id, // Use UUID session instead of channel_id
                content,
            )
            .with_provenance(provenance)
            .with_metadata(metadata);

            if detected.is_some() {
                let auth_ctx = crate::channels::AuthContext::from_message(&incoming, &policy).await;
                incoming.metadata = incoming.metadata.with_auth_context(&auth_ctx);
            }

            // Send to handler if configured; responses arrive via Channel::send()
            if let Some(tx) = &self.config.message_tx {
                if tx.send(incoming).is_err() {
                    warn!("Discord message send failed: receiver closed");
                }
            } else {
                warn!("No message_tx configured for Discord channel — message dropped");
            }
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("Discord bot connected as {}", ready.user.name);
    }

    /// Handle reaction additions
    async fn reaction_add(&self, ctx: Context, add_reaction: serenity::model::channel::Reaction) {
        debug!(
            "Reaction added: {:?} by user {}",
            add_reaction.emoji,
            add_reaction.user_id.map(|id| id.get()).unwrap_or(0)
        );

        // Get message info
        if let Ok(message) = add_reaction.message(&ctx.http).await {
            // Check if user is allowed
            let user_id_str = if let Some(user_id) = add_reaction.user_id {
                if !self.config.allowed_user_ids.is_empty()
                    && !self.config.allowed_user_ids.contains(&user_id.get())
                {
                    return;
                }
                user_id.get().to_string()
            } else {
                String::new()
            };

            // Create incoming message for reaction
            let reaction_content =
                format!("reaction_add:{}", reaction_emoji_name(&add_reaction.emoji));
            let incoming = IncomingMessage::new(
                &user_id_str,
                add_reaction.channel_id.get().to_string(),
                reaction_content,
            )
            .with_metadata(
                MessageMetadata::new()
                    .with_extra("message_id", add_reaction.message_id.get())
                    .with_extra("reaction_emoji", reaction_emoji_name(&add_reaction.emoji))
                    .with_extra("reaction_type", "add")
                    .with_extra("original_message", message.content.clone()),
            );

            // Send to handler if configured
            if let Some(tx) = &self.config.message_tx {
                if tx.send(incoming).is_err() {
                    warn!("Discord message send failed: receiver closed");
                }
            }
        }
    }

    /// Handle reaction removals
    async fn reaction_remove(
        &self,
        _ctx: Context,
        removed_reaction: serenity::model::channel::Reaction,
    ) {
        debug!("Reaction removed: {:?}", removed_reaction.emoji);

        // Create incoming message for reaction removal
        let reaction_content =
            format!("reaction_remove:{}", reaction_emoji_name(&removed_reaction.emoji));
        let incoming = IncomingMessage::new(
            removed_reaction
                .user_id
                .map(|id| id.get().to_string())
                .unwrap_or_default(),
            removed_reaction.channel_id.get().to_string(),
            reaction_content,
        )
        .with_metadata(
            MessageMetadata::new()
                .with_extra("message_id", removed_reaction.message_id.get())
                .with_extra("reaction_emoji", reaction_emoji_name(&removed_reaction.emoji))
                .with_extra("reaction_type", "remove"),
        );

        // Send to handler if configured
        if let Some(tx) = &self.config.message_tx {
            if tx.send(incoming).is_err() {
                warn!("Discord event handler send failed: receiver closed");
            }
        }
    }

    /// Handle all reactions being removed from a message
    async fn reaction_remove_all(
        &self,
        _ctx: Context,
        channel_id: serenity::model::id::ChannelId,
        message_id: serenity::model::id::MessageId,
    ) {
        debug!("All reactions removed from message {}", message_id);

        let incoming = IncomingMessage::new(
            "system",
            channel_id.get().to_string(),
            "reaction_remove_all".to_string(),
        )
        .with_metadata(
            MessageMetadata::new()
                .with_extra("message_id", message_id.get())
                .with_extra("reaction_type", "remove_all"),
        );

        if let Some(tx) = &self.config.message_tx {
            if tx.send(incoming).is_err() {
                warn!("Discord event handler send failed: receiver closed");
            }
        }
    }
}

/// Get emoji name for reaction
#[cfg(feature = "discord")]
fn reaction_emoji_name(emoji: &ReactionType) -> String {
    match emoji {
        ReactionType::Unicode(s) => s.clone(),
        ReactionType::Custom { animated: _, id, name } => {
            name.clone().unwrap_or_else(|| id.get().to_string())
        }
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discord_config() {
        let config = DiscordConfig::new("test_token")
            .allow_user_ids(vec![123456789])
            .with_prefix("!");

        assert_eq!(config.token, "test_token");
        assert_eq!(config.allowed_user_ids.len(), 1);
        assert_eq!(config.command_prefix, "!");
    }

    #[test]
    fn test_format_for_discord() {
        let md = "**bold** and *italic* and `code`";
        let formatted = DiscordChannel::format_for_discord(md);
        // Discord supports standard markdown, so it should remain similar
        assert!(formatted.contains("**bold**"));
        assert!(formatted.contains("*italic*"));
        assert!(formatted.contains("`code`"));
    }
}
