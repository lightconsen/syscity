//! Webhook Receivers - Public Tier
//!
//! These endpoints are publicly accessible for receiving callbacks from
//! external channel providers (WhatsApp, Telegram, Feishu, etc.).
//! Security is handled via HMAC signature verification per-channel.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use super::GatewayState;
use crate::channels::{ConversationId, IncomingMessage, InputProvenance, OutgoingMessage};

/// Query params for webhook verification (used by some platforms)
#[derive(Debug, Deserialize)]
pub struct WebhookVerifyQuery {
    /// Challenge token for verification
    pub hub_challenge: Option<String>,
    /// Verify token sent by platform
    pub hub_verify_token: Option<String>,
    /// Mode (subscribe/unsubscribe)
    pub hub_mode: Option<String>,
}

/// Generic webhook response
#[derive(Debug, Serialize)]
pub struct WebhookResponse {
    pub success: bool,
    pub message: String,
}

/// Session mapping for webhook-based channels (platform_id -> session_uuid)
/// This provides UUID-based sessions with /new command support
use std::collections::HashMap;

use tokio::sync::RwLock;

/// Get or create a session UUID for a platform user
async fn get_or_create_session(
    sessions: &RwLock<HashMap<String, String>>,
    platform_key: &str,
) -> String {
    let mut map = sessions.write().await;
    map.entry(platform_key.to_string())
        .or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

/// Reset session for a platform user (when /new is used)
async fn reset_session(sessions: &RwLock<HashMap<String, String>>, platform_key: &str) -> String {
    let new_session = uuid::Uuid::new_v4().to_string();
    let mut map = sessions.write().await;
    map.insert(platform_key.to_string(), new_session.clone());
    new_session
}

/// Create the public webhook router
pub fn create_webhook_router(state: Arc<GatewayState>) -> Router {
    let router = Router::new()
        // WhatsApp Business API webhooks
        .route("/webhooks/whatsapp", post(whatsapp_webhook_handler))
        .route("/webhooks/whatsapp/verify", get(whatsapp_verify_handler))
        // Telegram Bot API webhooks
        .route("/webhooks/telegram/:token", post(telegram_webhook_handler))
        // Feishu/Lark webhooks
        .route("/webhooks/feishu", post(feishu_webhook_handler))
        // Slack Events API webhooks
        .route("/webhooks/slack", post(slack_webhook_handler))
        // Generic webhook for custom integrations
        .route("/webhooks/:channel", post(generic_webhook_handler));

    // WeChat Official Account (公众号) webhooks (feature-gated channel)
    #[cfg(feature = "wechatmp")]
    let router = router.route(
        "/webhooks/wechatmp",
        get(wechatmp_verify_handler).post(wechatmp_webhook_handler),
    );

    router.with_state(state)
}

/// Verify WhatsApp webhook subscription (GET request for verification)
async fn whatsapp_verify_handler(
    Query(query): Query<WebhookVerifyQuery>,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    info!("WhatsApp webhook verification request");

    // Get verify token from config
    let expected_token = {
        let config = state.config.read().await;
        config
            .channels
            .get("whatsapp")
            .and_then(|c| c.credentials.get("verify_token"))
            .cloned()
    };

    match (query.hub_mode.as_deref(), query.hub_verify_token) {
        (Some("subscribe"), Some(token)) => {
            if expected_token.map(|t| t == token).unwrap_or(true) {
                // Return the challenge
                if let Some(challenge) = query.hub_challenge {
                    info!("WhatsApp webhook verified successfully");
                    return (StatusCode::OK, challenge).into_response();
                }
            }
            warn!("WhatsApp webhook verification failed: invalid token");
            StatusCode::FORBIDDEN.into_response()
        }
        _ => {
            warn!("WhatsApp webhook verification: invalid request");
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

/// Handle incoming WhatsApp messages with HMAC-SHA256 signature verification
async fn whatsapp_webhook_handler(
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: Bytes,
) -> impl IntoResponse {
    info!("Received WhatsApp webhook");

    // Get HMAC secret from config
    let hmac_secret = {
        let config = state.config.read().await;
        config
            .channels
            .get("whatsapp")
            .and_then(|c| c.credentials.get("app_secret"))
            .cloned()
    };

    // Verify HMAC signature if secret is configured
    if let Some(secret) = hmac_secret {
        let signature = headers
            .get("x-hub-signature-256")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.strip_prefix("sha256=").unwrap_or(s));

        if let Some(sig) = signature {
            if !verify_hmac_sha256(&secret, &body, sig) {
                warn!("WhatsApp webhook: invalid HMAC signature");
                return (StatusCode::UNAUTHORIZED, "Invalid signature").into_response();
            }
            debug!("WhatsApp webhook: HMAC signature verified");
        } else {
            warn!("WhatsApp webhook: missing signature");
            return (StatusCode::UNAUTHORIZED, "Missing signature").into_response();
        }
    }

    // Parse the webhook payload
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to parse WhatsApp webhook: {}", e);
            return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response();
        }
    };

    // Process webhook entries
    if let Some(entries) = payload.get("entry").and_then(|e| e.as_array()) {
        for entry in entries {
            if let Some(changes) = entry.get("changes").and_then(|c| c.as_array()) {
                for change in changes {
                    let value = change.get("value");

                    // Log and acknowledge statuses events (delivered, read, failed)
                    // instead of silently dropping them.
                    #[cfg(feature = "whatsapp")]
                    if let Some(v) = &value {
                        if let Some(statuses) = v.get("statuses") {
                            crate::channels::whatsapp::WhatsappChannel::handle_statuses_event(
                                statuses,
                            );
                        }
                    }

                    if let Some(messages) = value
                        .and_then(|v| v.get("messages"))
                        .and_then(|m| m.as_array())
                    {
                        for msg in messages {
                            if let (Some(from), Some(text_body)) = (
                                msg.get("from").and_then(|f| f.as_str()),
                                msg.get("text")
                                    .and_then(|t| t.get("body"))
                                    .and_then(|b| b.as_str()),
                            ) {
                                info!(
                                    "WhatsApp message from {}: {}",
                                    from,
                                    &text_body[..text_body.len().min(50)]
                                );

                                // Handle /new command to reset session
                                let platform_key = format!("whatsapp:{}", from);
                                let session_id = if text_body.trim() == "/new" {
                                    let new_session = reset_session(
                                        &state.channels.webhook_sessions,
                                        &platform_key,
                                    )
                                    .await;
                                    info!(
                                        "🆕 New WhatsApp session started for {}: {}",
                                        from, new_session
                                    );
                                    // Send confirmation message back to user
                                    let channel_opt = {
                                        let channels = state.channels.channels.read().await;
                                        channels.get("whatsapp").cloned()
                                    };
                                    if let Some(channel) = channel_opt {
                                        let confirmation = OutgoingMessage::new(
                                            ConversationId(from.to_string()),
                                            "✅ New session started. How can I help you?",
                                        );
                                        if let Err(e) = channel.send(confirmation).await {
                                            warn!(
                                                "Failed to send /new confirmation to {}: {}",
                                                from, e
                                            );
                                        }
                                    }
                                    new_session
                                } else {
                                    // Get or create session UUID
                                    get_or_create_session(
                                        &state.channels.webhook_sessions,
                                        &platform_key,
                                    )
                                    .await
                                };

                                // Store session mapping for response routing
                                {
                                    let mut sessions =
                                        state.channels.session_channels.write().await;
                                    sessions.insert(
                                        session_id.clone(),
                                        ("whatsapp".to_string(), from.to_string()),
                                    );
                                }

                                // Access control check
                                if state
                                    .check_incoming_access(
                                        "whatsapp",
                                        from,
                                        text_body,
                                        &crate::channels::MentionState::DirectMessage,
                                    )
                                    .await
                                    .is_err()
                                {
                                    continue;
                                }

                                // Route through unified inbound entry
                                let incoming =
                                    IncomingMessage::new(from, session_id.clone(), text_body)
                                        .with_provenance(InputProvenance::ExternalUser {
                                            channel: "whatsapp".to_string(),
                                            is_direct: true,
                                        });
                                if let Err(e) = state.pipelines.inbound_entry.send(incoming).await {
                                    warn!("Failed to enqueue WhatsApp message: {}", e);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Json(WebhookResponse {
        success: true,
        message: "Webhook received".to_string(),
    })
    .into_response()
}

/// Telegram webhook payload
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TelegramMessage {
    message_id: i64,
    from: Option<TelegramUser>,
    chat: TelegramChat,
    date: i64,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TelegramUser {
    id: i64,
    first_name: String,
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
}

/// Handle Telegram webhook with token-based verification
async fn telegram_webhook_handler(
    Path(token): Path<String>,
    State(state): State<Arc<GatewayState>>,
    Json(update): Json<TelegramUpdate>,
) -> impl IntoResponse {
    // Verify webhook token from URL path - required for all Telegram webhook
    // channels
    let expected_token = {
        let config = state.config.read().await;
        config
            .channels
            .get("telegram")
            .and_then(|c| c.credentials.get("webhook_token"))
            .cloned()
    };

    let expected = match expected_token {
        Some(t) if !t.is_empty() => t,
        _ => {
            warn!("Telegram webhook: webhook_token is required");
            return (StatusCode::UNAUTHORIZED, "Webhook token is required").into_response();
        }
    };

    if expected != token {
        warn!("Telegram webhook: invalid token");
        return (StatusCode::UNAUTHORIZED, "Invalid token").into_response();
    }
    debug!("Telegram webhook: token verified");

    // Process the update
    if let Some(message) = update.message {
        if let Some(text) = message.text {
            let user_id = message
                .from
                .as_ref()
                .map(|u| u.id.to_string())
                .unwrap_or_default();
            let chat_id = message.chat.id.to_string();

            info!(
                "Telegram message from {}: {}",
                user_id,
                text.chars().take(50).collect::<String>()
            );

            // Determine mention state from chat type
            let mention = match message.chat.chat_type.as_str() {
                "private" => crate::channels::MentionState::DirectMessage,
                _ => crate::channels::MentionState::NotMentioned,
            };

            // Access control check
            if state
                .check_incoming_access("telegram", &user_id, &text, &mention)
                .await
                .is_err()
            {
                return Json(WebhookResponse {
                    success: true,
                    message: "OK".to_string(),
                })
                .into_response();
            }

            // Route through unified inbound entry
            let incoming = IncomingMessage::new(user_id, format!("telegram:{}", chat_id), text)
                .with_provenance(InputProvenance::ExternalUser {
                    channel: "telegram".to_string(),
                    is_direct: true,
                });
            if let Err(e) = state.pipelines.inbound_entry.send(incoming).await {
                warn!("Failed to enqueue Telegram message: {}", e);
            }
        }
    }

    Json(WebhookResponse {
        success: true,
        message: "OK".to_string(),
    })
    .into_response()
}

/// Handle Feishu/Lark webhook with signature verification
async fn feishu_webhook_handler(
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: Bytes,
) -> impl IntoResponse {
    info!("Received Feishu webhook");

    // Get signature info from headers
    let signature = headers
        .get("x-lark-signature")
        .and_then(|v| v.to_str().ok());

    let timestamp = headers
        .get("x-lark-request-timestamp")
        .and_then(|v| v.to_str().ok());

    let nonce = headers
        .get("x-lark-request-nonce")
        .and_then(|v| v.to_str().ok());

    // Resolve the webhook secret from the secret store (falling back to the
    // legacy plaintext credentials map). The config guard is released before
    // the async resolution to avoid holding the lock across `.await`.
    let secret = {
        let legacy = {
            let config = state.config.read().await;
            config
                .channels
                .get("feishu")
                .and_then(|c| c.credentials.get("webhook_secret"))
                .cloned()
        };
        state
            .secrets
            .resolve_channel_credential("feishu", "webhook_secret", legacy.as_deref())
            .await
            .ok()
            .flatten()
            .map(|v| v.into_inner())
    };

    // Verify signature if secret and headers are present
    if let (Some(secret), Some(sig), Some(ts), Some(nonce)) = (secret, signature, timestamp, nonce)
    {
        if !verify_feishu_signature(&secret, ts, nonce, &body, sig) {
            warn!("Feishu webhook: invalid signature");
            return (StatusCode::UNAUTHORIZED, "Invalid signature").into_response();
        }
        debug!("Feishu webhook: signature verified");
    }

    // Parse the payload
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to parse Feishu webhook: {}", e);
            return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response();
        }
    };

    // Check if this is a challenge request (initial verification)
    if let Some(challenge) = payload.get("challenge").and_then(|v| v.as_str()) {
        return Json(serde_json::json!({
            "challenge": challenge
        }))
        .into_response();
    }

    // Extract message content from event
    if let Some(event) = payload.get("event") {
        if let (Some(message), Some(sender)) = (event.get("message"), event.get("sender")) {
            if let (Some(content), Some(sender_id)) = (
                message.get("content").and_then(|c| c.get("text")),
                sender.get("sender_id").and_then(|s| s.get("open_id")),
            ) {
                let text = content.as_str().unwrap_or_default();
                let user_id = sender_id.as_str().unwrap_or_default();

                info!(
                    "Feishu message from {}: {}",
                    user_id,
                    text.chars().take(50).collect::<String>()
                );

                // Access control check
                if state
                    .check_incoming_access(
                        "feishu",
                        user_id,
                        text,
                        &crate::channels::MentionState::DirectMessage,
                    )
                    .await
                    .is_err()
                {
                    return Json(WebhookResponse {
                        success: true,
                        message: "OK".to_string(),
                    })
                    .into_response();
                }

                // Handle /new command to reset session
                let platform_key = format!("feishu:{}", user_id);
                let session_id = if text.trim() == "/new" {
                    let new_session =
                        reset_session(&state.channels.webhook_sessions, &platform_key).await;
                    info!("🆕 New Feishu session started for {}: {}", user_id, new_session);
                    new_session
                } else {
                    // Get or create session UUID
                    get_or_create_session(&state.channels.webhook_sessions, &platform_key).await
                };

                // Store session mapping for response routing
                {
                    let mut sessions = state.channels.session_channels.write().await;
                    sessions
                        .insert(session_id.clone(), ("feishu".to_string(), user_id.to_string()));
                }

                // Route through unified inbound entry
                let incoming = IncomingMessage::new(user_id, session_id.clone(), text)
                    .with_provenance(InputProvenance::ExternalUser {
                        channel: "feishu".to_string(),
                        is_direct: true,
                    });
                if let Err(e) = state.pipelines.inbound_entry.send(incoming).await {
                    warn!("Failed to enqueue Feishu message: {}", e);
                }
            }
        }
    }

    Json(WebhookResponse {
        success: true,
        message: "OK".to_string(),
    })
    .into_response()
}

// ─────────────────────────────────────────────
// WeChat Official Account (公众号)
// ─────────────────────────────────────────────

/// Resolve one wechatmp credential from config, falling back to the secret
/// store. Returns `None` when unset.
#[cfg(feature = "wechatmp")]
async fn wechatmp_cred(state: &Arc<GatewayState>, key: &str) -> Option<String> {
    let legacy = {
        let config = state.config.read().await;
        config
            .channels
            .get("wechatmp")
            .and_then(|c| c.credentials.get(key))
            .cloned()
    };
    state
        .secrets
        .resolve_channel_credential("wechatmp", key, legacy.as_deref())
        .await
        .ok()
        .flatten()
        .map(|v| v.into_inner())
}

/// Resolve wechatmp credentials from config (with secret-store fallback).
#[cfg(feature = "wechatmp")]
async fn wechatmp_credentials(
    state: &Arc<GatewayState>,
) -> Option<crate::channels::wechatmp::WechatMpConfig> {
    Some(crate::channels::wechatmp::WechatMpConfig {
        app_id: wechatmp_cred(state, "app_id").await?,
        app_secret: wechatmp_cred(state, "app_secret").await?,
        token: wechatmp_cred(state, "token").await?,
        encoding_aes_key: wechatmp_cred(state, "encoding_aes_key").await?,
    })
}

/// WeChat MP verification query params (GET /webhooks/wechatmp).
#[cfg(feature = "wechatmp")]
#[derive(Debug, Deserialize)]
pub struct WechatMpVerifyQuery {
    pub signature: String,
    pub timestamp: String,
    pub nonce: String,
    pub echostr: String,
}

/// GET verification: echo `echostr` when the signature matches.
#[cfg(feature = "wechatmp")]
async fn wechatmp_verify_handler(
    Query(query): Query<WechatMpVerifyQuery>,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    use crate::channels::wechatmp::verify_echo_signature;

    let Some(credentials) = wechatmp_credentials(&state).await else {
        warn!("WeChat MP webhook verify: channel not configured");
        return StatusCode::FORBIDDEN.into_response();
    };
    if verify_echo_signature(&credentials.token, &query.timestamp, &query.nonce, &query.signature) {
        return (StatusCode::OK, query.echostr).into_response();
    }
    warn!("WeChat MP webhook verify: invalid signature");
    StatusCode::FORBIDDEN.into_response()
}

/// POST webhook: verify → decrypt → route to the inbound pipeline.
#[cfg(feature = "wechatmp")]
async fn wechatmp_webhook_handler(
    State(state): State<Arc<GatewayState>>,
    body: Bytes,
) -> impl IntoResponse {
    use crate::channels::wechatmp::{
        compute_signature, decrypt_message, parse_envelope, parse_incoming_xml, parse_plaintext,
    };

    let Some(credentials) = wechatmp_credentials(&state).await else {
        warn!("WeChat MP webhook: channel not configured");
        return StatusCode::FORBIDDEN.into_response();
    };

    let body_str = String::from_utf8_lossy(&body);
    let envelope = match parse_envelope(&body_str) {
        Ok(e) => e,
        Err(e) => {
            warn!("WeChat MP webhook: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // Verify signature over sorted [token, timestamp, nonce, encrypt].
    let expected = compute_signature(
        &credentials.token,
        &envelope.timestamp,
        &envelope.nonce,
        &envelope.encrypt,
    );
    if expected != envelope.msg_signature {
        warn!("WeChat MP webhook: invalid signature");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Decrypt and parse the inner message.
    let key = match credentials.aes_key() {
        Ok(k) => k,
        Err(e) => {
            warn!("WeChat MP webhook: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let plain = match decrypt_message(&key, &envelope.encrypt) {
        Ok(p) => p,
        Err(e) => {
            warn!("WeChat MP webhook: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let (msg_xml, _appid) = match parse_plaintext(&plain) {
        Ok(v) => v,
        Err(e) => {
            warn!("WeChat MP webhook: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let msg_xml = match String::from_utf8(msg_xml) {
        Ok(s) => s,
        Err(_) => {
            warn!("WeChat MP webhook: inner message not utf8");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let incoming = match parse_incoming_xml(&msg_xml) {
        Ok(i) => i,
        Err(e) => {
            warn!("WeChat MP webhook: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // Subscribe event: send a welcome message.
    if incoming.msg_type == "event" && incoming.event == "subscribe" {
        if let Some(channel) = state
            .channels
            .channels
            .read()
            .await
            .get("wechatmp")
            .cloned()
        {
            let _ = channel
                .send(OutgoingMessage::new(
                    ConversationId(incoming.from_user.clone()),
                    "你好！我是你的个人 AI 助手，直接发消息就能找我。".to_string(),
                ))
                .await;
        }
        return StatusCode::OK.into_response();
    }

    if !incoming.is_user_text() {
        // Non-text messages are acknowledged but not routed.
        return StatusCode::OK.into_response();
    }

    // Access control + session management (mirrors the feishu handler).
    if state
        .check_incoming_access(
            "wechatmp",
            &incoming.from_user,
            &incoming.content,
            &crate::channels::MentionState::DirectMessage,
        )
        .await
        .is_err()
    {
        return Json(WebhookResponse {
            success: true,
            message: "OK".to_string(),
        })
        .into_response();
    }

    let platform_key = format!("wechatmp:{}", incoming.from_user);
    let session_id = if incoming.content.trim() == "/new" {
        reset_session(&state.channels.webhook_sessions, &platform_key).await
    } else {
        get_or_create_session(&state.channels.webhook_sessions, &platform_key).await
    };
    {
        let mut sessions = state.channels.session_channels.write().await;
        sessions.insert(session_id.clone(), ("wechatmp".to_string(), incoming.from_user.clone()));
    }

    let msg = IncomingMessage::new(&incoming.from_user, session_id.clone(), incoming.content)
        .with_provenance(InputProvenance::ExternalUser {
            channel: "wechatmp".to_string(),
            is_direct: true,
        });
    if let Err(e) = state.pipelines.inbound_entry.send(msg).await {
        warn!("Failed to enqueue WeChat MP message: {e}");
    }

    // Respond immediately — WeChat requires a prompt 200; replies flow back
    // asynchronously via the customer-service API.
    StatusCode::OK.into_response()
}

/// Handle Slack Events API webhooks
///
/// Supports URL verification and event callbacks (message events).
async fn slack_webhook_handler(
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: Bytes,
) -> impl IntoResponse {
    info!("Received Slack webhook");

    // Verify Slack request signature if signing secret is configured
    let signing_secret = {
        let config = state.config.read().await;
        config
            .channels
            .get("slack")
            .and_then(|c| c.credentials.get("signing_secret"))
            .cloned()
    };

    if let Some(secret) = signing_secret {
        let timestamp = headers
            .get("x-slack-request-timestamp")
            .and_then(|v| v.to_str().ok());
        let signature = headers
            .get("x-slack-signature")
            .and_then(|v| v.to_str().ok());

        if let (Some(ts), Some(sig)) = (timestamp, signature) {
            if !verify_slack_signature(&secret, ts, &body, sig) {
                warn!("Slack webhook: invalid signature");
                return (StatusCode::UNAUTHORIZED, "Invalid signature").into_response();
            }
            debug!("Slack webhook: signature verified");
        } else {
            warn!("Slack webhook: missing signature headers");
            return (StatusCode::UNAUTHORIZED, "Missing signature").into_response();
        }
    }

    // Parse payload
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to parse Slack webhook: {}", e);
            return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response();
        }
    };

    match payload.get("type").and_then(|v| v.as_str()) {
        Some("url_verification") => {
            // URL verification challenge — respond with the challenge string
            if let Some(challenge) = payload.get("challenge").and_then(|v| v.as_str()) {
                info!("Slack URL verification challenge");
                return (StatusCode::OK, challenge.to_string()).into_response();
            }
            (StatusCode::BAD_REQUEST, "Missing challenge").into_response()
        }
        Some("event_callback") => {
            // Event callback — process the event
            if let Some(event) = payload.get("event") {
                handle_slack_event(event, &state).await
            } else {
                (StatusCode::BAD_REQUEST, "Missing event").into_response()
            }
        }
        _ => {
            warn!("Slack webhook: unknown payload type");
            (StatusCode::BAD_REQUEST, "Unknown payload type").into_response()
        }
    }
}

/// Verify Slack request signature (v0 format)
fn verify_slack_signature(secret: &str, timestamp: &str, body: &[u8], signature: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let basestring = format!("v0:{}:", timestamp);
    type HmacSha256 = Hmac<Sha256>;

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };

    mac.update(basestring.as_bytes());
    mac.update(body);

    let result = mac.finalize();
    let code_bytes = result.into_bytes();
    let expected = format!("v0={}", hex::encode(code_bytes));

    // Constant-time comparison
    use subtle::ConstantTimeEq;
    expected.as_bytes().ct_eq(signature.as_bytes()).into()
}

/// Process a Slack event payload
async fn handle_slack_event(
    event: &serde_json::Value,
    state: &Arc<GatewayState>,
) -> axum::response::Response {
    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // Only handle message events
    if event_type != "message" {
        return (StatusCode::OK, "Ignored").into_response();
    }

    // Ignore bot messages
    if event.get("bot_id").is_some() {
        return (StatusCode::OK, "Bot message ignored").into_response();
    }

    // Ignore message subtypes (edits, deletions, etc.)
    if event.get("subtype").is_some() {
        return (StatusCode::OK, "Subtype ignored").into_response();
    }

    let user_id = event.get("user").and_then(|v| v.as_str()).unwrap_or("");
    let channel = event.get("channel").and_then(|v| v.as_str()).unwrap_or("");
    let text = event.get("text").and_then(|v| v.as_str()).unwrap_or("");

    if user_id.is_empty() || text.is_empty() {
        return (StatusCode::OK, "Empty user or text").into_response();
    }

    info!("Slack message from {} in {}: {}", user_id, channel, &text[..text.len().min(50)]);

    // Determine mention state: D-prefixed channels are DMs
    let mention = if channel.starts_with('D') {
        crate::channels::MentionState::DirectMessage
    } else {
        crate::channels::MentionState::NotMentioned
    };

    // Access control check
    if state
        .check_incoming_access("slack", user_id, text, &mention)
        .await
        .is_err()
    {
        return (StatusCode::OK, "Access denied").into_response();
    }

    // Route through unified inbound entry
    let incoming =
        IncomingMessage::new(user_id.to_string(), format!("slack:{}", channel), text.to_string())
            .with_provenance(InputProvenance::ExternalUser {
                channel: "slack".to_string(),
                is_direct: channel.starts_with('D'),
            });

    if let Err(e) = state.pipelines.inbound_entry.send(incoming).await {
        warn!("Failed to enqueue Slack message: {}", e);
    }

    (StatusCode::OK, "OK").into_response()
}

/// Generic webhook handler for custom integrations with HMAC verification
async fn generic_webhook_handler(
    Path(channel): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    body: Bytes,
) -> impl IntoResponse {
    info!("Received generic webhook for channel: {}", channel);

    // Get channel config. Extract the fields we need and release the config
    // guard before the async secret resolution below (avoids holding the lock
    // across `.await`).
    let (channel_enabled, legacy_secret) = {
        let config = state.config.read().await;
        let Some(channel_config) = config.channels.get(&channel) else {
            return (StatusCode::NOT_FOUND, "Channel not configured").into_response();
        };
        (
            channel_config.enabled,
            channel_config.credentials.get("webhook_secret").cloned(),
        )
    };

    if !channel_enabled {
        return (StatusCode::SERVICE_UNAVAILABLE, "Channel disabled").into_response();
    }

    // Resolve webhook secret from the secret store (falling back to legacy
    // plaintext) - required for all generic webhook channels.
    let secret = match state
        .secrets
        .resolve_channel_credential(&channel, "webhook_secret", legacy_secret.as_deref())
        .await
    {
        Ok(Some(s)) if !s.is_empty() => s.into_inner(),
        _ => {
            warn!("{} webhook: webhook_secret is required", channel);
            return (StatusCode::UNAUTHORIZED, "Webhook secret is required for this channel")
                .into_response();
        }
    };

    // Verify HMAC signature
    let signature = headers
        .get("x-signature")
        .or_else(|| headers.get("x-hub-signature-256"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("sha256=").unwrap_or(s));

    if let Some(sig) = signature {
        if !verify_hmac_sha256(&secret, &body, sig) {
            warn!("{} webhook: invalid HMAC signature", channel);
            return (StatusCode::UNAUTHORIZED, "Invalid signature").into_response();
        }
        debug!("{} webhook: HMAC signature verified", channel);
    } else {
        warn!("{} webhook: missing signature", channel);
        return (StatusCode::UNAUTHORIZED, "Missing signature").into_response();
    }

    // Parse generic JSON payload
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => {
            // Try to parse as plain text
            serde_json::json!({
                "text": String::from_utf8_lossy(&body)
            })
        }
    };

    // Extract user ID and message content
    let user_id = payload
        .get("user_id")
        .or_else(|| payload.get("from"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let content = payload
        .get("message")
        .or_else(|| payload.get("text"))
        .or_else(|| payload.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    if !content.is_empty() {
        // Access control check
        if state
            .check_incoming_access(
                &channel,
                &user_id,
                &content,
                &crate::channels::MentionState::DirectMessage,
            )
            .await
            .is_err()
        {
            return Json(WebhookResponse {
                success: true,
                message: "OK".to_string(),
            })
            .into_response();
        }

        // Handle /new command to reset session
        let platform_key = format!("{}:{}", channel, user_id);
        let session_id = if content.trim() == "/new" {
            let new_session = reset_session(&state.channels.webhook_sessions, &platform_key).await;
            info!("🆕 New {} session started for {}: {}", channel, user_id, new_session);
            new_session
        } else {
            // Get or create session UUID
            get_or_create_session(&state.channels.webhook_sessions, &platform_key).await
        };

        // Store session mapping for response routing
        {
            let mut sessions = state.channels.session_channels.write().await;
            sessions.insert(session_id.clone(), (channel.clone(), user_id.clone()));
        }

        // Route through unified inbound entry
        let incoming = IncomingMessage::new(user_id, session_id, content).with_provenance(
            InputProvenance::ExternalUser {
                channel: channel.clone(),
                is_direct: true,
            },
        );
        if let Err(e) = state.pipelines.inbound_entry.send(incoming).await {
            warn!("Failed to enqueue {} webhook message: {}", channel, e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Gateway queue full, please retry".to_string(),
            )
                .into_response();
        }
    }

    Json(WebhookResponse {
        success: true,
        message: "Webhook received".to_string(),
    })
    .into_response()
}

/// Verify HMAC-SHA256 signature
///
/// Used by WhatsApp and generic webhooks
fn verify_hmac_sha256(secret: &str, body: &[u8], expected_sig: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            warn!("Failed to create HMAC from secret");
            return false;
        }
    };

    mac.update(body);
    let result = mac.finalize();
    let computed_sig = hex::encode(result.into_bytes());

    // Constant-time comparison to prevent timing attacks
    use subtle::ConstantTimeEq;
    computed_sig
        .as_bytes()
        .ct_eq(expected_sig.as_bytes())
        .into()
}

/// Verify Feishu/Lark signature
///
/// Feishu uses a custom signature algorithm:
/// SHA256(timestamp + nonce + secret + body)
fn verify_feishu_signature(
    secret: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
    expected_sig: &str,
) -> bool {
    use sha2::{Digest, Sha256};

    // Feishu signature: SHA256(timestamp + nonce + secret + body)
    let body_str = String::from_utf8_lossy(body);
    let sign_string = format!("{}{}{}{}", timestamp, nonce, secret, body_str);

    let mut hasher = Sha256::new();
    hasher.update(sign_string.as_bytes());
    let computed_sig = hex::encode(hasher.finalize());

    // Constant-time comparison to prevent timing attacks
    use subtle::ConstantTimeEq;
    computed_sig
        .as_bytes()
        .ct_eq(expected_sig.as_bytes())
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hmac_sha256_verification() {
        let secret = "test_secret";
        let body = b"test message";

        // Compute expected signature
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let expected_sig = hex::encode(mac.finalize().into_bytes());

        // Verify signature
        assert!(verify_hmac_sha256(secret, body, &expected_sig));

        // Verify wrong signature fails
        assert!(!verify_hmac_sha256(secret, body, "invalid_sig"));
    }

    #[test]
    fn test_feishu_signature_verification() {
        let secret = "test_secret";
        let timestamp = "1234567890";
        let nonce = "abc123";
        let body = b"test message";

        // Compute expected signature
        use sha2::{Digest, Sha256};
        let body_str = String::from_utf8_lossy(body);
        let sign_string = format!("{}{}{}{}", timestamp, nonce, secret, body_str);
        let mut hasher = Sha256::new();
        hasher.update(sign_string.as_bytes());
        let expected_sig = hex::encode(hasher.finalize());

        // Verify signature
        assert!(verify_feishu_signature(secret, timestamp, nonce, body, &expected_sig));

        // Verify wrong signature fails
        assert!(!verify_feishu_signature(secret, timestamp, nonce, body, "invalid_sig"));
    }

    // ── Handler-level integration tests ──────────────────────────────────────

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn make_webhook_state() -> GatewayState {
        let mut config = crate::gateway::GatewayConfig::default();

        let mut whatsapp =
            crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Whatsapp);
        whatsapp
            .credentials
            .insert("verify_token".to_string(), "secret123".to_string());
        config.channels.insert("whatsapp".to_string(), whatsapp);

        let mut telegram =
            crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Telegram);
        telegram
            .credentials
            .insert("webhook_token".to_string(), "mytoken".to_string());
        config.channels.insert("telegram".to_string(), telegram);

        let mut feishu = crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Feishu);
        feishu
            .credentials
            .insert("webhook_secret".to_string(), "feishu_secret".to_string());
        config.channels.insert("feishu".to_string(), feishu);

        let slack = crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Slack);
        config.channels.insert("slack".to_string(), slack);

        let mut disabled =
            crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Whatsapp);
        disabled.enabled = false;
        config.channels.insert("disabled".to_string(), disabled);

        crate::gateway::state_tests::make_test_state(config).await
    }

    #[tokio::test]
    async fn whatsapp_verify_success() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let req = Request::builder()
            .uri(
                "/webhooks/whatsapp/verify?hub_mode=subscribe&hub_verify_token=secret123&\
                 hub_challenge=123456",
            )
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body, "123456");
    }

    #[tokio::test]
    async fn whatsapp_verify_wrong_token() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let req = Request::builder()
            .uri(
                "/webhooks/whatsapp/verify?hub_mode=subscribe&hub_verify_token=wrong&\
                 hub_challenge=123",
            )
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn whatsapp_verify_missing_params() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let req = Request::builder()
            .uri("/webhooks/whatsapp/verify")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn telegram_webhook_valid_token() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({
            "update_id": 1,
            "message": {
                "message_id": 1,
                "from": { "id": 123, "first_name": "Test" },
                "chat": { "id": 456, "type": "private" },
                "date": 1700000000,
                "text": "Hello"
            }
        });

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/telegram/mytoken")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn telegram_webhook_invalid_token() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({"update_id": 1});

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/telegram/badtoken")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn feishu_webhook_challenge() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({"challenge": "abc123"});

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/feishu")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["challenge"], "abc123");
    }

    #[tokio::test]
    async fn generic_webhook_channel_not_found() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({"user_id": "u1", "message": "hi"});

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/unknown")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn generic_webhook_channel_disabled() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({"user_id": "u1", "message": "hi"});

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/disabled")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── Slack webhook tests ────────────────────────────────────────────────

    fn make_slack_signature(secret: &str, timestamp: &str, body: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        let basestring = format!("v0:{}:{}", timestamp, body);
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(basestring.as_bytes());
        format!("v0={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[tokio::test]
    async fn slack_url_verification() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({
            "type": "url_verification",
            "challenge": "slack_challenge_123"
        });
        let body_str = payload.to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/slack")
            .header("content-type", "application/json")
            .body(Body::from(body_str))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body, "slack_challenge_123");
    }

    #[tokio::test]
    async fn slack_event_callback_message() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "user": "U123456",
                "text": "Hello bot",
                "channel": "CABCDEF"
            }
        });
        let body_str = payload.to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/slack")
            .header("content-type", "application/json")
            .body(Body::from(body_str))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn slack_event_callback_dm() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "user": "U123456",
                "text": "Hello in DM",
                "channel": "DABCDEF"
            }
        });
        let body_str = payload.to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/slack")
            .header("content-type", "application/json")
            .body(Body::from(body_str))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn slack_ignores_bot_messages() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "user": "U123456",
                "bot_id": "B123",
                "text": "I am a bot",
                "channel": "CABCDEF"
            }
        });
        let body_str = payload.to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/slack")
            .header("content-type", "application/json")
            .body(Body::from(body_str))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn slack_ignores_message_subtypes() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "user": "U123456",
                "subtype": "message_changed",
                "text": "edited",
                "channel": "CABCDEF"
            }
        });
        let body_str = payload.to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/slack")
            .header("content-type", "application/json")
            .body(Body::from(body_str))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn make_slack_webhook_state_with_secret() -> GatewayState {
        let mut config = crate::gateway::GatewayConfig::default();
        let mut slack = crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Slack);
        slack
            .credentials
            .insert("signing_secret".to_string(), "slack_secret".to_string());
        config.channels.insert("slack".to_string(), slack);
        crate::gateway::state_tests::make_test_state(config).await
    }

    #[tokio::test]
    async fn slack_signature_verification_success() {
        let state = std::sync::Arc::new(make_slack_webhook_state_with_secret().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "user": "U123456",
                "text": "Hello",
                "channel": "CABCDEF"
            }
        });
        let body_str = payload.to_string();
        let timestamp = "1234567890";
        let signature = make_slack_signature("slack_secret", timestamp, &body_str);

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/slack")
            .header("content-type", "application/json")
            .header("x-slack-request-timestamp", timestamp)
            .header("x-slack-signature", signature)
            .body(Body::from(body_str))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn slack_signature_verification_failure() {
        let state = std::sync::Arc::new(make_slack_webhook_state_with_secret().await);
        let app = create_webhook_router(state);

        let payload = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "user": "U123456",
                "text": "Hello",
                "channel": "CABCDEF"
            }
        });
        let body_str = payload.to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/slack")
            .header("content-type", "application/json")
            .header("x-slack-request-timestamp", "1234567890")
            .header("x-slack-signature", "v0=bad_signature")
            .body(Body::from(body_str))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
