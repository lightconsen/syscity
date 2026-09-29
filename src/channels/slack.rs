//! Slack Channel Implementation
//!
//! This module implements the Channel trait for Slack using the Web API.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
#[cfg(feature = "slack")]
use futures::{SinkExt, StreamExt};
use regex::Regex;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

use crate::channels::{
    Channel, ChannelCapabilities, ChannelPolicy, ConversationId, FormattedContent, IncomingMessage,
    MentionState, MessageMetadata, OutgoingMessage, UserId,
};
use crate::core::models::Id;
use crate::security::pairing::{DmPolicy, PairingStore, RequestAccessResult};

#[allow(clippy::unwrap_used)]
static RE_BOLD_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
#[allow(clippy::unwrap_used)]
static RE_BOLD_UNDERSCORE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__(.+?)__").unwrap());
#[allow(clippy::unwrap_used)]
static RE_ITALIC_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*(.+?)\*").unwrap());
#[allow(clippy::unwrap_used)]
static RE_STRIKETHROUGH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~~(.+?)~~").unwrap());
#[allow(clippy::unwrap_used)]
static RE_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
#[allow(clippy::unwrap_used)]
static RE_UNDERSCORE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_(.+?)_").unwrap());
#[allow(clippy::unwrap_used)]
static RE_CODE_INLINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());

/// Slack channel configuration
#[derive(Debug, Clone)]
pub struct SlackConfig {
    /// Bot token (xoxb-...)
    pub bot_token: String,
    /// App token for Socket Mode (xapp-...)
    pub app_token: Option<String>,
    /// Optional allowed user IDs (empty = allow all)
    pub allowed_user_ids: Vec<String>,
    /// Message handler channel
    pub message_tx: Option<mpsc::UnboundedSender<IncomingMessage>>,
    /// Bot user ID (filled after connection)
    pub bot_user_id: Option<String>,
}

impl SlackConfig {
    /// Create new config with bot token
    pub fn new(bot_token: impl Into<String>) -> Self {
        Self {
            bot_token: bot_token.into(),
            app_token: None,
            allowed_user_ids: Vec::new(),
            message_tx: None,
            bot_user_id: None,
        }
    }

    /// Set app token for Socket Mode
    pub fn with_app_token(mut self, app_token: impl Into<String>) -> Self {
        self.app_token = Some(app_token.into());
        self
    }

    /// Set allowed user IDs
    pub fn allow_user_ids(mut self, user_ids: Vec<String>) -> Self {
        self.allowed_user_ids = user_ids;
        self
    }

    /// Set message handler
    pub fn with_message_handler(mut self, tx: mpsc::UnboundedSender<IncomingMessage>) -> Self {
        self.message_tx = Some(tx);
        self
    }
}

/// Slack channel implementation
pub struct SlackChannel {
    config: SlackConfig,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Maps our internal message ID -> (slack_channel_id, slack_ts) for
    /// edit/delete
    message_ids:
        std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<String, (String, String)>>>,
    /// Pairing store for DM access control
    pairing_store: Arc<RwLock<Option<Arc<PairingStore>>>>,
    /// DM policy for access control
    dm_policy: Arc<RwLock<DmPolicy>>,
    /// Allowlist for users (used with Allowlist policy)
    allow_from: Arc<RwLock<Vec<String>>>,
    /// Background Socket Mode task handle
    socket_mode_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl std::fmt::Debug for SlackChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackChannel")
            .field("config", &self.config)
            .field("running", &self.running)
            .finish()
    }
}

impl SlackChannel {
    /// Create a new Slack channel
    pub fn new(config: SlackConfig) -> Self {
        Self {
            config,
            running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            message_ids: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            pairing_store: Arc::new(RwLock::new(None)),
            dm_policy: Arc::new(RwLock::new(DmPolicy::Open)),
            allow_from: Arc::new(RwLock::new(Vec::new())),
            socket_mode_task: Arc::new(RwLock::new(None)),
        }
    }

