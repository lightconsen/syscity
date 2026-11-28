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

// ── Agent access tokens ──

use crate::gateway::config::AgentTokenEntry;

fn agent_entry(agent_id: &str) -> AgentTokenEntry {
    AgentTokenEntry {
        token: "tok".into(),
        principal: String::new(),
        agent_id: agent_id.into(),
        scopes: crate::gateway::config::default_agent_token_scopes(),
    }
}

/// `auth_mode = "none"` checks no token at all, so an agent token there is a
/// credential that authenticates nobody while reading, in the file, as if it
/// restricted who reaches which agent. Refused, not warned.
#[test]
fn validate_auth_refuses_agent_tokens_without_auth() {
    let mut config = GatewayConfig::default();
    config
        .security
        .agent_tokens
        .insert("alice".into(), agent_entry("alpha"));
    let err = super::validate_auth_config(&config).expect_err("must refuse");
    assert!(err.to_string().contains("agent_tokens"), "{err}");
    assert!(err.to_string().contains("auth_mode"), "{err}");
}

/// With a token-checking auth mode the same table is accepted.
#[test]
fn validate_auth_allows_agent_tokens_with_token_mode() {
    let mut config = GatewayConfig::default();
    config.security.auth_mode = AuthMode::Token;
    config
        .security
        .agent_tokens
        .insert("alice".into(), agent_entry("alpha"));
    assert!(super::validate_auth_config(&config).is_ok());
}

/// An entry with no token would authenticate nobody — a config error, since
/// the shape is almost certainly one field left unwritten.
#[test]
fn validate_auth_refuses_an_agent_token_without_a_token() {
    let mut config = GatewayConfig::default();
    config.security.auth_mode = AuthMode::Token;
    let mut entry = agent_entry("alpha");
    entry.token.clear();
    config.security.agent_tokens.insert("alice".into(), entry);
    let err = super::validate_auth_config(&config).expect_err("must refuse");
    assert!(err.to_string().contains("has no token"), "{err}");
}

/// An entry with no `agent_id` would bind to a nonexistent agent and be
/// refused everywhere with no clue why.
#[test]
fn validate_auth_refuses_an_agent_token_without_an_agent() {
    let mut config = GatewayConfig::default();
    config.security.auth_mode = AuthMode::Token;
    let mut entry = agent_entry("");
    entry.token = "tok".into();
    config.security.agent_tokens.insert("alice".into(), entry);
    let err = super::validate_auth_config(&config).expect_err("must refuse");
    assert!(err.to_string().contains("has no agent_id"), "{err}");
}

/// One token in two entries makes the binding depend on `HashMap` order, which
/// is randomized per process: the same Desktop would reach `alpha` after one
/// restart and `beta` after the next. Refused, naming both labels.
#[test]
fn validate_auth_refuses_a_token_shared_by_two_entries() {
    let mut config = GatewayConfig::default();
    config.security.auth_mode = AuthMode::Token;
    let mut alice = agent_entry("alpha");
    alice.token = "shared-token".into();
    let mut bob = agent_entry("beta");
    bob.token = "shared-token".into();
    config.security.agent_tokens.insert("alice".into(), alice);
    config.security.agent_tokens.insert("bob".into(), bob);
    let err = super::validate_auth_config(&config).expect_err("must refuse");
    let msg = err.to_string();
    assert!(msg.contains("share the same token"), "{msg}");
    assert!(msg.contains("alice") && msg.contains("bob"), "{msg}");
}

/// The duplicate check compares tokens exactly as `agent_token_match` does —
/// with no trimming — so `"tok-a"` and `" tok-a "` are two distinct credentials
/// and must not be refused as a duplicate. Trimming here would reject a config
/// that works.
#[test]
fn validate_auth_does_not_treat_a_padded_token_as_the_same_token() {
    let mut config = GatewayConfig::default();
    config.security.auth_mode = AuthMode::Token;
    let mut alice = agent_entry("alpha");
    alice.token = "tok-a".into();
    let mut bob = agent_entry("beta");
    bob.token = " tok-a ".into();
    config.security.agent_tokens.insert("alice".into(), alice);
    config.security.agent_tokens.insert("bob".into(), bob);
    assert!(super::validate_auth_config(&config).is_ok());
}
