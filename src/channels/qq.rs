//! QQ Channel Implementation
//!
//! This module implements the Channel trait for QQ using the Tencent QQ Bot
//! API. Requires: Tencent developer account and bot registration.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

use crate::channels::{
    Channel, ChannelCapabilities, ConversationId, FormattedContent, IncomingMessage,
    OutgoingMessage,
};
use crate::core::models::Id;
use crate::security::pairing::{DmPolicy, PairingStore, RequestAccessResult};

/// QQ Bot API base URL
const QQ_API_BASE: &str = "https://api.sgroup.qq.com";
const QQ_SANDBOX_BASE: &str = "https://sandbox.api.sgroup.qq.com";

/// QQ channel configuration
#[derive(Debug, Clone)]
pub struct QqConfig {
    /// App ID from QQ Open Platform
    pub app_id: String,
    /// App Secret from QQ Open Platform
    pub app_secret: String,
    /// Bot QQ number
    pub bot_qq: String,
    /// Access token
    pub access_token: String,
    /// Optional allowed QQ numbers (empty = allow all)
    pub allowed_qqs: Vec<String>,
    /// Use sandbox environment
    pub use_sandbox: bool,
    /// Intents (bitmap)
    pub intents: u32,
    /// Message handler channel for inbound pipeline
    pub message_tx: Option<mpsc::UnboundedSender<IncomingMessage>>,
}

impl QqConfig {
    /// Create new config with app credentials
    pub fn new(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        bot_qq: impl Into<String>,
    ) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            bot_qq: bot_qq.into(),
            access_token: String::new(),
            allowed_qqs: Vec::new(),
            use_sandbox: false,
            intents: (1 << 30) | (1 << 0), // GUILD_MESSAGES (30) | AT_MESSAGES (0)
            message_tx: None,
        }
    }

    /// Set access token
    pub fn with_access_token(mut self, token: impl Into<String>) -> Self {
        self.access_token = token.into();
        self
    }

    /// Set allowed QQ numbers
    pub fn allow_qqs(mut self, qqs: Vec<String>) -> Self {
        self.allowed_qqs = qqs;
        self
    }

    /// Use sandbox environment
    pub fn with_sandbox(mut self, use_sandbox: bool) -> Self {
        self.use_sandbox = use_sandbox;
        self
    }

    /// Set intents
    pub fn with_intents(mut self, intents: u32) -> Self {
        self.intents = intents;
        self
    }

    /// Get base URL
    fn base_url(&self) -> &str {
        if self.use_sandbox {
            QQ_SANDBOX_BASE
        } else {
            QQ_API_BASE
        }
    }
}

/// QQ API response wrapper
#[derive(Debug, Deserialize)]
struct QqResponse<T> {
    code: i32,
    message: String,
    data: Option<T>,
}

/// QQ message request
#[derive(Debug, Serialize)]
struct QqMessageRequest {
    #[serde(rename = "guild_id", skip_serializing_if = "Option::is_none")]
    guild_id: Option<String>,
    #[serde(rename = "channel_id", skip_serializing_if = "Option::is_none")]
    channel_id: Option<String>,
    content: String,
    #[serde(rename = "msg_id", skip_serializing_if = "Option::is_none")]
    msg_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    markdown: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keyboard: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ark: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
}

/// QQ access token response
#[derive(Debug, Deserialize)]
struct QqTokenResponse {
    #[serde(rename = "access_token")]
    access_token: String,
    #[serde(rename = "expires_in")]
    #[allow(dead_code)]
    expires_in: i64,
}

/// QQ channel implementation
pub struct QqChannel {
    config: QqConfig,
    http_client: reqwest::Client,
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Track message IDs
    message_map: Arc<RwLock<HashMap<String, String>>>,
    /// Current access token (may be refreshed)
    current_token: Arc<RwLock<String>>,
    /// Pairing store for DM access control
    pairing_store: Arc<RwLock<Option<Arc<PairingStore>>>>,
    /// DM policy for access control
    dm_policy: Arc<RwLock<DmPolicy>>,
    /// Allowlist for QQ numbers (used with Allowlist policy)
    allow_from: Arc<RwLock<Vec<String>>>,
    /// Inbound message sender for pipeline integration
    message_tx: Option<mpsc::UnboundedSender<IncomingMessage>>,
}

