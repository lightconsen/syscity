//! Lark/Feishu Channel Implementation
//!
//! This module implements the Channel trait for Lark/Feishu using the ByteDance
//! Lark Open Platform API. Requires: ByteDance developer account and bot
//! registration.
//!
//! Feishu and Lark are the same platform (China / international branding of one
//! ByteDance API), so this is the single implementation for both — there is
//! deliberately no separate `feishu` module to alias it.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::channels::{
    Channel, ChannelCapabilities, ConversationId, FormattedContent, OutgoingMessage,
};
use crate::core::models::Id;
use crate::security::pairing::{DmPolicy, PairingStore, RequestAccessResult};

/// Lark API base URL
const LARK_API_BASE: &str = "https://open.feishu.cn/open-apis";

/// Lark/Feishu channel configuration
#[derive(Debug, Clone)]
pub struct LarkConfig {
    /// App ID from Lark/Feishu Open Platform
    pub app_id: String,
    /// App Secret from Lark/Feishu Open Platform
    pub app_secret: String,
    /// Verification token for webhook
    pub verification_token: String,
    /// Encrypt key for webhook (optional)
    pub encrypt_key: Option<String>,
    /// Optional allowed user IDs (empty = allow all)
    pub allowed_users: Vec<String>,
    /// Tenant access token (if already obtained)
    pub tenant_access_token: Option<String>,
}

impl LarkConfig {
    /// Create new config with app credentials
    pub fn new(app_id: impl Into<String>, app_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            verification_token: String::new(),
            encrypt_key: None,
            allowed_users: Vec::new(),
            tenant_access_token: None,
        }
    }

    /// Set verification token
    pub fn with_verification_token(mut self, token: impl Into<String>) -> Self {
        self.verification_token = token.into();
        self
    }

    /// Set encrypt key
    pub fn with_encrypt_key(mut self, key: impl Into<String>) -> Self {
        self.encrypt_key = Some(key.into());
        self
    }

    /// Set tenant access token
    pub fn with_tenant_token(mut self, token: impl Into<String>) -> Self {
        self.tenant_access_token = Some(token.into());
        self
    }

    /// Set allowed user IDs
    pub fn allow_users(mut self, users: Vec<String>) -> Self {
        self.allowed_users = users;
        self
    }
}

/// Lark API response wrapper
#[derive(Debug, Deserialize)]
struct LarkResponse<T> {
    code: i32,
    msg: String,
    data: Option<T>,
}

/// Lark token response
#[derive(Debug, Deserialize)]
struct LarkTokenResponse {
    #[serde(rename = "tenant_access_token")]
    tenant_access_token: String,
    #[serde(rename = "expire")]
    expire: i64,
}

/// Lark message content
#[derive(Debug, Serialize)]
#[serde(untagged)]
#[allow(dead_code)]
enum LarkMessageContent {
    Text { text: String },
    Post { post: serde_json::Value },
    Image { image_key: String },
    Interactive { card: serde_json::Value },
}

/// Lark send message request
#[derive(Debug, Serialize)]
struct LarkSendMessageRequest {
    #[serde(rename = "receive_id")]
    receive_id: String,
    #[serde(rename = "msg_type")]
    msg_type: String,
    content: String,
    #[serde(rename = "uuid", skip_serializing_if = "Option::is_none")]
    uuid: Option<String>,
}

/// Lark channel implementation
pub struct LarkChannel {
    config: LarkConfig,
    http_client: reqwest::Client,
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Track message IDs
    message_map: Arc<RwLock<HashMap<String, String>>>,
    /// Current tenant token (may be refreshed)
    current_token: Arc<RwLock<String>>,
    /// Token expiry time
    token_expiry: Arc<RwLock<Option<std::time::Instant>>>,
    /// Pairing store for DM access control
    pairing_store: Arc<RwLock<Option<Arc<PairingStore>>>>,
    /// DM policy for access control
    dm_policy: Arc<RwLock<DmPolicy>>,
    /// Allowlist for users (used with Allowlist policy)
    allow_from: Arc<RwLock<Vec<String>>>,
}

impl std::fmt::Debug for LarkChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LarkChannel")
            .field("config", &self.config)
            .field("running", &self.running)
            .finish()
    }
}

