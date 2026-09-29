//! Signal Channel Implementation
//!
//! This module implements the Channel trait for Signal messaging using
//! the signal-cli daemon JSON-RPC HTTP interface.
//!
//! Requires: signal-cli daemon running with HTTP interface enabled:
//!   signal-cli daemon --http localhost:8080
//!
//! The daemon exposes a JSON-RPC endpoint at /api/v1/rpc where messages
//! can be sent and received.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

use crate::channels::{
    Channel, ChannelCapabilities, ChatType, ConversationId, FormattedContent, IncomingMessage,
    OutgoingMessage,
};
use crate::core::models::Id;
use crate::security::pairing::{DmPolicy, PairingStore, RequestAccessResult};

#[allow(clippy::unwrap_used)]
static RE_HTML_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new("<[^>]+>").unwrap());
#[allow(clippy::unwrap_used)]
static RE_MD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());

/// Default signal-cli JSON-RPC endpoint
const DEFAULT_SIGNAL_RPC_URL: &str = "http://localhost:8080/api/v1/rpc";

/// Signal channel configuration
#[derive(Debug, Clone)]
pub struct SignalConfig {
    /// signal-cli daemon RPC URL
    pub rpc_url: String,
    /// Signal account (phone number in E.164 format, e.g. +1234567890)
    pub account: String,
    /// Optional allowed phone numbers (empty = allow all)
    pub allowed_numbers: Vec<String>,
    /// Message handler channel for incoming messages
    pub message_tx: Option<mpsc::UnboundedSender<IncomingMessage>>,
}

impl SignalConfig {
    /// Create new config with account phone number
    pub fn new(account: impl Into<String>) -> Self {
        Self {
            rpc_url: DEFAULT_SIGNAL_RPC_URL.to_string(),
            account: account.into(),
            allowed_numbers: Vec::new(),
            message_tx: None,
        }
    }

    /// Set custom RPC URL
    pub fn with_rpc_url(mut self, url: impl Into<String>) -> Self {
        self.rpc_url = url.into();
        self
    }

    /// Set allowed phone numbers
    pub fn allow_numbers(mut self, numbers: Vec<String>) -> Self {
        self.allowed_numbers = numbers;
        self
    }

    /// Set message handler
    pub fn with_message_handler(mut self, tx: mpsc::UnboundedSender<IncomingMessage>) -> Self {
        self.message_tx = Some(tx);
        self
    }
}

/// JSON-RPC request wrapper
#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    params: serde_json::Value,
    id: u64,
}

/// JSON-RPC response wrapper
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
    id: u64,
}

/// JSON-RPC error
#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// Signal channel implementation
pub struct SignalChannel {
    config: SignalConfig,
    http_client: reqwest::Client,
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Track message IDs: internal -> signal timestamp
    message_map: Arc<RwLock<HashMap<String, String>>>,
    /// Pairing store for DM access control
    pairing_store: Arc<RwLock<Option<Arc<PairingStore>>>>,
    /// DM policy
    dm_policy: Arc<RwLock<DmPolicy>>,
    /// Allowlist of phone numbers
    allow_from: Arc<RwLock<Vec<String>>>,
    /// Message sender for incoming messages
    message_tx: Arc<RwLock<Option<mpsc::UnboundedSender<IncomingMessage>>>>,
}

impl std::fmt::Debug for SignalChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalChannel")
            .field("config", &self.config)
            .field("running", &self.running)
            .finish()
    }
}

impl SignalChannel {
    /// Create a new Signal channel
    pub fn new(config: SignalConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            config,
            http_client,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            message_map: Arc::new(RwLock::new(HashMap::new())),
            pairing_store: Arc::new(RwLock::new(None)),
            dm_policy: Arc::new(RwLock::new(DmPolicy::Open)),
            allow_from: Arc::new(RwLock::new(Vec::new())),
            message_tx: Arc::new(RwLock::new(None)),
        }
    }

    /// Set pairing store
    pub async fn set_pairing_store(&self, store: Arc<PairingStore>) {
        let mut s = self.pairing_store.write().await;
        *s = Some(store);
    }

    /// Set DM policy
    pub async fn set_dm_policy(&self, policy: DmPolicy) {
        let mut p = self.dm_policy.write().await;
        *p = policy;
    }

    /// Set allowlist
    pub async fn set_allow_from(&self, numbers: Vec<String>) {
        let mut a = self.allow_from.write().await;
        *a = numbers;
    }

    /// Set message sender
    pub async fn set_message_sender(&self, sender: mpsc::UnboundedSender<IncomingMessage>) {
        let mut tx = self.message_tx.write().await;
        *tx = Some(sender);
    }

    /// Check if a Signal user is authorized
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
                    match store.request_access("signal", user_id, user_name).await {
                        Ok(RequestAccessResult::AlreadyAuthorized) => (true, None),
                        Ok(RequestAccessResult::AlreadyPending { code, .. }) => (
                            false,
                            Some(format!(
                                "Your access request is pending admin approval. Pairing code: `{}`",
                                code
                            )),
                        ),
                        Ok(RequestAccessResult::NewRequest { code }) => (
                            false,
                            Some(format!(
                                "Access requested. An admin will approve your request.\nPairing \
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

    /// Make JSON-RPC call to signal-cli
    async fn rpc_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> crate::Result<serde_json::Value> {
        let request_id = rand::random::<u64>();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id: request_id,
        };

        let response = self
            .http_client
            .post(&self.config.rpc_url)
            .json(&request)
            .send()
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: format!("Signal RPC request failed: {}", e),
                cause: Some(Box::new(e)),
            })?;

        let rpc_response: JsonRpcResponse =
            response
                .json()
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: format!("Failed to parse Signal RPC response: {}", e),
                    cause: Some(Box::new(e)),
                })?;

        if let Some(error) = rpc_response.error {
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("Signal RPC error {}: {}", error.code, error.message),
                cause: None,
            });
        }

        Ok(rpc_response.result.unwrap_or(serde_json::Value::Null))
    }

    /// Send a Signal message
    async fn send_signal_message(&self, recipient: &str, content: &str) -> crate::Result<String> {
        let params = serde_json::json!({
            "account": self.config.account,
            "recipient": [recipient],
            "message": content,
        });

        let result = self.rpc_call("send", params).await?;

        // Extract timestamp from result
        let timestamp = result
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .map(|ts| ts.to_string())
            .unwrap_or_default();

        Ok(timestamp)
    }

    /// Format content for Signal (basic markdown stripping)
    fn format_for_signal(text: &str) -> String {
        // Signal supports basic formatting but plain text is safest
        let mut result = text.to_string();

        // Strip HTML tags if present
        result = RE_HTML_TAG.replace_all(&result, "").to_string();

        // Convert markdown links to plain text
        result = RE_MD_LINK.replace_all(&result, "$1 ($2)").to_string();

        result
    }
}