    /// Set pairing store for DM access control
    pub async fn set_pairing_store(&self, store: Arc<PairingStore>) {
        let mut s = self.pairing_store.write().await;
        *s = Some(store);
    }

    /// Set DM policy
    pub async fn set_dm_policy(&self, policy: DmPolicy) {
        let mut p = self.dm_policy.write().await;
        *p = policy;
    }

    /// Set allowlist of user IDs
    pub async fn set_allow_from(&self, users: Vec<String>) {
        let mut a = self.allow_from.write().await;
        *a = users;
    }

    /// Check if a Slack user is authorized to interact.
    ///
    /// Returns `(is_authorized, optional_reply_message)`. Callers should send
    /// the reply message to the user when `is_authorized` is false.
    pub async fn check_access(
        &self,
        user_id: &str,
        user_name: Option<&str>,
    ) -> (bool, Option<String>) {
        let policy = *self.dm_policy.read().await;
        match policy {
            DmPolicy::Open => (true, None),
            DmPolicy::Allowlist => {
                let allow_from = self.allow_from.read().await;
                if allow_from.contains(&user_id.to_string()) {
                    (true, None)
                } else {
                    (false, Some("You are not authorized to use this bot.".to_string()))
                }
            }
            DmPolicy::Pairing => {
                let store_guard = self.pairing_store.read().await;
                if let Some(store) = store_guard.as_ref() {
                    match store.request_access("slack", user_id, user_name).await {
                        Ok(RequestAccessResult::AlreadyAuthorized) => (true, None),
                        Ok(RequestAccessResult::AlreadyPending { code, .. }) => (
                            false,
                            Some(format!(
                                "Your access request is pending admin approval. Your pairing \
                                 code: `{}`",
                                code
                            )),
                        ),
                        Ok(RequestAccessResult::NewRequest { code }) => (
                            false,
                            Some(format!(
                                "Access requested. An admin will approve your request.\nYour \
                                 pairing code: `{}`",
                                code
                            )),
                        ),
                        Ok(RequestAccessResult::RateLimited { .. }) => {
                            (false, Some("Too many requests. Please try again later.".to_string()))
                        }
                        Err(_) => {
                            (false, Some("An error occurred processing your request.".to_string()))
                        }
                    }
                } else {
                    (false, Some("Access control is not configured.".to_string()))
                }
            }
        }
    }

