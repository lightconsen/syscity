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
use crate::channels::{
    ConversationId, IncomingMessage, InputProvenance, MentionState, OutgoingMessage,
};

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

/// How long a repeated delivery counts as a replay.
///
/// Comfortably longer than any of these platforms' retry schedules and longer
/// than the 300 s freshness window Slack and Feishu enforce, so a captured
/// request cannot be replayed at the edge of one check and inside another.
const REPLAY_WINDOW: std::time::Duration = std::time::Duration::from_secs(900);

/// How many deliveries are remembered before the oldest are forgotten.
///
/// Bounds the memory a public endpoint can be made to hold; an endpoint seeing
/// this many distinct deliveries inside `REPLAY_WINDOW` is being retried
/// rather than used.
const REPLAY_CAPACITY: usize = 4096;

/// Remembers recently seen webhook deliveries so a repeat is not acted on.
///
/// A signature proves a delivery came from the platform; it says nothing about
/// whether the delivery is *new*. Slack and Feishu carry a timestamp checked
/// against a 300 s window, but WhatsApp's HMAC covers the body only — that
/// scheme has no timestamp — so a captured delivery can be replayed
/// indefinitely, and a Slack or Feishu one can be replayed inside its window.
/// This is the nonce store that closes both.
///
/// The key is the SHA-256 of the raw body. Every one of these platforms embeds
/// a unique id in each delivery (Slack `event_id`, Telegram `update_id`,
/// Feishu `header.event_id`, WhatsApp message/status id, WeChat `MsgId`), so
/// two byte-identical bodies inside the window *are* one delivery — which also
/// means the common case, a platform retry, needs no per-platform payload
/// parsing to catch.
#[derive(Debug, Default)]
pub(crate) struct ReplayGuard {
    seen: std::sync::Mutex<HashMap<String, std::time::Instant>>,
}

impl ReplayGuard {
    /// Record a delivery. `false` means it was already seen inside the window.
    ///
    /// Either way the caller answers 200 — the platform should stop retrying —
    /// but a delivery that is not new must not be processed again.
    pub(crate) fn first_sighting(&self, body: &[u8]) -> bool {
        let key = delivery_digest(body);
        let now = std::time::Instant::now();
        // A poisoned lock would only mean another request panicked mid-insert;
        // the map is still consistent, and refusing every webhook afterwards
        // would be a worse failure than carrying on.
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, at| now.duration_since(*at) < REPLAY_WINDOW);
        if seen.contains_key(&key) {
            return false;
        }
        if seen.len() >= REPLAY_CAPACITY {
            // Forget the oldest rather than refuse new deliveries: receiving is
            // the job, and forgetting only widens the window for an attacker
            // who can already forge signatures.
            if let Some(oldest) = seen
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(k, _)| k.clone())
            {
                seen.remove(&oldest);
            }
        }
        seen.insert(key, now);
        true
    }
}

/// SHA-256 of the raw body, hex-encoded: the replay key.
fn delivery_digest(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(body))
}

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

/// What became of a message handed to [`dispatch_channel_message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InboundDisposition {
    /// Accepted and queued for the agent.
    Queued,
    /// Access control refused the sender. The platform is told nothing, so a
    /// probe cannot tell a blocked sender from an accepted one.
    Refused,
    /// The gateway's inbound queue is full; the platform should retry.
    QueueFull,
}

/// The tail that every direct-message channel shares — webhook or not.
///
/// Once a channel has authenticated a delivery and pulled a `(user_id, text)`
/// pair out of its platform's payload, what remains is the same everywhere: ask
/// access control, derive the platform session key, honour `/new`, remember
/// which channel owns the session so the reply can be routed back, and hand the
/// message to the unified inbound entry.
///
/// Feishu, WeChat MP and the generic webhook had each grown their own copy of
/// this. Sharing it is what keeps a new channel down to "authenticate, extract,
/// call this" — and it is the part easiest to get subtly wrong, because a
/// handler that forgets the `session_channels` insert still compiles and still
/// accepts messages; the agent just answers into the void.
///
/// It is `pub(crate)` because a channel that holds a connection rather than a
/// route needs exactly the same tail: `init_wecom_channel`'s bridge calls this
/// for every message the socket delivers, so a persistent-connection channel
/// does not grow a second copy of the access check and the session mapping.
///
/// Slack and WhatsApp are deliberately not callers: Slack keys sessions by
/// channel rather than user and has no `/new`, and WhatsApp resets *before*
/// the access check because it sends the user a confirmation.
pub(crate) async fn dispatch_channel_message(
    state: &Arc<GatewayState>,
    channel: &str,
    user_id: &str,
    text: &str,
    mention: crate::channels::MentionState,
) -> InboundDisposition {
    dispatch_channel_message_to(state, channel, user_id, user_id, text, mention).await
}

