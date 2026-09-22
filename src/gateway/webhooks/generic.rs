//! Generic HMAC-verified webhook handler for custom integrations.

use super::signing::verify_hmac_sha256;
use super::*;

/// Generic webhook handler for custom integrations with HMAC verification
pub(super) async fn generic_webhook_handler(
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