    /// Convert markdown to Slack mrkdwn format
    fn markdown_to_mrkdwn(text: &str) -> String {
        let mut result = text.to_string();

        // Use placeholders to protect patterns during conversion
        let bold_placeholder = "<<<BOLD>>>";
        let italic_placeholder = "<<<ITALIC>>>";

        // Step 1: Protect bold patterns (**text** and __text__)
        result = RE_BOLD_STAR
            .replace_all(&result, |caps: &regex::Captures<'_>| {
                format!("{}{}{}", bold_placeholder, &caps[1], bold_placeholder)
            })
            .to_string();
        result = RE_BOLD_UNDERSCORE
            .replace_all(&result, |caps: &regex::Captures<'_>| {
                format!("{}{}{}", bold_placeholder, &caps[1], bold_placeholder)
            })
            .to_string();

        // Step 2: Protect italic patterns (*text*)
        // These become <<<ITALIC>>>text<<<ITALIC>>>
        result = RE_ITALIC_STAR
            .replace_all(&result, |caps: &regex::Captures<'_>| {
                format!("{}{}{}", italic_placeholder, &caps[1], italic_placeholder)
            })
            .to_string();

        // Step 3: Restore bold placeholders as *text* (Slack bold)
        result = result.replace(bold_placeholder, "*");

        // Step 4: Restore italic placeholders as _text_ (Slack italic)
        result = result.replace(italic_placeholder, "_");

        // Strikethrough: ~~text~~ -> ~text~
        result = RE_STRIKETHROUGH.replace_all(&result, "~$1~").to_string();

        // Links: [text](url) -> <url|text>
        result = RE_LINK.replace_all(&result, "<$2|$1>").to_string();

        result
    }

    /// Strip markdown formatting for plain text fallback
    fn strip_markdown(text: &str) -> String {
        let mut result = text.to_string();

        // Protect patterns with placeholders, then strip the markers
        let bold_placeholder = "<<<BOLD>>>";
        let italic_placeholder = "<<<ITALIC>>>";

        // Protect bold patterns
        result = RE_BOLD_STAR
            .replace_all(&result, |caps: &regex::Captures<'_>| {
                format!("{}{}{}", bold_placeholder, &caps[1], bold_placeholder)
            })
            .to_string();
        result = RE_BOLD_UNDERSCORE
            .replace_all(&result, |caps: &regex::Captures<'_>| {
                format!("{}{}{}", bold_placeholder, &caps[1], bold_placeholder)
            })
            .to_string();

        // Protect italic patterns
        result = RE_ITALIC_STAR
            .replace_all(&result, |caps: &regex::Captures<'_>| {
                format!("{}{}{}", italic_placeholder, &caps[1], italic_placeholder)
            })
            .to_string();

        // Restore and strip bold placeholders (keep content only)
        result = result.replace(bold_placeholder, "");

        // Restore and strip italic placeholders (keep content only)
        result = result.replace(italic_placeholder, "");

        result = RE_UNDERSCORE.replace_all(&result, "$1").to_string();
        result = RE_CODE_INLINE.replace_all(&result, "$1").to_string();
        result = RE_LINK.replace_all(&result, "$1 ($2)").to_string();

        result
    }
}