/// As [`dispatch_channel_message`], but the reply is addressed somewhere other
/// than the sender.
///
/// Google Chat is why this exists: a message arrives from a *user*
/// (`users/123`) inside a *space* (`spaces/S`), and a reply goes to the space.
/// Keying the session on the sender is still right; addressing the reply to
/// them would name something the Chat API cannot send to.
///
/// `mention` carries more than whether the bot was named. A group message is
/// exactly one that is not a [`MentionState::DirectMessage`], and the inbound
/// stage reads the provenance built from that to decide whether an unmentioned
/// group message is suppressed — so a channel that reports a group as direct
/// has its bot answer every message in it.
pub(crate) async fn dispatch_channel_message_to(
    state: &Arc<GatewayState>,
    channel: &str,
    user_id: &str,
    conversation_id: &str,
    text: &str,
    mention: crate::channels::MentionState,
) -> InboundDisposition {
    if state
        .check_incoming_access(channel, user_id, text, &mention)
        .await
        .is_err()
    {
        return InboundDisposition::Refused;
    }

    let platform_key = format!("{}:{}", channel, user_id);
    let session_id = if text.trim() == "/new" {
        let new_session = reset_session(&state.channels.webhook_sessions, &platform_key).await;
        info!("🆕 New {} session started for {}: {}", channel, user_id, new_session);
        new_session
    } else {
        get_or_create_session(&state.channels.webhook_sessions, &platform_key).await
    };

    // Remember which channel owns this session, so the agent's reply can be
    // routed back out through it — addressed to `conversation_id`, which is the
    // sender for every channel whose reply goes back to them.
    {
        let mut sessions = state.channels.session_channels.write().await;
        sessions.insert(session_id.clone(), (channel.to_string(), conversation_id.to_string()));
    }

    let incoming = IncomingMessage::new(user_id, session_id, text)
        .with_provenance(InputProvenance::ExternalUser {
            channel: channel.to_string(),
            is_direct: matches!(mention, crate::channels::MentionState::DirectMessage),
        })
        .with_mention(mention);

    match state.pipelines.inbound_entry.send(incoming).await {
        Ok(()) => InboundDisposition::Queued,
        Err(e) => {
            warn!("Failed to enqueue {} webhook message: {}", channel, e);
            InboundDisposition::QueueFull
        }
    }
}

/// Resolve one channel credential from config, falling back to the secret store.
///
/// The secret store (namespace `channel`) is authoritative; the plaintext
/// `credentials` map is the legacy fallback for pre-migration configs. Returns
/// `None` when the key is unset, which every caller reads as "refuse".
#[cfg(any(feature = "googlechat", feature = "teams"))]
async fn channel_credential(state: &Arc<GatewayState>, channel: &str, key: &str) -> Option<String> {
    let legacy = {
        let config = state.config.read().await;
        config
            .channels
            .get(channel)
            .and_then(|c| c.credentials.get(key))
            .cloned()
    };
    state
        .secrets
        .resolve_channel_credential(channel, key, legacy.as_deref())
        .await
        .ok()
        .flatten()
        .map(|v| v.into_inner())
}

