//! WeChat Official Account (公众号) webhook handlers (feature-gated).

use super::*;

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
pub(super) async fn wechatmp_verify_handler(
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
pub(super) async fn wechatmp_webhook_handler(
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
