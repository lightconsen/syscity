//! WhatsApp Business API webhooks: subscription verification and inbound messages.

use super::signing::verify_hmac_sha256;
use super::*;

/// Verify WhatsApp webhook subscription (GET request for verification)
pub(super) async fn whatsapp_verify_handler(
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
pub(super) async fn whatsapp_webhook_handler(
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