#[async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &str {
        "slack"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![
                crate::channels::ChatType::Direct,
                crate::channels::ChatType::Channel,
                crate::channels::ChatType::Thread,
            ],
            supports_formatting: true,
            supports_attachments: true,
            supports_images: true,
            supports_threads: true,
            supports_typing: false, // Slack doesn't have typing indicators in the same way
            supports_buttons: true,
            supports_commands: true,
            supports_reactions: true,
            supports_edit: true,
            supports_unsend: true,
            supports_effects: false,
        }
    }

    async fn start(&self) -> crate::Result<()> {
        #[cfg(feature = "slack")]
        {
            info!("Starting Slack channel");

            // Test the connection using reqwest
            let client = reqwest::Client::new();
            let resp = client
                .post("https://slack.com/api/auth.test")
                .header("Authorization", format!("Bearer {}", self.config.bot_token))
                .send()
                .await
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!("Slack API request failed: {}", e))
                })?;

            let status = resp.status();
            if !status.is_success() {
                return Err(crate::error::SyscityError::Internal(format!(
                    "Slack API returned error: {}",
                    status
                )));
            }

            let auth_json: serde_json::Value = resp.json().await.unwrap_or_default();
            let bot_user_id = auth_json["user_id"].as_str().map(|s| s.to_string());
            info!(
                "Slack auth.test OK — bot_user_id={:?}",
                bot_user_id.as_deref().unwrap_or("unknown")
            );

            self.running
                .store(true, std::sync::atomic::Ordering::SeqCst);

            // Start Socket Mode listener if app_token is provided
            if let Some(app_token) = self.config.app_token.clone() {
                let running = self.running.clone();
                let bot_token = self.config.bot_token.clone();
                let message_tx = self.config.message_tx.clone();
                let policy = ChannelPolicy::new(
                    self.pairing_store.clone(),
                    self.dm_policy.clone(),
                    self.allow_from.clone(),
                );

                let handle = tokio::spawn(async move {
                    let mut backoff_secs = 1u64;
                    const MAX_BACKOFF: u64 = 30;

                    while running.load(std::sync::atomic::Ordering::SeqCst) {
                        info!("Slack Socket Mode: opening connection...");

                        match open_socket_mode_url(&app_token).await {
                            Ok(ws_url) => {
                                backoff_secs = 1; // Reset backoff on successful open

                                match connect_and_listen(
                                    &ws_url,
                                    &bot_token,
                                    bot_user_id.as_deref(),
                                    message_tx.as_ref(),
                                    &running,
                                    policy.clone(),
                                )
                                .await
                                {
                                    Ok(()) => {
                                        info!("Slack Socket Mode: connection closed gracefully");
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Slack Socket Mode: connection error: {}. \
                                             Reconnecting in {}s...",
                                            e, backoff_secs
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "Slack Socket Mode: failed to open connection: {}. Retrying \
                                     in {}s...",
                                    e, backoff_secs
                                );
                            }
                        }

                        if !running.load(std::sync::atomic::Ordering::SeqCst) {
                            break;
                        }

                        tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
                    }

                    info!("Slack Socket Mode: listener stopped");
                });

                let mut task_guard = self.socket_mode_task.write().await;
                *task_guard = Some(handle);
            }

            info!("Slack channel started");
            Ok(())
        }

        #[cfg(not(feature = "slack"))]
        {
            Err(crate::error::SyscityError::Internal("Slack feature not enabled".to_string()))
        }
    }

    async fn stop(&self) -> crate::Result<()> {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);

        let mut task_guard = self.socket_mode_task.write().await;
        if let Some(handle) = task_guard.take() {
            handle.abort();
            info!("Slack Socket Mode task aborted");
        }

        info!("Slack channel stopped");
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        #[cfg(feature = "slack")]
        {
            let channel_id = message.conversation_id.0.clone();

            // Format content
            let content = match &message.formatted_content {
                Some(FormattedContent::SlackMrkdwn(mrkdwn)) => mrkdwn.clone(),
                Some(FormattedContent::Markdown(md)) => Self::markdown_to_mrkdwn(md),
                Some(FormattedContent::Html(html)) => {
                    // Convert HTML to mrkdwn (simplified)
                    Self::strip_markdown(html)
                }
                _ => Self::markdown_to_mrkdwn(&message.content),
            };

            // Send message using reqwest with basic retry
            let client = reqwest::Client::new();
            let attempt_send = || async {
                client
                    .post("https://slack.com/api/chat.postMessage")
                    .header("Authorization", format!("Bearer {}", self.config.bot_token))
                    .header("Content-Type", "application/json")
                    .json(&serde_json::json!({
                        "channel": &channel_id,
                        "text": &content,
                    }))
                    .send()
                    .await
            };

            let resp = match attempt_send().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("Slack send failed (will retry once): {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    match attempt_send().await {
                        Ok(r) => r,
                        Err(e2) => {
                            return Err(crate::error::SyscityError::Internal(format!(
                                "Slack send failed after retry: {}",
                                e2
                            )));
                        }
                    }
                }
            };

            let resp_status = resp.status();
            let resp_json: serde_json::Value = resp.json().await.unwrap_or_default();

            if resp_status.is_success() && resp_json["ok"].as_bool().unwrap_or(false) {
                let slack_ts = resp_json["ts"].as_str().unwrap_or("").to_string();
                let slack_channel = resp_json["channel"]
                    .as_str()
                    .unwrap_or(&channel_id)
                    .to_string();
                let msg_id = Id::new();
                if !slack_ts.is_empty() {
                    let mut map = self.message_ids.write().await;
                    map.insert(msg_id.to_string(), (slack_channel, slack_ts));
                }
                debug!("Slack message sent successfully");
                Ok(msg_id)
            } else {
                Err(crate::error::SyscityError::Internal(format!(
                    "Slack API error: {} — {}",
                    resp_status,
                    resp_json["error"].as_str().unwrap_or("unknown")
                )))
            }
        }

        #[cfg(not(feature = "slack"))]
        {
            let _ = message;
            Err(crate::error::SyscityError::Internal("Slack feature not enabled".to_string()))
        }
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // Slack doesn't have typing indicators in the same way as other platforms
        Ok(())
    }

    async fn edit_message(&self, message_id: Id, new_content: String) -> crate::Result<()> {
        #[cfg(feature = "slack")]
        {
            let msg_key = message_id.to_string();
            let (slack_channel, slack_ts) = {
                let map = self.message_ids.read().await;
                map.get(&msg_key)
                    .cloned()
                    .ok_or_else(|| crate::error::SyscityError::NotFound {
                        resource: format!(
                            "Slack message {} not found in tracking (may have been sent before \
                             bot started)",
                            msg_key
                        ),
                    })?
            };

            let client = reqwest::Client::new();
            let resp = client
                .post("https://slack.com/api/chat.update")
                .header("Authorization", format!("Bearer {}", self.config.bot_token))
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "channel": slack_channel,
                    "ts": slack_ts,
                    "text": new_content,
                }))
                .send()
                .await
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!(
                        "Slack edit request failed: {}",
                        e
                    ))
                })?;

            let resp_json: serde_json::Value = resp.json().await.unwrap_or_default();
            if !resp_json["ok"].as_bool().unwrap_or(false) {
                return Err(crate::error::SyscityError::ExternalService {
                    source: format!(
                        "Slack chat.update failed: {}",
                        resp_json["error"].as_str().unwrap_or("unknown")
                    ),
                    cause: None,
                });
            }

            Ok(())
        }

        #[cfg(not(feature = "slack"))]
        {
            let _ = (message_id, new_content);
            Err(crate::error::SyscityError::Internal("Slack feature not enabled".to_string()))
        }
    }

    async fn delete_message(&self, message_id: Id) -> crate::Result<()> {
        #[cfg(feature = "slack")]
        {
            let msg_key = message_id.to_string();
            let (slack_channel, slack_ts) = {
                let map = self.message_ids.read().await;
                map.get(&msg_key)
                    .cloned()
                    .ok_or_else(|| crate::error::SyscityError::NotFound {
                        resource: format!(
                            "Slack message {} not found in tracking (may have been sent before \
                             bot started)",
                            msg_key
                        ),
                    })?
            };

            let client = reqwest::Client::new();
            let resp = client
                .post("https://slack.com/api/chat.delete")
                .header("Authorization", format!("Bearer {}", self.config.bot_token))
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "channel": slack_channel,
                    "ts": slack_ts,
                }))
                .send()
                .await
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!(
                        "Slack delete request failed: {}",
                        e
                    ))
                })?;

            let resp_json: serde_json::Value = resp.json().await.unwrap_or_default();
            if !resp_json["ok"].as_bool().unwrap_or(false) {
                return Err(crate::error::SyscityError::ExternalService {
                    source: format!(
                        "Slack chat.delete failed: {}",
                        resp_json["error"].as_str().unwrap_or("unknown")
                    ),
                    cause: None,
                });
            }

            // Remove from tracking map
            let mut map = self.message_ids.write().await;
            map.remove(&msg_key);

            Ok(())
        }

        #[cfg(not(feature = "slack"))]
        {
            let _ = message_id;
            Err(crate::error::SyscityError::Internal("Slack feature not enabled".to_string()))
        }
    }

    async fn health_check(&self) -> crate::Result<bool> {
        #[cfg(feature = "slack")]
        {
            // Simple check: verify we have a token and are running
            Ok(self.running.load(std::sync::atomic::Ordering::SeqCst)
                && !self.config.bot_token.is_empty())
        }

        #[cfg(not(feature = "slack"))]
        {
            Ok(false)
        }
    }
}