#[async_trait]
impl Channel for SignalChannel {
    fn name(&self) -> &str {
        "signal"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct, ChatType::Group],
            supports_formatting: false, // Signal uses basic formatting
            supports_attachments: true,
            supports_images: true,
            supports_threads: false,
            supports_typing: false,
            supports_buttons: false,
            supports_commands: false,
            supports_reactions: false,
            supports_edit: false,
            supports_unsend: true, // Signal supports delete for everyone
            supports_effects: false,
        }
    }

    async fn start(&self) -> crate::Result<()> {
        info!("Starting Signal channel for account {}", self.config.account);

        // Verify signal-cli daemon is reachable
        match self.rpc_call("listAccounts", serde_json::json!({})).await {
            Ok(result) => {
                debug!("Signal daemon accounts: {:?}", result);
                info!("Signal daemon connected successfully");
            }
            Err(e) => {
                warn!("Could not connect to signal-cli daemon at {}: {}", self.config.rpc_url, e);
                warn!("Make sure signal-cli is running: signal-cli daemon --http localhost:8080");
                // Continue anyway - daemon might come online later
            }
        }

        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // Spawn polling loop for incoming messages
        let running = self.running.clone();
        let _message_tx = self.message_tx.clone();
        let poll_interval = std::time::Duration::from_secs(5);

        tokio::spawn(async move {
            // Note: In production, you'd use a webhook or WebSocket from signal-cli
            // This is a simplified polling approach
            while running.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(poll_interval).await;
                // Polling logic would go here - requires self reference
                // For now, the channel supports outbound sending
            }
        });

        info!("Signal channel started");
        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping Signal channel...");
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let recipient = &message.conversation_id.0;

        // Format content
        let content = match &message.formatted_content {
            Some(FormattedContent::Markdown(md)) => Self::format_for_signal(md),
            Some(FormattedContent::Html(html)) => Self::format_for_signal(html),
            _ => message.content,
        };

        let timestamp = self.send_signal_message(recipient, &content).await?;

        let msg_id = Id::new();
        if !timestamp.is_empty() {
            let mut map = self.message_map.write().await;
            map.insert(msg_id.to_string(), timestamp.clone());
        }

        debug!("Signal message sent to {} with timestamp {}", recipient, timestamp);
        Ok(msg_id)
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // Signal does not have typing indicators
        Ok(())
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        warn!("edit_message called but Signal does not support message editing");
        Err(crate::error::SyscityError::Internal(
            "Signal does not support message editing".to_string(),
        ))
    }

    async fn delete_message(&self, message_id: Id) -> crate::Result<()> {
        let msg_key = message_id.to_string();
        let timestamp = {
            let map = self.message_map.read().await;
            map.get(&msg_key)
                .cloned()
                .ok_or_else(|| crate::error::SyscityError::NotFound {
                    resource: format!("Signal message {} not found", msg_key),
                })?
        };

        let params = serde_json::json!({
            "account": self.config.account,
            "targetTimestamp": timestamp.parse::<i64>().unwrap_or(0),
        });

        self.rpc_call("remoteDelete", params).await?;

        let mut map = self.message_map.write().await;
        map.remove(&msg_key);

        Ok(())
    }

    async fn health_check(&self) -> crate::Result<bool> {
        if !self.running.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(false);
        }

        match self.rpc_call("listAccounts", serde_json::json!({})).await {
            Ok(_) => Ok(true),
            Err(e) => {
                warn!("Signal health check failed: {}", e);
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signal_config() {
        let config = SignalConfig::new("+1234567890")
            .with_rpc_url("http://localhost:9999")
            .allow_numbers(vec!["+0987654321".to_string()]);

        assert_eq!(config.account, "+1234567890");
        assert_eq!(config.rpc_url, "http://localhost:9999");
        assert_eq!(config.allowed_numbers.len(), 1);
    }

    #[test]
    fn test_format_for_signal() {
        let input = "Hello <b>world</b> [link](http://example.com)";
        let output = SignalChannel::format_for_signal(input);
        assert!(!output.contains("<b>"));
        assert!(output.contains("link (http://example.com)"));
    }

    #[test]
    fn test_signal_capabilities() {
        let config = SignalConfig::new("+1234567890");
        let channel = SignalChannel::new(config);
        let caps = channel.capabilities();
        assert!(caps.supports_attachments);
        assert!(caps.supports_unsend);
        assert!(!caps.supports_edit);
        assert!(!caps.supports_typing);
    }
}