/// The bearer token on a delivery, or `None` when it carries none.
///
/// Shared by the two channels whose webhook is authenticated by a JWT rather
/// than a signature. A missing or empty header is `None` rather than an empty
/// string, so a caller cannot accidentally hand `""` to a verifier.
#[cfg(any(feature = "googlechat", feature = "teams"))]
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
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

    // LINE Messaging API webhooks (feature-gated channel)
    #[cfg(feature = "line")]
    let router = router.route("/webhooks/line", post(line_webhook_handler));

    // Twilio SMS webhooks (feature-gated channel)
    #[cfg(feature = "twilio")]
    let router = router.route("/webhooks/twilio", post(twilio_webhook_handler));

    // Google Chat webhooks (feature-gated channel)
    #[cfg(feature = "googlechat")]
    let router = router.route("/webhooks/googlechat", post(googlechat_webhook_handler));

    // Microsoft Teams webhooks (feature-gated channel)
    #[cfg(feature = "teams")]
    let router = router.route("/webhooks/teams", post(teams_webhook_handler));

    // One guard for the process: a delivery is a replay wherever in the router
    // it arrives.
    router
        .layer(axum::Extension(Arc::new(ReplayGuard::default())))
        .with_state(state)
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
            // Fail closed: without a configured verify_token there is nothing
            // to check the request against, so the challenge must be refused —
            // not accepted because `expected_token` happened to be None.
            match expected_token.as_deref() {
                Some(expected) if expected == token => {
                    // Return the challenge
                    if let Some(challenge) = query.hub_challenge {
                        info!("WhatsApp webhook verified successfully");
                        return (StatusCode::OK, challenge).into_response();
                    }
                }
                Some(_) => {
                    warn!("WhatsApp webhook verification failed: invalid token");
                }
                None => {
                    warn!("WhatsApp webhook verification failed: no verify_token configured — \
                           the webhook cannot be verified and the subscription challenge is refused");
                }
            }
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
    replay: axum::Extension<Arc<ReplayGuard>>,
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

    // Verify the HMAC signature. The secret is *required*: a WhatsApp webhook
    // without one would accept unauthenticated POSTs straight into the agent
    // pipeline. A configured secret with a missing or wrong signature is equally
    // refused.
    let secret = match hmac_secret {
        Some(secret) => secret,
        None => {
            warn!("WhatsApp webhook: app_secret is not configured — refusing the request");
            return (StatusCode::UNAUTHORIZED, "Webhook secret is required").into_response();
        }
    };
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

    // A signature proves where a delivery came from, not that it is new.
    if !replay.first_sighting(&body) {
        info!("WhatsApp webhook: duplicate delivery ignored");
        return Json(WebhookResponse {
            success: true,
            message: "duplicate ignored".to_string(),
        })
        .into_response();
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
    replay: axum::Extension<Arc<ReplayGuard>>,
    body: Bytes,
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

    // A token in the URL proves where a delivery came from, not that it is new.
    if !replay.first_sighting(&body) {
        info!("Telegram webhook: duplicate delivery ignored");
        return Json(WebhookResponse {
            success: true,
            message: "duplicate ignored".to_string(),
        })
        .into_response();
    }

    // The body is parsed here rather than by the `Json` extractor so the raw
    // delivery is available to the replay guard above.
    let update: TelegramUpdate = match serde_json::from_slice(&body) {
        Ok(update) => update,
        Err(e) => {
            error!("Failed to parse Telegram webhook: {}", e);
            return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response();
        }
    };

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
    replay: axum::Extension<Arc<ReplayGuard>>,
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

    // Verify the signature. Every piece is required: a missing secret or a
    // missing header means the request cannot be authenticated, which must
    // refuse it — not let it through by falling out of the `if let`.
    let (secret, sig, ts, nonce) = match (secret, signature, timestamp, nonce) {
        (Some(secret), Some(sig), Some(ts), Some(nonce)) => (secret, sig, ts, nonce),
        _ => {
            warn!("Feishu webhook: secret or signature headers missing — refusing the request");
            return (
                StatusCode::UNAUTHORIZED,
                "Signature verification requires a configured secret and full signature headers",
            )
                .into_response();
        }
    };
    if !verify_feishu_signature(&secret, ts, nonce, &body, sig) {
        warn!("Feishu webhook: invalid signature");
        return (StatusCode::UNAUTHORIZED, "Invalid signature").into_response();
    }
    debug!("Feishu webhook: signature verified");

    // A signature proves where a delivery came from, not that it is new.
    if !replay.first_sighting(&body) {
        info!("Feishu webhook: duplicate delivery ignored");
        return Json(WebhookResponse {
            success: true,
            message: "duplicate ignored".to_string(),
        })
        .into_response();
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

                // Verify, extract, hand off — the shared direct-message tail.
                if dispatch_channel_message(
                    &state,
                    "feishu",
                    user_id,
                    text,
                    MentionState::DirectMessage,
                )
                .await
                    == InboundDisposition::Refused
                {
                    return Json(WebhookResponse {
                        success: true,
                        message: "OK".to_string(),
                    })
                    .into_response();
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

/// Handle LINE Messaging API webhooks.
///
/// LINE signs the *raw* body with the channel secret and carries the result
/// base64-encoded in `X-Line-Signature` — which is why the body is verified
/// before it is parsed, and why the parse takes the bytes rather than a
/// re-serialised value.
///
/// One delivery is a batch of events, and most events are not messages: LINE
/// reports follows, joins, leaves and postbacks through the same endpoint.
/// Those are acknowledged and skipped.
#[cfg(feature = "line")]
async fn line_webhook_handler(
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    replay: axum::Extension<Arc<ReplayGuard>>,
    body: Bytes,
) -> impl IntoResponse {
    info!("Received LINE webhook");

    let signature = headers
        .get(crate::channels::line::LINE_SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok());

    // Resolve the channel secret from the secret store, falling back to the
    // legacy plaintext credentials map. The config guard is released before the
    // async resolution to avoid holding the lock across `.await`.
    let secret = {
        let legacy = {
            let config = state.config.read().await;
            config
                .channels
                .get("line")
                .and_then(|c| c.credentials.get("channel_secret"))
                .cloned()
        };
        state
            .secrets
            .resolve_channel_credential("line", "channel_secret", legacy.as_deref())
            .await
            .ok()
            .flatten()
            .map(|v| v.into_inner())
    };

    // Every piece is required: a missing secret or header means the request
    // cannot be authenticated, which must refuse it rather than let it through
    // by falling out of the `if let`.
    let (secret, signature) = match (secret, signature) {
        (Some(secret), Some(signature)) => (secret, signature),
        _ => {
            warn!("LINE webhook: channel secret or signature header missing — refusing");
            return (
                StatusCode::UNAUTHORIZED,
                "Signature verification requires a configured secret and the signature header",
            )
                .into_response();
        }
    };

    if !crate::channels::line::verify_signature(&secret, &body, signature) {
        warn!("LINE webhook: invalid signature");
        return (StatusCode::UNAUTHORIZED, "Invalid signature").into_response();
    }
    debug!("LINE webhook: signature verified");

    // A signature proves where a delivery came from, not that it is new.
    if !replay.first_sighting(&body) {
        info!("LINE webhook: duplicate delivery ignored");
        return Json(WebhookResponse {
            success: true,
            message: "duplicate ignored".to_string(),
        })
        .into_response();
    }

    let events = match crate::channels::line::parse_events(&body) {
        Ok(events) => events,
        Err(e) => {
            error!("Failed to parse LINE webhook: {}", e);
            return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response();
        }
    };

    for event in events {
        // Verify, extract, hand off — the shared direct-message tail.
        //
        // A full queue is logged by the helper and deliberately *not* turned
        // into a 503, unlike the generic webhook's handler. LINE retries a
        // non-2xx with the same body, and the replay guard above would then
        // drop that retry as a duplicate — so the retry cannot recover the
        // message and a non-2xx would only claim it might.
        dispatch_channel_message(
            &state,
            "line",
            &event.user_id,
            &event.text,
            MentionState::DirectMessage,
        )
        .await;
    }

    Json(WebhookResponse {
        success: true,
        message: "OK".to_string(),
    })
    .into_response()
}

/// The empty TwiML reply that tells Twilio there is nothing to send back.
///
/// A 2xx with any body would also do, but an explicit empty `<Response/>` is
/// what Twilio's own examples use and removes the ambiguity about whether the
/// gateway meant to reply.
#[cfg(feature = "twilio")]
fn twiml_ok() -> axum::response::Response {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/xml"),
        )],
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response></Response>",
    )
        .into_response()
}