/// Open a Slack Socket Mode connection and return the WebSocket URL.
#[cfg(feature = "slack")]
async fn open_socket_mode_url(app_token: &str) -> crate::Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://slack.com/api/apps.connections.open")
        .header("Authorization", format!("Bearer {}", app_token))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send()
        .await
        .map_err(|e| {
            crate::error::SyscityError::Internal(format!(
                "Slack apps.connections.open request failed: {}",
                e
            ))
        })?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();

    if !status.is_success() || !body["ok"].as_bool().unwrap_or(false) {
        return Err(crate::error::SyscityError::Internal(format!(
            "Slack apps.connections.open failed: {} — {}",
            status,
            body["error"].as_str().unwrap_or("unknown")
        )));
    }

    let url = body["url"]
        .as_str()
        .ok_or_else(|| {
            crate::error::SyscityError::Internal(
                "Slack apps.connections.open response missing 'url' field".to_string(),
            )
        })?
        .to_string();

    Ok(url)
}

/// Connect to the Socket Mode WebSocket and listen for events.
#[cfg(feature = "slack")]
async fn connect_and_listen(
    ws_url: &str,
    bot_token: &str,
    bot_user_id: Option<&str>,
    message_tx: Option<&mpsc::UnboundedSender<IncomingMessage>>,
    running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    policy: ChannelPolicy,
) -> crate::Result<()> {
    use tokio_tungstenite::connect_async;

    let ws_stream =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), connect_async(ws_url))
            .await
            .map_err(|_| {
                crate::error::SyscityError::Internal(
                    "Slack Socket Mode WebSocket connection timed out after 5s".to_string(),
                )
            })?
            .map_err(|e| {
                crate::error::SyscityError::Internal(format!(
                    "Slack Socket Mode WebSocket connection failed: {}",
                    e
                ))
            })?;

    let (ws_stream, _) = ws_stream;

    info!("Slack Socket Mode: WebSocket connected");

    let (mut write, mut read) = ws_stream.split();

    while running.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(tokio_tungstenite::tungstenite::protocol::Message::Text(text))) => {
                        debug!("Slack Socket Mode: received text: {}", text);
                        handle_socket_mode_message(
                            &text,
                            bot_token,
                            bot_user_id,
                            message_tx,
                            &mut write,
                            policy.clone(),
                        )
                        .await;
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::protocol::Message::Close(_))) => {
                        info!("Slack Socket Mode: received close frame");
                        break;
                    }
                    Some(Ok(_)) => {
                        // Ignore other message types (binary, ping, pong)
                    }
                    Some(Err(e)) => {
                        warn!("Slack Socket Mode: WebSocket error: {}", e);
                        return Err(crate::error::SyscityError::Internal(format!(
                            "WebSocket error: {}",
                            e
                        )));
                    }
                    None => {
                        info!("Slack Socket Mode: WebSocket stream ended");
                        break;
                    }
                }
            }
        }
    }

    // Close the WebSocket gracefully
    let _ = write
        .send(tokio_tungstenite::tungstenite::protocol::Message::Close(None))
        .await;

    Ok(())
}