impl std::fmt::Debug for QqChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QqChannel")
            .field("config", &self.config)
            .field("running", &self.running)
            .finish()
    }
}

impl QqChannel {
    /// Create a new QQ channel
    pub fn new(config: QqConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let initial_token = config.access_token.clone();
        let message_tx = config.message_tx.clone();

        Self {
            config,
            http_client,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            message_map: Arc::new(RwLock::new(HashMap::new())),
            current_token: Arc::new(RwLock::new(initial_token)),
            pairing_store: Arc::new(RwLock::new(None)),
            dm_policy: Arc::new(RwLock::new(DmPolicy::Open)),
            allow_from: Arc::new(RwLock::new(Vec::new())),
            message_tx,
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

    /// Set allowlist of QQ numbers
    pub async fn set_allow_from(&self, qqs: Vec<String>) {
        let mut a = self.allow_from.write().await;
        *a = qqs;
    }

    /// Check if a QQ user is authorized to interact.
    ///
    /// Returns `(is_authorized, optional_reply_message)`. Callers
    /// (webhook/event handlers) should send the reply to the user when
    /// `is_authorized` is false.
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
                    (false, Some("您无权使用此机器人。".to_string()))
                }
            }
            DmPolicy::Pairing => {
                let store_guard = self.pairing_store.read().await;
                if let Some(store) = store_guard.as_ref() {
                    match store.request_access("qq", user_id, user_name).await {
                        Ok(RequestAccessResult::AlreadyAuthorized) => (true, None),
                        Ok(RequestAccessResult::AlreadyPending { code, .. }) => (
                            false,
                            Some(format!("您的接入申请正在等待管理员审批。配对码：{}", code)),
                        ),
                        Ok(RequestAccessResult::NewRequest { code }) => (
                            false,
                            Some(format!(
                                "已提交接入申请，请等待管理员审批。\n您的配对码：{}",
                                code
                            )),
                        ),
                        Ok(RequestAccessResult::RateLimited { .. }) => {
                            (false, Some("请求过于频繁，请稍后再试。".to_string()))
                        }
                        Err(_) => (false, Some("处理请求时发生错误。".to_string())),
                    }
                } else {
                    (false, Some("访问控制未配置。".to_string()))
                }
            }
        }
    }

    /// Check if QQ number is allowed (legacy; prefer `check_access` for
    /// policy-aware checks)
    fn is_qq_allowed(&self, qq: &str) -> bool {
        if self.config.allowed_qqs.is_empty() {
            return true;
        }
        self.config.allowed_qqs.iter().any(|q| q == qq)
    }

    /// Get access token (refresh if needed)
    async fn get_access_token(&self) -> crate::Result<String> {
        let token = self.current_token.read().await.clone();
        if !token.is_empty() {
            return Ok(token);
        }

        // Need to get token using app credentials
        self.refresh_token().await
    }

    /// Refresh access token
    async fn refresh_token(&self) -> crate::Result<String> {
        let url = format!("{}/app/getAppAccessToken", self.config.base_url());

        let params = serde_json::json!({
            "appId": self.config.app_id,
            "clientSecret": self.config.app_secret,
        });

        let response = self
            .http_client
            .post(&url)
            .json(&params)
            .send()
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("Failed to get QQ access token: {}", e),
                cause: Some(Box::new(e)),
            })?;

        let token_resp: QqTokenResponse =
            response
                .json()
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Failed to parse QQ token response: {}", e),
                    cause: Some(Box::new(e)),
                })?;

        let mut token = self.current_token.write().await;
        *token = token_resp.access_token.clone();

        Ok(token_resp.access_token)
    }

    /// Make authenticated API request
    async fn api_request<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        endpoint: &str,
        payload: Option<serde_json::Value>,
    ) -> crate::Result<T> {
        let url = format!("{}{}", self.config.base_url(), endpoint);
        let token = self.get_access_token().await?;

        let mut request = self
            .http_client
            .request(method, &url)
            .header("Authorization", format!("QQBot {}", token))
            .header("X-Union-Appid", &self.config.app_id);

        if let Some(payload) = payload {
            request = request.json(&payload);
        }

        let response =
            request
                .send()
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("QQ API request failed: {}", e),
                    cause: Some(Box::new(e)),
                })?;

        let result: T =
            response
                .json()
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Failed to parse QQ response: {}", e),
                    cause: Some(Box::new(e)),
                })?;

        Ok(result)
    }

    /// Send message to channel (guild) or user (direct message).
    ///
    /// QQ Guild API uses `/channels/{channel_id}/messages` for both guild
    /// channels and DM channels (after a DM session is opened).  The Discord
    /// `/dms/{guild_id}/messages` endpoint does not exist in QQ Guild API.
    async fn send_message(&self, req: QqMessageRequest) -> crate::Result<String> {
        let channel_id = req
            .channel_id
            .as_ref()
            .or(req.guild_id.as_ref())
            .ok_or_else(|| {
                crate::error::SyscityError::Validation(
                    "Channel ID or Guild ID required".to_string(),
                )
            })?;

        let endpoint = format!("/channels/{}/messages", channel_id);

        let response: QqResponse<serde_json::Value> = self
            .api_request(
                reqwest::Method::POST,
                &endpoint,
                Some(serde_json::to_value(&req).unwrap_or(serde_json::Value::Null)),
            )
            .await?;

        if response.code != 0 {
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("QQ API error {}: {}", response.code, response.message),
                cause: None,
            });
        }

        // Extract message ID from response
        let msg_id = response
            .data
            .as_ref()
            .and_then(|d| d.get("id").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();

        Ok(msg_id)
    }

    /// Format content for QQ
    fn format_for_qq(text: &str) -> String {
        // QQ uses standard markdown mostly
        let mut result = text.to_string();

        // Bold: **text** -> **text* (QQ uses single asterisk variants or plain text)
        // QQ bot API supports markdown with limited formatting

        // Convert HTML-like formatting to markdown
        result = result.replace("<b>", "**").replace("</b>", "**");
        result = result.replace("<i>", "*").replace("</i>", "*");
        result = result.replace("<code>", "`").replace("</code>", "`");
        result = result.replace("<pre>", "```\n").replace("</pre>", "\n```");

        result
    }
}