/// Handle Twilio SMS webhooks.
///
/// Twilio posts `application/x-www-form-urlencoded`, not JSON, and signs **the
/// URL it dialled** together with the sorted parameters. That is why
/// verification needs the configured public URL and cannot read it off the
/// request: behind a tunnel the gateway sees `127.0.0.1:<port>` while Twilio
/// signed `https://…`. A delivery whose signature cannot be checked is refused
/// rather than let through — there is deliberately no "skip validation" switch,
/// the way there is none for the other channels.
///
/// The same URL also receives status callbacks for messages Twilio has already
/// handled. Those carry no `Body`, so they are acknowledged without becoming an
/// empty turn.
#[cfg(feature = "twilio")]
async fn twilio_webhook_handler(
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    replay: axum::Extension<Arc<ReplayGuard>>,
    body: Bytes,
) -> impl IntoResponse {
    info!("Received Twilio webhook");

    let signature = headers
        .get(crate::channels::twilio::TWILIO_SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok());

    // Resolve the auth token and the configured public URL from the secret
    // store, falling back to the legacy plaintext credentials map. The config
    // guard is released before the async resolution to avoid holding the lock
    // across `.await`.
    let (auth_token, webhook_url) = {
        let legacy = {
            let config = state.config.read().await;
            config.channels.get("twilio").map(|c| {
                (
                    c.credentials.get("auth_token").cloned(),
                    c.credentials.get("webhook_url").cloned(),
                )
            })
        };
        let (legacy_token, legacy_url) = legacy.unwrap_or((None, None));

        let token = state
            .secrets
            .resolve_channel_credential("twilio", "auth_token", legacy_token.as_deref())
            .await
            .ok()
            .flatten()
            .map(|v| v.into_inner());
        let url = state
            .secrets
            .resolve_channel_credential("twilio", "webhook_url", legacy_url.as_deref())
            .await
            .ok()
            .flatten()
            .map(|v| v.into_inner());

        (token, url)
    };

    // Every piece is required, and the URL is not optional the way a display
    // name would be: without it the signature covers a string this process
    // cannot construct, so nothing can be verified.
    let (auth_token, webhook_url, signature) = match (auth_token, webhook_url, signature) {
        (Some(token), Some(url), Some(signature)) => (token, url, signature),
        _ => {
            warn!(
                "Twilio webhook: auth token, configured webhook URL, or signature header \
                 missing — refusing the delivery"
            );
            return (
                StatusCode::UNAUTHORIZED,
                "Signature verification requires the auth token, the configured webhook URL, \
                 and the signature header",
            )
                .into_response();
        }
    };

    let params = crate::channels::twilio::parse_form(&body);

    if !crate::channels::twilio::verify_signature(&auth_token, &webhook_url, &params, signature) {
        warn!("Twilio webhook: invalid signature");
        return (StatusCode::UNAUTHORIZED, "Invalid signature").into_response();
    }
    debug!("Twilio webhook: signature verified");

    // A signature proves where a delivery came from, not that it is new.
    if !replay.first_sighting(&body) {
        info!("Twilio webhook: duplicate delivery ignored");
        return twiml_ok();
    }

    match crate::channels::twilio::parse_incoming(&params) {
        Some(incoming) => {
            // Verify, extract, hand off — the shared direct-message tail.
            dispatch_channel_message(
                &state,
                "twilio",
                &incoming.from,
                &incoming.body,
                MentionState::DirectMessage,
            )
            .await;
        }
        None => {
            debug!("Twilio webhook: not an inbound message (a status callback?) — acknowledged");
        }
    }

    twiml_ok()
}

