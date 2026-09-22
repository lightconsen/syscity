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

use feishu::feishu_webhook_handler;
use generic::generic_webhook_handler;
use slack::slack_webhook_handler;
use telegram::telegram_webhook_handler;
use whatsapp::{whatsapp_verify_handler, whatsapp_webhook_handler};

mod feishu;
mod generic;
mod signing;
mod slack;
mod telegram;
#[cfg(test)]
mod tests;
#[cfg(feature = "wechatmp")]
mod wechatmp;
mod whatsapp;
#[cfg(feature = "wechatmp")]
use wechatmp::{wechatmp_verify_handler, wechatmp_webhook_handler};

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
struct ReplayGuard {
    seen: std::sync::Mutex<HashMap<String, std::time::Instant>>,
}

impl ReplayGuard {
    /// Record a delivery. `false` means it was already seen inside the window.
    ///
    /// Either way the caller answers 200 — the platform should stop retrying —
    /// but a delivery that is not new must not be processed again.
    fn first_sighting(&self, body: &[u8]) -> bool {
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

    // One guard for the process: a delivery is a replay wherever in the router
    // it arrives.
    router
        .layer(axum::Extension(Arc::new(ReplayGuard::default())))
        .with_state(state)
}
