//! Gateway API route integration tests
//!
//! Tests for the admin-tier HTTP API endpoints.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use tower::ServiceExt;

use super::*;

// ── GET /ready (not ready by default) ──

#[tokio::test]
async fn ready_handler_returns_503_when_not_ready() {
    let state =
        Arc::new(crate::gateway::state_tests::make_test_state(GatewayConfig::default()).await);
    let app = Router::new()
        .route("/ready", get(super::ready_handler))
        .with_state(state);

    let req = Request::builder()
        .uri("/ready")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ready"], false);
}

// ── Auth mode ambiguity detection ──

use crate::gateway::protocol::AuthMode;

#[test]
fn validate_auth_passes_when_security_disabled() {
    let mut config = GatewayConfig::default();
    config.security.enabled = false;
    config.security.shared_token = Some("test-token".into());
    // auth_mode is None by default — should NOT fail
    assert!(super::validate_auth_config(&config).is_ok());
}

#[test]
fn validate_auth_passes_when_only_token_configured() {
    let mut config = GatewayConfig::default();
    config.security.auth_required = true;
    config.security.shared_token = Some("test-token".into());
    config.security.auth_mode = AuthMode::None;
    // Only token — should warn but not fail
    assert!(super::validate_auth_config(&config).is_ok());
}

#[test]
fn validate_auth_passes_when_token_mode_explicit() {
    let mut config = GatewayConfig::default();
    config.security.enabled = true;
    config.security.auth_required = true;
    config.security.shared_token = Some("test-token".into());
    config.security.auth_mode = AuthMode::Token;
    // Token mode explicitly set — should pass
    assert!(super::validate_auth_config(&config).is_ok());
}