/// Handle Google Chat webhooks.
///
/// Chat authenticates with a bearer token rather than a signature, which makes
/// this handler different in two ways from its neighbours. The token is checked
/// against the issuer's published keys — and then its **identity** is checked
/// too, because Google signs these for its own service accounts: a valid
/// signature proves Google issued the token, not that it was issued for *this*
/// app.
///
/// A delivery may also be a bot's own reply arriving back around, or one of the
/// non-message events Chat posts to the same URL. Both are acknowledged without
/// becoming turns.
#[cfg(feature = "googlechat")]
async fn googlechat_webhook_handler(
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    replay: axum::Extension<Arc<ReplayGuard>>,
    body: Bytes,
) -> impl IntoResponse {
    info!("Received Google Chat webhook");

    // Built once at init and kept, because the verifier caches the issuer's key
    // set — constructing one here would refetch it on every delivery.
    let verifier = {
        let verifiers = state.channels.webhook_verifiers.read().await;
        verifiers.get("googlechat").cloned()
    };
    let Some(verifier) = verifier else {
        warn!("Google Chat webhook: the channel is not configured to verify deliveries — refusing");
        return (StatusCode::UNAUTHORIZED, "Channel is not configured to verify deliveries")
            .into_response();
    };

    let Some(token) = bearer_token(&headers) else {
        warn!("Google Chat webhook: no bearer token — refusing");
        return (StatusCode::UNAUTHORIZED, "A bearer token is required").into_response();
    };

    let claims = match verifier.verify(token).await {
        Ok(claims) => claims,
        Err(e) => {
            warn!("Google Chat webhook: {}", e);
            return (StatusCode::UNAUTHORIZED, "Invalid bearer token").into_response();
        }
    };

    // Which identity the token was issued to, resolved the same way the other
    // handlers resolve their credentials.
    let allowed: Vec<String> = channel_credential(&state, "googlechat", "allowed_service_accounts")
        .await
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if !crate::channels::googlechat::identity_is_allowed(&allowed, email) {
        warn!("Google Chat webhook: bearer token was not issued to an allowed identity");
        return (StatusCode::UNAUTHORIZED, "Token identity is not allowed").into_response();
    }
    debug!("Google Chat webhook: bearer token verified");

    // A signature proves where a delivery came from, not that it is new.
    if !replay.first_sighting(&body) {
        info!("Google Chat webhook: duplicate delivery ignored");
        return Json(WebhookResponse {
            success: true,
            message: "duplicate ignored".to_string(),
        })
        .into_response();
    }

    match crate::channels::googlechat::parse_event(&body) {
        Ok(Some(incoming)) => {
            // The session is keyed on the sender, the reply addressed to the
            // space — which is why this is the `_to` variant.
            dispatch_channel_message_to(
                &state,
                "googlechat",
                &incoming.user_id,
                &incoming.space,
                &incoming.text,
                MentionState::DirectMessage,
            )
            .await;
        }
        Ok(None) => {
            debug!("Google Chat webhook: not a user message — acknowledged");
        }
        Err(e) => {
            warn!("Google Chat webhook: {}", e);
            return (StatusCode::BAD_REQUEST, "Invalid payload").into_response();
        }
    }

    Json(WebhookResponse {
        success: true,
        message: "OK".to_string(),
    })
    .into_response()
}

