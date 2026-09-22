//! Webhook receiver tests, driven through the real router.

use super::signing::{
    timestamp_is_fresh, verify_feishu_signature, verify_hmac_sha256, SIGNATURE_MAX_AGE_SECS,
};
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

    let mut whatsapp = crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Whatsapp);
    whatsapp
        .credentials
        .insert("verify_token".to_string(), "secret123".to_string());
    // The webhook verification secret the handlers now require.
    whatsapp
        .credentials
        .insert("app_secret".to_string(), "whatsapp_secret".to_string());
    config.channels.insert("whatsapp".to_string(), whatsapp);

    let mut telegram = crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Telegram);
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

    let mut disabled = crate::gateway::ChannelConfig::new(crate::channels::ChannelType::Whatsapp);
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