#[async_trait]
impl Channel for QqChannel {
    fn name(&self) -> &str {
        "qq"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![
                crate::channels::ChatType::Direct,
                crate::channels::ChatType::Group,
                crate::channels::ChatType::Channel,
            ],
            supports_formatting: true,
            supports_attachments: true,
            supports_images: true,
            supports_threads: true,
            supports_typing: false,
            supports_buttons: true,
            supports_commands: true,
            supports_reactions: true,
            supports_edit: true,
            supports_unsend: false,
            supports_effects: false,
        }
    }

    async fn start(&self) -> crate::Result<()> {
        info!("Starting QQ channel...");

        // Test connection by getting access token
        if self.config.access_token.is_empty() {
            match self.refresh_token().await {
                Ok(token) => {
                    info!("QQ access token obtained successfully");
                    debug!("Token length: {}", token.len());
                }
                Err(e) => {
                    warn!("Failed to get QQ access token: {}", e);
                    // Continue anyway, might be configured with static token
                }
            }
        }

        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        info!("QQ channel started — connecting to WebSocket gateway");

        // WebSocket gateway reconnect loop (like Slack Socket Mode)
        let current_token = self.current_token.read().await.clone();
        let intents = self.config.intents;
        let message_tx = self.message_tx.clone();
        let running = self.running.clone();

        let mut backoff_secs = 1u64;
        const MAX_BACKOFF: u64 = 30;

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            match qq_get_gateway_url(self.config.base_url(), &current_token).await {
                Ok(ws_url) => {
                    backoff_secs = 1;

                    match qq_connect_and_listen(
                        &ws_url,
                        &current_token,
                        intents,
                        message_tx.as_ref(),
                        &running,
                    )
                    .await
                    {
                        Ok(()) => {
                            info!("QQ WebSocket: connection closed gracefully");
                        }
                        Err(e) => {
                            warn!(
                                "QQ WebSocket: connection error: {}. Reconnecting in {}s...",
                                e, backoff_secs
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "QQ WebSocket: failed to get gateway URL: {}. Retrying in {}s...",
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

        info!("QQ WebSocket: listener stopped");
        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping QQ channel...");
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let recipient = &message.conversation_id.0;

        // Check if allowed
        if !self.is_qq_allowed(recipient) {
            return Err(crate::error::SyscityError::Validation(format!(
                "QQ {} is not in allow list",
                recipient
            )));
        }

        // Format content
        let content = match &message.formatted_content {
            Some(FormattedContent::Markdown(md)) => Self::format_for_qq(md),
            Some(FormattedContent::Html(html)) => Self::format_for_qq(html),
            _ => message.content,
        };

        // Determine if it's a DM or guild message
        // QQ conversation IDs starting with "dm:" are direct messages
        let is_dm = recipient.starts_with("dm:");

        let req = if is_dm {
            QqMessageRequest {
                guild_id: Some(recipient.trim_start_matches("dm:").to_string()),
                channel_id: None,
                content,
                msg_id: None,
                markdown: None,
                keyboard: None,
                ark: None,
                image: None,
            }
        } else {
            QqMessageRequest {
                guild_id: None,
                channel_id: Some(recipient.to_string()),
                content,
                msg_id: None,
                markdown: None,
                keyboard: None,
                ark: None,
                image: None,
            }
        };

        let msg_id = self.send_message(req).await?;

        // Track message
        let mut map = self.message_map.write().await;
        map.insert(msg_id.clone(), recipient.to_string());

        debug!("QQ message sent to {} with ID {}", recipient, msg_id);
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // QQ doesn't have a typing indicator API
        Ok(())
    }

    async fn edit_message(&self, message_id: Id, new_content: String) -> crate::Result<()> {
        let msg_id_str = message_id.to_string();

        // Look up where this message was sent
        let channel_id = {
            let map = self.message_map.read().await;
            map.get(&msg_id_str)
                .cloned()
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Message {} not found", msg_id_str),
                })?
        };

        // QQ API: PATCH /channels/{channel_id}/messages/{message_id}
        let endpoint = format!("/channels/{}/messages/{}", channel_id, msg_id_str);

        let payload = serde_json::json!({
            "content": Self::format_for_qq(&new_content),
        });

        let response: QqResponse<serde_json::Value> = self
            .api_request(reqwest::Method::PATCH, &endpoint, Some(payload))
            .await?;

        if response.code != 0 {
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("QQ edit failed: {}", response.message),
                cause: None,
            });
        }

        Ok(())
    }

    async fn delete_message(&self, message_id: Id) -> crate::Result<()> {
        let msg_id_str = message_id.to_string();

        // Look up where this message was sent
        let channel_id = {
            let map = self.message_map.read().await;
            map.get(&msg_id_str)
                .cloned()
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Message {} not found", msg_id_str),
                })?
        };

        // QQ API: DELETE /channels/{channel_id}/messages/{message_id}
        let endpoint = format!("/channels/{}/messages/{}", channel_id, msg_id_str);

        let response: QqResponse<serde_json::Value> = self
            .api_request(reqwest::Method::DELETE, &endpoint, None)
            .await?;

        if response.code != 0 {
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("QQ delete failed: {}", response.message),
                cause: None,
            });
        }

        // Remove from tracking
        let mut map = self.message_map.write().await;
        map.remove(&msg_id_str);

        Ok(())
    }

    async fn health_check(&self) -> crate::Result<bool> {
        if !self.running.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(false);
        }

        // Try to get access token
        match self.get_access_token().await {
            Ok(_) => Ok(true),
            Err(e) => {
                warn!("QQ health check failed: {}", e);
                Ok(false)
            }
        }
    }
}