impl LarkChannel {
    /// Create a new Lark/Feishu channel
    pub fn new(config: LarkConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let initial_token = config.tenant_access_token.clone().unwrap_or_default();

        Self {
            config,
            http_client,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            message_map: Arc::new(RwLock::new(HashMap::new())),
            current_token: Arc::new(RwLock::new(initial_token)),
            token_expiry: Arc::new(RwLock::new(None)),
            pairing_store: Arc::new(RwLock::new(None)),
            dm_policy: Arc::new(RwLock::new(DmPolicy::Open)),
            allow_from: Arc::new(RwLock::new(Vec::new())),
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

    /// Check if a Lark user is authorized to interact.
    ///
    /// Returns `(is_authorized, optional_reply_message)`. Callers (webhook
    /// handlers) should send the reply to the user when `is_authorized` is
    /// false.
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
                    match store.request_access("lark", user_id, user_name).await {
                        Ok(RequestAccessResult::AlreadyAuthorized) => (true, None),
                        Ok(RequestAccessResult::AlreadyPending { code, .. }) => (
                            false,
                            Some(format!("您的接入申请正在等待管理员审批。配对码：`{}`", code)),
                        ),
                        Ok(RequestAccessResult::NewRequest { code }) => (
                            false,
                            Some(format!(
                                "已提交接入申请，请等待管理员审批。\n您的配对码：`{}`",
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

    /// Check if user is allowed (legacy; prefer `check_access` for policy-aware
    /// checks)
    fn is_user_allowed(&self, user_id: &str) -> bool {
        if self.config.allowed_users.is_empty() {
            return true;
        }
        self.config.allowed_users.iter().any(|u| u == user_id)
    }

    /// Get tenant access token (refresh if needed)
    async fn get_tenant_token(&self) -> crate::Result<String> {
        // Check if we have a valid cached token
        let should_refresh = {
            let expiry = self.token_expiry.read().await;
            match *expiry {
                Some(exp) => std::time::Instant::now() >= exp,
                None => true,
            }
        };

        if !should_refresh {
            let token = self.current_token.read().await.clone();
            if !token.is_empty() {
                return Ok(token);
            }
        }

        // Refresh token
        self.refresh_tenant_token().await
    }

    /// Refresh tenant access token
    async fn refresh_tenant_token(&self) -> crate::Result<String> {
        let url = format!("{}/auth/v3/tenant_access_token/internal", LARK_API_BASE);

        let params = serde_json::json!({
            "app_id": self.config.app_id,
            "app_secret": self.config.app_secret,
        });

        let response = self
            .http_client
            .post(&url)
            .json(&params)
            .send()
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("Failed to get Lark tenant token: {}", e),
                cause: Some(Box::new(e)),
            })?;

        let token_resp: LarkTokenResponse =
            response
                .json()
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Failed to parse Lark token response: {}", e),
                    cause: Some(Box::new(e)),
                })?;

        let mut token = self.current_token.write().await;
        *token = token_resp.tenant_access_token.clone();

        // Set expiry (with 5 minute buffer)
        let expiry_seconds = std::cmp::max(token_resp.expire - 300, 60);
        let mut expiry = self.token_expiry.write().await;
        *expiry =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(expiry_seconds as u64));

        Ok(token_resp.tenant_access_token)
    }

    /// Make authenticated API request
    async fn api_request<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        endpoint: &str,
        payload: Option<serde_json::Value>,
    ) -> crate::Result<T> {
        let url = format!("{}{}", LARK_API_BASE, endpoint);
        let token = self.get_tenant_token().await?;

        let mut request = self
            .http_client
            .request(method, &url)
            .header("Authorization", format!("Bearer {}", token));

        if let Some(payload) = payload {
            request = request.json(&payload);
        }

        let response =
            request
                .send()
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Lark API request failed: {}", e),
                    cause: Some(Box::new(e)),
                })?;

        let result: T =
            response
                .json()
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Failed to parse Lark response: {}", e),
                    cause: Some(Box::new(e)),
                })?;

        Ok(result)
    }

    /// Send message
    async fn send_message(
        &self,
        receive_id: &str,
        msg_type: &str,
        content: impl Serialize,
    ) -> crate::Result<String> {
        let content_str = serde_json::to_string(&content).map_err(|e| {
            crate::error::SyscityError::Validation(format!(
                "Failed to serialize message content: {}",
                e
            ))
        })?;

        let req = LarkSendMessageRequest {
            receive_id: receive_id.to_string(),
            msg_type: msg_type.to_string(),
            content: content_str,
            uuid: Some(uuid::Uuid::new_v4().to_string()),
        };

        let response: LarkResponse<serde_json::Value> = self
            .api_request(
                reqwest::Method::POST,
                "/im/v1/messages",
                Some(serde_json::to_value(&req).unwrap_or(serde_json::Value::Null)),
            )
            .await?;

        if response.code != 0 {
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("Lark API error {}: {}", response.code, response.msg),
                cause: None,
            });
        }

        // Extract message ID
        let msg_id = response
            .data
            .as_ref()
            .and_then(|d| d.get("message_id").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();

        Ok(msg_id)
    }

    /// Format content for Lark
    fn format_for_lark(text: &str) -> String {
        // Lark supports standard markdown in post messages
        // For text messages, it uses a simplified format

        // Bold: **text** is supported
        // Italic: *text* is supported
        // Links: [text](url) is supported

        text.to_string()
    }

    /// Build message content based on type
    fn build_content(text: &str, is_post: bool) -> serde_json::Value {
        if is_post {
            // Rich post format with markdown
            serde_json::json!({
                "zh_cn": {
                    "title": "",
                    "content": [
                        [{"tag": "text", "text": text}]
                    ]
                }
            })
        } else {
            // Simple text
            serde_json::json!({ "text": text })
        }
    }
}

