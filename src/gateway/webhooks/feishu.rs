//! Feishu/Lark webhook handler (signature verified in `signing`).

use super::signing::verify_feishu_signature;
use super::*;

/// Handle Feishu/Lark webhook with signature verification
pub(super) async fn feishu_webhook_handler(
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