/// Handle a single Socket Mode message.
#[cfg(feature = "slack")]
async fn handle_socket_mode_message(
    text: &str,
    bot_token: &str,
    bot_user_id: Option<&str>,
    message_tx: Option<&mpsc::UnboundedSender<IncomingMessage>>,
    write: &mut (impl SinkExt<
        tokio_tungstenite::tungstenite::protocol::Message,
        Error = tokio_tungstenite::tungstenite::Error,
    > + Unpin),
    policy: ChannelPolicy,
) {
    let payload: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warn!("Slack Socket Mode: failed to parse message: {}", e);
            return;
        }
    };

    let envelope_id = payload["envelope_id"].as_str();

    // ACK every message that has an envelope_id
    if let Some(eid) = envelope_id {
        let ack = serde_json::json!({ "envelope_id": eid });
        if let Err(e) = write
            .send(tokio_tungstenite::tungstenite::protocol::Message::Text(ack.to_string()))
            .await
        {
            warn!("Slack Socket Mode: failed to send ACK: {}", e);
            return;
        }
        debug!("Slack Socket Mode: ACK sent for envelope_id={}", eid);
    }

    let msg_type = payload["type"].as_str().unwrap_or("");

    match msg_type {
        "hello" => {
            info!("Slack Socket Mode: received hello");
        }
        "events_api" => {
            if let Some(event) = payload["payload"]["event"].as_object() {
                let event_type = event["type"].as_str().unwrap_or("");
                match event_type {
                    "message" | "app_mention" => {
                        handle_event_message(
                            event,
                            bot_user_id,
                            message_tx,
                            bot_token,
                            policy.dm_policy.clone(),
                            policy.allow_from.clone(),
                            policy.pairing_store.clone(),
                        )
                        .await;
                    }
                    _ => {
                        debug!("Slack Socket Mode: ignoring event type: {}", event_type);
                    }
                }
            }
        }
        "disconnect" => {
            info!("Slack Socket Mode: received disconnect");
        }
        _ => {
            debug!("Slack Socket Mode: unhandled type: {}", msg_type);
        }
    }
}

