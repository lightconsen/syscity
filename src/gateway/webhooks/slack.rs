//! Slack Events API webhook handler and its v0 signature verification.

use super::signing::timestamp_is_fresh;
use super::*;

/// Handle Slack Events API webhooks
///
/// Supports URL verification and event callbacks (message events).
pub(super) async fn slack_webhook_handler(
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
