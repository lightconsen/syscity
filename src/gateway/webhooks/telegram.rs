//! Telegram Bot API webhook handler and its update payload types.

use super::*;

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
pub(super) async fn telegram_webhook_handler(
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