/// Check access for a Slack user using the provided policy state.
#[cfg(feature = "slack")]
async fn check_access_inline(
    user_id: &str,
    dm_policy: &DmPolicy,
    allow_from: &[String],
    pairing_store: &Option<Arc<PairingStore>>,
) -> (bool, Option<String>) {
    match dm_policy {
        DmPolicy::Open => (true, None),
        DmPolicy::Allowlist => {
            if allow_from.contains(&user_id.to_string()) {
                (true, None)
            } else {
                (false, Some("You are not authorized to use this bot.".to_string()))
            }
        }
        DmPolicy::Pairing => {
            if let Some(store) = pairing_store {
                match store.request_access("slack", user_id, None).await {
                    Ok(RequestAccessResult::AlreadyAuthorized) => (true, None),
                    Ok(RequestAccessResult::AlreadyPending { code, .. }) => (
                        false,
                        Some(format!(
                            "Your access request is pending admin approval. Your pairing code: \
                             `{}`",
                            code
                        )),
                    ),
                    Ok(RequestAccessResult::NewRequest { code }) => (
                        false,
                        Some(format!(
                            "Access requested. An admin will approve your request.\nYour pairing \
                             code: `{}`",
                            code
                        )),
                    ),
                    Ok(RequestAccessResult::RateLimited { .. }) => {
                        (false, Some("Too many requests. Please try again later.".to_string()))
                    }
                    Err(_) => {
                        (false, Some("An error occurred processing your request.".to_string()))
                    }
                }
            } else {
                (false, Some("Access control is not configured.".to_string()))
            }
        }
    }
}