/// Handle Microsoft Teams webhooks.
///
/// The delivery is a Bot Framework *activity* carrying a bearer token. The token
/// is verified against the issuer's published keys, and then its `serviceurl`
/// claim is compared with the activity's `serviceUrl` field — Microsoft signs one token per bot,
/// and that claim is what binds a token that verifies to the conversation
/// surface the activity actually came from. This is the check that stands in for
/// Google Chat's identity allowlist: there, the token names an identity that has
/// to be listed; here, it names the service that has to agree.
///
/// An activity may also be a conversation update, a reaction, or the bot's own
/// message arriving back around. All of those are acknowledged without becoming
/// turns.
#[cfg(feature = "teams")]
async fn teams_webhook_handler(
    headers: HeaderMap,
    State(state): State<Arc<GatewayState>>,
    replay: axum::Extension<Arc<ReplayGuard>>,
    body: Bytes,
) -> impl IntoResponse {
    info!("Received Microsoft Teams webhook");

    // Built once at init and kept, because the verifier caches the issuer's key
    // set — constructing one here would refetch it on every delivery.
    let verifier = {
        let verifiers = state.channels.webhook_verifiers.read().await;
        verifiers.get("teams").cloned()
    };
    let Some(verifier) = verifier else {
        warn!("Teams webhook: the channel is not configured to verify deliveries — refusing");
        return (StatusCode::UNAUTHORIZED, "Channel is not configured to verify deliveries")
            .into_response();
    };

    let Some(token) = bearer_token(&headers) else {
        warn!("Teams webhook: no bearer token — refusing");
        return (StatusCode::UNAUTHORIZED, "A bearer token is required").into_response();
    };

    let claims = match verifier.verify(token).await {
        Ok(claims) => claims,
        Err(e) => {
            warn!("Teams webhook: {}", e);
            return (StatusCode::UNAUTHORIZED, "Invalid bearer token").into_response();
        }
    };
    debug!("Teams webhook: bearer token verified");

    // A signature proves where a delivery came from, not that it is new.
    if !replay.first_sighting(&body) {
        info!("Teams webhook: duplicate delivery ignored");
        return Json(WebhookResponse {
            success: true,
            message: "duplicate ignored".to_string(),
        })
        .into_response();
    }

    // The app id is what the token was verified against, so it is already
    // configured; it is read again because the parser needs it to recognise the
    // bot's own activities.
    let app_id = channel_credential(&state, "teams", "app_id")
        .await
        .unwrap_or_default();

    let incoming = match crate::channels::teams::parse_activity(&body, &app_id) {
        Ok(Some(incoming)) => incoming,
        Ok(None) => {
            debug!("Teams webhook: not a user message — acknowledged");
            return Json(WebhookResponse {
                success: true,
                message: "OK".to_string(),
            })
            .into_response();
        }
        Err(e) => {
            warn!("Teams webhook: {}", e);
            return (StatusCode::BAD_REQUEST, "Invalid payload").into_response();
        }
    };

    // The claim is spelled `serviceurl` — all one word, all lower case — which
    // is the constant both Bot Framework SDKs look up, and the name the claim
    // actually carries in the token. `serviceUrl` is the activity *field*'s
    // spelling and finds nothing here, which would refuse every delivery.
    //
    // Both sides go through the same canonicalization, so a tenant that spells
    // its service URL with and without the trailing slash still matches. A token
    // minted for another bot's connector verifies against the same keys and
    // names the same audience, so this comparison is what says it belongs to
    // *this* conversation.
    let claimed = claims
        .get("serviceurl")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if crate::channels::teams::validate_service_url(claimed).as_deref()
        != Some(incoming.address.service_url.as_str())
    {
        warn!(
            "Teams webhook: the token's serviceurl claim does not match the activity's — refusing"
        );
        return (StatusCode::UNAUTHORIZED, "Token is for another service").into_response();
    }

    // The session is keyed on the sender, the reply addressed to the
    // conversation they wrote in — which is why this is the `_to` variant.
    dispatch_channel_message_to(
        &state,
        "teams",
        &incoming.user_id,
        &incoming.address.encode(),
        &incoming.text,
        MentionState::DirectMessage,
    )
    .await;

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
    replay: axum::Extension<Arc<ReplayGuard>>,
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

    // A signature proves where a delivery came from, not that it is new.
    if !replay.first_sighting(&body) {
        info!("WeChat MP webhook: duplicate delivery ignored");
        return StatusCode::OK.into_response();
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

    // Verify, extract, hand off — the shared direct-message tail.
    if dispatch_channel_message(
        &state,
        "wechatmp",
        &incoming.from_user,
        &incoming.content,
        MentionState::DirectMessage,
    )
    .await
        == InboundDisposition::Refused
    {
        return Json(WebhookResponse {
            success: true,
            message: "OK".to_string(),
        })
        .into_response();
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
    replay: axum::Extension<Arc<ReplayGuard>>,
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

    // The signing secret is required: a Slack webhook without one would accept
    // unauthenticated POSTs. A configured secret with missing or wrong
    // signature headers is equally refused.
    let secret = match signing_secret {
        Some(secret) => secret,
        None => {
            warn!("Slack webhook: signing_secret is not configured — refusing the request");
            return (StatusCode::UNAUTHORIZED, "Webhook signing secret is required")
                .into_response();
        }
    };
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

    // A signature proves where a delivery came from, not that it is new: Slack
    // retries, and a captured request can be replayed inside the freshness
    // window.
    if !replay.first_sighting(&body) {
        info!("Slack webhook: duplicate delivery ignored");
        return (StatusCode::OK, "duplicate ignored").into_response();
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
    if !timestamp_is_fresh(timestamp) {
        return false;
    }
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
    replay: axum::Extension<Arc<ReplayGuard>>,
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

    // A signature proves where a delivery came from, not that it is new.
    if !replay.first_sighting(&body) {
        info!("{} webhook: duplicate delivery ignored", channel);
        return (StatusCode::OK, "duplicate ignored").into_response();
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
        // Verify, extract, hand off — the shared direct-message tail.
        match dispatch_channel_message(
            &state,
            &channel,
            &user_id,
            &content,
            MentionState::DirectMessage,
        )
        .await
        {
            InboundDisposition::Refused => {
                return Json(WebhookResponse {
                    success: true,
                    message: "OK".to_string(),
                })
                .into_response();
            }
            InboundDisposition::QueueFull => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Gateway queue full, please retry".to_string(),
                )
                    .into_response();
            }
            InboundDisposition::Queued => {}
        }
    }

    Json(WebhookResponse {
        success: true,
        message: "Webhook received".to_string(),
    })
    .into_response()
}

/// How old a signed webhook request may be.
///
/// The signature covers the timestamp, so a captured request stays valid
/// forever unless its age is checked — this is the only replay defense on
/// routes that are unauthenticated by design. Five minutes is what Slack
/// documents (their own retries land within seconds, so this tolerates clock
/// skew without leaving a useful window).
const SIGNATURE_MAX_AGE_SECS: u64 = 300;

/// Whether a signed-request timestamp is recent enough to act on.
///
/// Rejects a timestamp that is unparseable, older than
/// [`SIGNATURE_MAX_AGE_SECS`], or far in the future (which a skewed or
/// hand-crafted header can claim).
fn timestamp_is_fresh(timestamp: &str) -> bool {
    let Ok(secs) = timestamp.trim().parse::<i64>() else {
        return false;
    };
    let now = chrono::Utc::now().timestamp();
    let age = now.saturating_sub(secs);
    // The future bound is looser than the past one: a few seconds of skew in
    // the other direction is normal.
    age <= SIGNATURE_MAX_AGE_SECS as i64 && age >= -(SIGNATURE_MAX_AGE_SECS as i64)
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
    if !timestamp_is_fresh(timestamp) {
        return false;
    }
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
        let timestamp = &now_timestamp();
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
        // The webhook verification secret the handlers now require.
        whatsapp
            .credentials
            .insert("app_secret".to_string(), "whatsapp_secret".to_string());
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

        let mut slack = crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Slack);
        slack
            .credentials
            .insert("signing_secret".to_string(), "slack_secret".to_string());
        config.channels.insert("slack".to_string(), slack);

        let mut disabled =
            crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Whatsapp);
        disabled.enabled = false;
        config.channels.insert("disabled".to_string(), disabled);

        crate::gateway::state_tests::make_test_state(config).await
    }

    /// A state whose webhook channels are enabled but carry no verification
    /// secrets — the configuration every fail-closed test is about.
    async fn make_secretless_webhook_state() -> GatewayState {
        let mut config = crate::gateway::GatewayConfig::default();
        for (name, kind) in [
            ("whatsapp", crate::channels::ChannelType::Whatsapp),
            ("slack", crate::channels::ChannelType::Slack),
            ("feishu", crate::channels::ChannelType::Feishu),
        ] {
            let channel = crate::gateway::ChannelConfig::new(kind);
            config.channels.insert(name.to_string(), channel);
        }
        crate::gateway::state_tests::make_test_state(config).await
    }

    fn signed_slack_request(body: &str) -> Request<Body> {
        let timestamp = now_timestamp();
        let signature = make_slack_signature("slack_secret", &timestamp, body);
        Request::builder()
            .method("POST")
            .uri("/webhooks/slack")
            .header("content-type", "application/json")
            .header("x-slack-request-timestamp", &timestamp)
            .header("x-slack-signature", signature)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn signed_feishu_request(body: &str) -> Request<Body> {
        let timestamp = now_timestamp();
        const NONCE: &str = "nonce-123";
        let sign_string = format!("{}{}{}{}", timestamp, NONCE, "feishu_secret", body);
        let digest = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(sign_string.as_bytes());
            hex::encode(hasher.finalize())
        };
        Request::builder()
            .method("POST")
            .uri("/webhooks/feishu")
            .header("content-type", "application/json")
            .header("x-lark-request-timestamp", &timestamp)
            .header("x-lark-request-nonce", NONCE)
            .header("x-lark-signature", digest)
            .body(Body::from(body.to_string()))
            .unwrap()
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

        let req = signed_feishu_request(&payload.to_string());

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

    /// The window is the only replay defense on an unauthenticated route, so
    /// it gets its own tests rather than riding on the handler tests.
    #[test]
    fn timestamp_window_rejects_stale_and_future_and_garbage() {
        let now = chrono::Utc::now().timestamp();
        assert!(timestamp_is_fresh(&now.to_string()));
        assert!(
            timestamp_is_fresh(&(now - SIGNATURE_MAX_AGE_SECS as i64 + 10).to_string()),
            "just inside the window"
        );
        assert!(
            !timestamp_is_fresh(&(now - SIGNATURE_MAX_AGE_SECS as i64 - 10).to_string()),
            "just outside the window"
        );
        assert!(!timestamp_is_fresh(&"1234567890".to_string()), "a 2009 timestamp is a replay");
        assert!(!timestamp_is_fresh(&(now + 3600).to_string()), "far future");
        assert!(!timestamp_is_fresh(""), "empty");
        assert!(!timestamp_is_fresh("not-a-number"), "garbage");
    }

    /// End to end: a request that is *correctly signed* but old must still be
    /// refused — that is the whole point of the window.
    #[tokio::test]
    async fn slack_replay_of_an_old_signed_request_is_refused() {
        let state = std::sync::Arc::new(make_slack_webhook_state_with_secret().await);
        let app = create_webhook_router(state);
        let body_str = serde_json::json!({
            "type": "event_callback",
            "event": { "type": "message", "user": "U1", "text": "hi", "channel": "C1" }
        })
        .to_string();
        // Correctly signed, but an hour old.
        let old = (chrono::Utc::now().timestamp() - 3_600).to_string();
        let signature = make_slack_signature("slack_secret", &old, &body_str);
        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/slack")
            .header("content-type", "application/json")
            .header("x-slack-request-timestamp", old)
            .header("x-slack-signature", signature)
            .body(Body::from(body_str))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// A signature timestamp of "now" — the freshness window means a fixed
    /// timestamp from 2009 would fail verification for the wrong reason.
    fn now_timestamp() -> String {
        chrono::Utc::now().timestamp().to_string()
    }

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

        let req = signed_slack_request(&body_str);

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body, "slack_challenge_123");
    }

    /// The guard's own contract: one sighting per distinct delivery.
    #[test]
    fn replay_guard_allows_each_delivery_once() {
        let guard = ReplayGuard::default();
        assert!(guard.first_sighting(b"{\"id\": 1}"));
        assert!(!guard.first_sighting(b"{\"id\": 1}"), "the same delivery is not new");
        assert!(guard.first_sighting(b"{\"id\": 2}"), "a different delivery is");
        assert!(!guard.first_sighting(b"{\"id\": 2}"));
    }

    /// A retry is a replay: the platform resends the delivery byte for byte, so
    /// it is acknowledged (the platform should stop retrying) but not acted on
    /// twice.
    #[tokio::test]
    async fn a_repeated_signed_delivery_is_acknowledged_but_not_reprocessed() {
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
        let body = payload.to_string();

        let first = app
            .clone()
            .oneshot(signed_slack_request(&body))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = axum::body::to_bytes(first.into_body(), 4096).await.unwrap();
        assert_ne!(first_body.as_ref(), b"duplicate ignored");

        let second = app.oneshot(signed_slack_request(&body)).await.unwrap();
        assert_eq!(
            second.status(),
            StatusCode::OK,
            "a retry still gets 200, or the platform keeps retrying"
        );
        let second_body = axum::body::to_bytes(second.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            second_body.as_ref(),
            b"duplicate ignored",
            "the second delivery must be recognised as a replay"
        );
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

        let req = signed_slack_request(&body_str);

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

        let req = signed_slack_request(&body_str);

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

        let req = signed_slack_request(&body_str);

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

        let req = signed_slack_request(&body_str);

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
        let timestamp = now_timestamp();
        let signature = make_slack_signature("slack_secret", &timestamp, &body_str);

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
            // Fresh, so the refusal is the bad signature — not the age.
            .header("x-slack-request-timestamp", now_timestamp())
            .header("x-slack-signature", "v0=bad_signature")
            .body(Body::from(body_str))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    // ── Fail-closed verification (A4) ──────────────────────────────────────

    /// A WhatsApp POST without an app_secret must be refused, not processed —
    /// this was the unauthenticated message-injection path.
    #[tokio::test]
    async fn whatsapp_post_without_secret_is_refused() {
        let state = std::sync::Arc::new(make_secretless_webhook_state().await);
        let app = create_webhook_router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/whatsapp")
            .header("content-type", "application/json")
            .body(Body::from(r#"{ "entry": [] }"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// WhatsApp's subscription handshake must refuse the challenge when no
    /// verify_token is configured — accepting it made the URL verifiable by
    /// anyone who could reach the endpoint.
    #[tokio::test]
    async fn whatsapp_verify_without_token_is_refused() {
        let state = std::sync::Arc::new(make_secretless_webhook_state().await);
        let app = create_webhook_router(state);
        let req = Request::builder()
            .uri(
                "/webhooks/whatsapp/verify?hub_mode=subscribe&hub_verify_token=anything&\
                 hub_challenge=identity",
            )
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// A Slack POST without a signing_secret must be refused.
    #[tokio::test]
    async fn slack_post_without_secret_is_refused() {
        let state = std::sync::Arc::new(make_secretless_webhook_state().await);
        let app = create_webhook_router(state);
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": { "type": "message", "text": "hi", "channel": "C1" }
        });
        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/slack")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// A Feishu POST missing any of the signature pieces must be refused —
    /// previously a missing header slid the request straight past verification.
    #[tokio::test]
    async fn feishu_post_without_signature_headers_is_refused() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);
        // The state HAS a secret; the request simply omits the headers.
        let payload = serde_json::json!({ "event": { "type": "message" } });
        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/feishu")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn feishu_post_with_a_bad_signature_is_refused() {
        let state = std::sync::Arc::new(make_webhook_state().await);
        let app = create_webhook_router(state);
        let payload = serde_json::json!({ "event": { "type": "message" } });
        let req = Request::builder()
            .method("POST")
            .uri("/webhooks/feishu")
            .header("content-type", "application/json")
            // Fresh, so the refusal is the bad signature — not the age.
            .header("x-lark-request-timestamp", now_timestamp())
            .header("x-lark-request-nonce", "nonce-123")
            .header("x-lark-signature", "deadbeef")
            .body(Body::from(payload.to_string()))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
