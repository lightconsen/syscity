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

/// A wildcard origin together with `allow_credentials` tells every browser
/// that any site may make credentialed requests — refused at startup rather
/// than mirrored at runtime.
#[test]
fn validate_auth_refuses_wildcard_cors_with_credentials() {
    let mut config = GatewayConfig::default();
    config.security.cors.allowed_origins = vec!["*".into()];
    config.security.cors.allow_credentials = true;
    let err = super::validate_auth_config(&config).expect_err("must refuse");
    assert!(err.to_string().contains("allow_credentials"), "{err}");
}

/// The same credentials setting is fine once the origins are listed.
#[test]
fn validate_auth_allows_credentials_with_an_explicit_origin_list() {
    let mut config = GatewayConfig::default();
    config.security.cors.allowed_origins = vec!["https://app.example".into()];
    config.security.cors.allow_credentials = true;
    assert!(super::validate_auth_config(&config).is_ok());
}

/// And the default (wildcard, no credentials) is untouched — this is the
/// combination a browser will not send credentials to.
#[test]
fn validate_auth_allows_the_default_cors() {
    let config = GatewayConfig::default();
    assert_eq!(config.security.cors.allowed_origins, vec!["*".to_string()]);
    assert!(!config.security.cors.allow_credentials);
    assert!(super::validate_auth_config(&config).is_ok());
}

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