// ── QQ Guild Bot WebSocket Gateway
// ─────────────────────────────────────────────

const QQ_OP_HEARTBEAT: u64 = 1;
const QQ_OP_IDENTIFY: u64 = 2;
const QQ_OP_HELLO: u64 = 10;
const QQ_OP_HEARTBEAT_ACK: u64 = 11;

/// Fetch WebSocket gateway URL from QQ Guild Bot API.
async fn qq_get_gateway_url(base_url: &str, token: &str) -> crate::Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/gateway", base_url))
        .header("Authorization", format!("QQBot {}", token))
        .send()
        .await
        .map_err(|e| {
            crate::error::SyscityError::Internal(format!("QQ gateway request failed: {}", e))
        })?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();

    if !status.is_success() {
        return Err(crate::error::SyscityError::Internal(format!(
            "QQ gateway request failed: {} — {}",
            status,
            body["message"].as_str().unwrap_or("unknown")
        )));
    }

    let url = body["url"]
        .as_str()
        .ok_or_else(|| {
            crate::error::SyscityError::Internal(
                "QQ gateway response missing 'url' field".to_string(),
            )
        })?
        .to_string();

    Ok(url)
}

/// Connect to the QQ Guild Bot WebSocket gateway and listen for events.
async fn qq_connect_and_listen(
    ws_url: &str,
    token: &str,
    intents: u32,
    message_tx: Option<&mpsc::UnboundedSender<crate::channels::IncomingMessage>>,
    running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> crate::Result<()> {
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let (ws_stream, _) = connect_async(ws_url).await.map_err(|e| {
        crate::error::SyscityError::Internal(format!("QQ WebSocket connection failed: {}", e))
    })?;

    info!("QQ WebSocket: connected to gateway");

    let (mut write, mut read) = ws_stream.split();

    // ── Step 1: Wait for Hello (op: 10) ─────────────────────────────────
    let heartbeat_interval = loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                let msg: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                if msg["op"].as_u64() == Some(QQ_OP_HELLO) {
                    let interval = msg["d"]["heartbeat_interval"].as_u64().unwrap_or(45000);
                    info!("QQ WebSocket: received Hello, heartbeat interval={}ms", interval);
                    break interval;
                } else {
                    debug!("QQ WebSocket: received non-Hello message before identify: {}", text);
                }
            }
            Some(Ok(Message::Ping(_))) => {
                // Auto-pong handled by tungstenite
            }
            Some(Ok(Message::Close(frame))) => {
                warn!("QQ WebSocket: server closed connection before Hello: {:?}", frame);
                return Ok(());
            }
            Some(Err(e)) => {
                return Err(crate::error::SyscityError::Internal(format!(
                    "QQ WebSocket error during Hello: {}",
                    e
                )));
            }
            None => {
                return Err(crate::error::SyscityError::Internal(
                    "QQ WebSocket: connection closed before Hello".to_string(),
                ));
            }
            _ => {}
        }
    };

    // ── Step 2: Send Identify (op: 2) ────────────────────────────────────
    let identify = serde_json::json!({
        "op": QQ_OP_IDENTIFY,
        "d": {
            "token": token,
            "intents": intents,
            "shard": [0, 1]
        }
    });

    write
        .send(Message::Text(identify.to_string()))
        .await
        .map_err(|e| {
            crate::error::SyscityError::Internal(format!(
                "QQ WebSocket: failed to send Identify: {}",
                e
            ))
        })?;

    info!("QQ WebSocket: Identify sent");

    // ── Step 3: Heartbeat & event loop ───────────────────────────────────
    let heartbeat_duration = tokio::time::Duration::from_millis(heartbeat_interval);
    let mut heartbeat_interval_timer = tokio::time::interval(heartbeat_duration);
    let mut seq: Option<u64> = None;

    while running.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::select! {
            _ = heartbeat_interval_timer.tick() => {
                let hb = serde_json::json!({
                    "op": QQ_OP_HEARTBEAT,
                    "d": seq
                });
                if write.send(Message::Text(hb.to_string())).await.is_err() {
                    warn!("QQ WebSocket: heartbeat send failed");
                    break;
                }
                debug!("QQ WebSocket: heartbeat sent (seq={:?})", seq);
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        debug!("QQ WebSocket: received: {}", &text[..text.len().min(200)]);
                        let parsed: serde_json::Value =
                            serde_json::from_str(&text).unwrap_or_default();
                        let op = parsed["op"].as_u64().unwrap_or(99);

                        match op {
                            0 => {
                                // Dispatch — update sequence and handle event
                                if let Some(s) = parsed["s"].as_u64() {
                                    seq = Some(s);
                                }
                                let event_type = parsed["t"].as_str().unwrap_or("");

                                match event_type {
                                    "AT_MESSAGE_CREATE" | "DIRECT_MESSAGE_CREATE" => {
                                        if let Some(tx) = message_tx {
                                            if let Some(incoming) = parse_qq_message(&parsed["d"]) {
                                                let _ = tx.send(incoming);
                                            }
                                        }
                                    }
                                    "READY" => {
                                        info!("QQ WebSocket: ready — session_id={}",
                                            parsed["d"]["session_id"].as_str().unwrap_or("unknown"));
                                    }
                                    "RESUMED" => {
                                        info!("QQ WebSocket: resumed");
                                    }
                                    _ => {
                                        debug!("QQ WebSocket: unhandled dispatch event: {}", event_type);
                                    }
                                }
                            }
                            7 => {
                                warn!("QQ WebSocket: server requested reconnect (op: 7)");
                                break;
                            }
                            9 => {
                                warn!("QQ WebSocket: invalid session (op: 9) — re-identify needed");
                                // Reconnect will be handled by outer retry loop
                                break;
                            }
                            QQ_OP_HEARTBEAT_ACK => {
                                // Heartbeat ACK — nothing to do
                                debug!("QQ WebSocket: heartbeat ACK");
                            }
                            _ => {
                                debug!("QQ WebSocket: unhandled op: {}", op);
                            }
                        }
                    }
                    Some(Ok(Message::Ping(_))) => {
                        // Auto-pong handled by tungstenite
                    }
                    Some(Ok(Message::Close(frame))) => {
                        info!("QQ WebSocket: connection closed: {:?}", frame);
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("QQ WebSocket: read error: {}", e);
                        break;
                    }
                    None => {
                        info!("QQ WebSocket: read stream ended");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

/// Parse a QQ Guild Bot AT_MESSAGE_CREATE or DIRECT_MESSAGE_CREATE event
/// into an IncomingMessage.
fn parse_qq_message(data: &serde_json::Value) -> Option<crate::channels::IncomingMessage> {
    let content = data["content"].as_str()?;
    let author_id = data["author"]["id"].as_str()?;
    let _channel_id = data["channel_id"].as_str();
    let _guild_id = data["guild_id"].as_str();

    // Determine if DM
    let is_direct = data["guild_id"].is_null();

    Some(crate::channels::IncomingMessage {
        id: crate::core::models::Id::new(),
        user_id: crate::channels::UserId(author_id.to_string()),
        conversation_id: crate::channels::ConversationId(author_id.to_string()),
        content: content.to_string(),
        attachments: Vec::new(),
        metadata: crate::channels::MessageMetadata::default(),
        provenance: crate::channels::InputProvenance::ExternalUser {
            channel: "qq".to_string(),
            is_direct,
        },
        mention: if is_direct {
            crate::channels::MentionState::DirectMessage
        } else {
            crate::channels::MentionState::Mentioned
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qq_config() {
        let config = QqConfig::new("123456", "secret", "1234567890")
            .with_access_token("token123")
            .with_sandbox(true);

        assert_eq!(config.app_id, "123456");
        assert_eq!(config.app_secret, "secret");
        assert_eq!(config.bot_qq, "1234567890");
        assert_eq!(config.access_token, "token123");
        assert!(config.use_sandbox);
    }

    #[test]
    fn test_format_for_qq() {
        let input = "<b>bold</b> and <i>italic</i>";
        let output = QqChannel::format_for_qq(input);
        assert!(output.contains("**bold**"));
        assert!(output.contains("*italic*"));
    }

    #[test]
    fn test_base_url() {
        let config_sandbox = QqConfig::new("1", "s", "qq").with_sandbox(true);
        assert_eq!(config_sandbox.base_url(), QQ_SANDBOX_BASE);

        let config_prod = QqConfig::new("1", "s", "qq").with_sandbox(false);
        assert_eq!(config_prod.base_url(), QQ_API_BASE);
    }
}