/// Handle a Slack message or app_mention event.
#[cfg(feature = "slack")]
async fn handle_event_message(
    event: &serde_json::Map<String, serde_json::Value>,
    bot_user_id: Option<&str>,
    message_tx: Option<&mpsc::UnboundedSender<IncomingMessage>>,
    bot_token: &str,
    dm_policy: Arc<RwLock<DmPolicy>>,
    allow_from: Arc<RwLock<Vec<String>>>,
    pairing_store: Arc<RwLock<Option<Arc<PairingStore>>>>,
) {
    // Ignore messages with subtypes (edits, deletions, bot messages, etc.)
    if event.contains_key("subtype") {
        let subtype = event["subtype"].as_str().unwrap_or("");
        if !subtype.is_empty() {
            debug!("Slack Socket Mode: ignoring message with subtype: {}", subtype);
            return;
        }
    }

    let event_user_id = event["user"].as_str().unwrap_or("");
    let event_channel = event["channel"].as_str().unwrap_or("");
    let event_text = event["text"].as_str().unwrap_or("").to_string();

    if event_user_id.is_empty() || event_channel.is_empty() {
        debug!("Slack Socket Mode: missing user or channel in event");
        return;
    }

    // Filter out messages from the bot itself
    if let Some(bid) = bot_user_id {
        if event_user_id == bid {
            debug!("Slack Socket Mode: ignoring bot's own message");
            return;
        }
    }

    // Determine if DM
    let is_dm = event_channel.starts_with('D');

    // Check access control
    let policy = *dm_policy.read().await;
    let allow_list = allow_from.read().await.clone();
    let store = pairing_store.read().await.clone();
    let (authorized, reply) =
        check_access_inline(event_user_id, &policy, &allow_list, &store).await;

    if !authorized {
        if let Some(reply_text) = reply {
            // Send access-denied reply back via Slack API
            let client = reqwest::Client::new();
            if let Err(e) = client
                .post("https://slack.com/api/chat.postMessage")
                .header("Authorization", format!("Bearer {}", bot_token))
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "channel": event_channel,
                    "text": reply_text,
                }))
                .send()
                .await
            {
                warn!("Slack: failed to send access-denied reply to {}: {}", event_channel, e);
            }
        }
        return;
    }

    let mention = if is_dm {
        MentionState::DirectMessage
    } else {
        MentionState::Mentioned
    };

    let detected = crate::tools::command_detector::detect_command(&event_text);

    let mut metadata = MessageMetadata::new();
    if let Some(ref result) = detected {
        metadata = metadata.with_detected_command(result);
    }

    let mut incoming = IncomingMessage {
        id: Id::new(),
        user_id: UserId::new(event_user_id),
        conversation_id: ConversationId::new(event_channel),
        content: event_text,
        attachments: vec![],
        metadata,
        provenance: crate::channels::InputProvenance::ExternalUser {
            channel: "slack".to_string(),
            is_direct: is_dm,
        },
        mention,
    };

    if detected.is_some() {
        let policy = crate::channels::ChannelPolicy::new(
            pairing_store.clone(),
            dm_policy.clone(),
            allow_from.clone(),
        );
        let auth_ctx = crate::channels::AuthContext::from_message(&incoming, &policy).await;
        incoming.metadata = incoming.metadata.with_auth_context(&auth_ctx);
    }

    if let Some(tx) = message_tx {
        if let Err(e) = tx.send(incoming) {
            warn!("Slack Socket Mode: failed to route message: {}", e);
        } else {
            debug!(
                "Slack Socket Mode: routed message from user={} channel={}",
                event_user_id, event_channel
            );
        }
    } else {
        warn!("Slack Socket Mode: no message_tx configured — message dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slack_config() {
        let config = SlackConfig::new("xoxb-short")
            .with_app_token("xapp-test-token")
            .allow_user_ids(vec!["U123".to_string()]);

        assert_eq!(config.bot_token, "xoxb-short");
        assert_eq!(config.app_token, Some("xapp-test-token".to_string()));
        assert_eq!(config.allowed_user_ids.len(), 1);
    }

    #[test]
    fn test_markdown_to_mrkdwn() {
        let md = "**bold** and *italic* and `code`";
        let mrkdwn = SlackChannel::markdown_to_mrkdwn(md);
        println!("Input: {}", md);
        println!("Output: {}", mrkdwn);
        assert!(mrkdwn.contains("*bold*"), "Expected *bold* in: {}", mrkdwn); // Slack bold is single asterisk
        assert!(mrkdwn.contains("_italic_"), "Expected _italic_ in: {}", mrkdwn); // Slack italic is underscore
        assert!(mrkdwn.contains("`code`"), "Expected `code` in: {}", mrkdwn); // Code stays the same
    }

    #[test]
    fn test_strip_markdown() {
        let md = "**bold** and [link](http://example.com)";
        let plain = SlackChannel::strip_markdown(md);
        assert!(plain.contains("bold"));
        assert!(!plain.contains("**"));
        assert!(plain.contains("link (http://example.com)"));
    }
}