#[async_trait]
impl Channel for LarkChannel {
    fn name(&self) -> &str {
        "lark"
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
            supports_reactions: false,
            supports_edit: true,
            supports_unsend: false,
            supports_effects: false,
        }
    }

    async fn start(&self) -> crate::Result<()> {
        info!("Starting Lark/Feishu channel...");

        // Test connection by getting tenant token
        match self.refresh_tenant_token().await {
            Ok(_) => {
                info!("Lark tenant token obtained successfully");
            }
            Err(e) => {
                warn!("Failed to get Lark tenant token: {}", e);
                // Continue anyway, might be configured with static token
            }
        }

        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        info!("Lark/Feishu channel started");
        info!("Note: Webhook configuration required for receiving messages");
        info!("Configure webhook at: https://open.feishu.cn/app");

        // Keep running until stopped
        while self.running.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }

        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping Lark/Feishu channel...");
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let recipient = &message.conversation_id.0;

        // Check if user is allowed
        if !self.is_user_allowed(recipient) {
            return Err(crate::error::SyscityError::Validation(format!(
                "User {} is not in allow list",
                recipient
            )));
        }

        // Format content
        let content = match &message.formatted_content {
            Some(FormattedContent::Markdown(md)) => Self::format_for_lark(md),
            Some(FormattedContent::Html(html)) => Self::format_for_lark(html),
            _ => message.content,
        };

        // Use post format for better markdown support
        let content_json = Self::build_content(&content, false);
        let msg_id = self.send_message(recipient, "text", content_json).await?;

        // Track message
        let mut map = self.message_map.write().await;
        map.insert(msg_id.clone(), recipient.to_string());

        debug!("Lark message sent to {} with ID {}", recipient, msg_id);
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // Lark doesn't support typing indicators
        Ok(())
    }

    async fn edit_message(&self, message_id: Id, new_content: String) -> crate::Result<()> {
        let msg_id_str = message_id.to_string();

        // Look up the recipient
        let _receive_id = {
            let map = self.message_map.read().await;
            map.get(&msg_id_str)
                .cloned()
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Message {} not found", msg_id_str),
                })?
        };

        let content_json = Self::build_content(&Self::format_for_lark(&new_content), false);
        let content_str = serde_json::to_string(&content_json).map_err(|e| {
            crate::error::SyscityError::Validation(format!("Failed to serialize: {}", e))
        })?;

        // Lark API: PATCH /im/v1/messages/{message_id}
        let endpoint = format!("/im/v1/messages/{}", msg_id_str);
        let payload = serde_json::json!({
            "content": content_str,
        });

        let response: LarkResponse<serde_json::Value> = self
            .api_request(reqwest::Method::PATCH, &endpoint, Some(payload))
            .await?;

        if response.code != 0 {
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("Lark edit failed: {}", response.msg),
                cause: None,
            });
        }

        Ok(())
    }

    async fn delete_message(&self, message_id: Id) -> crate::Result<()> {
        let msg_id_str = message_id.to_string();

        // Look up the recipient
        {
            let map = self.message_map.read().await;
            if !map.contains_key(&msg_id_str) {
                return Err(crate::error::SyscityError::NotFound {
                    resource: format!("Message {} not found", msg_id_str),
                });
            }
        }

        // Lark API: DELETE /im/v1/messages/{message_id}
        let endpoint = format!("/im/v1/messages/{}", msg_id_str);

        let response: LarkResponse<serde_json::Value> = self
            .api_request(reqwest::Method::DELETE, &endpoint, None)
            .await?;

        if response.code != 0 {
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("Lark delete failed: {}", response.msg),
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

        // Try to verify token
        match self.get_tenant_token().await {
            Ok(_) => Ok(true),
            Err(e) => {
                warn!("Lark health check failed: {}", e);
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lark_config() {
        let config = LarkConfig::new("cli_123456", "secret123")
            .with_verification_token("verify_123")
            .with_encrypt_key("encrypt_key_456");

        assert_eq!(config.app_id, "cli_123456");
        assert_eq!(config.app_secret, "secret123");
        assert_eq!(config.verification_token, "verify_123");
        assert_eq!(config.encrypt_key, Some("encrypt_key_456".to_string()));
    }

    #[test]
    fn test_format_for_lark() {
        let input = "**bold** and *italic*";
        let output = LarkChannel::format_for_lark(input);
        assert_eq!(output, "**bold** and *italic*"); // Lark supports standard
                                                     // markdown
    }

    #[test]
    fn test_build_content() {
        let text = "Hello world";
        let content = LarkChannel::build_content(text, false);
        assert_eq!(content["text"], "Hello world");
    }
}
