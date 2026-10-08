//! Provider OAuth callback — where the browser lands after the user authorizes.
//!
//! This is one of the documented REST exceptions (`CLAUDE.md`): the redirect is
//! made by the provider's server and carried by a browser, so there is no WS
//! connection to answer on and no bearer token that could be attached. What
//! authenticates the request instead is the `state` we issued: single-use, bound
//! to a pending flow, and rejected otherwise. That is the same shape as
//! `/webhooks/*` — authentication by other means, not an open route — and it is
//! why the handler sits on the essential tier (rate limiting and security
//! headers, no token middleware).
//!
//! Nothing the provider sends is echoed back into the page. `error_description`
//! and friends arrive over the open internet and would otherwise be reflected
//! into HTML on the gateway's own origin.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::gateway::runtime::GatewayEvent;
use crate::gateway::state::GatewayState;

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    /// The value we issued in `providers.auth_start`.
    pub state: Option<String>,
    /// RFC 6749 §4.1.2.1: a refusal (or a provider-side failure) arrives here
    /// instead of a code.
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// A page a human reads in a browser tab.
///
/// Plain on purpose: no script and no stylesheet, so nothing here needs an
/// exemption from the security-headers middleware, and there is no markup for
/// provider-supplied text to be interpolated into.
fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>{title}</title></head><body><h1>{title}</h1><p>{body}</p></body></html>"
    ))
}

/// Tell anyone listening that a provider's authorization finished (or did not).
fn emit(state: &Arc<GatewayState>, event: GatewayEvent) {
    if let Err(e) = state.events.tx.send(event) {
        // No receivers, which is normal for a headless gateway — but an event
        // that nobody hears is still worth a line in the log.
        warn!("Provider auth event had no subscribers: {e}");
    }
}

/// GET /oauth/provider/callback?code=…&state=…
pub async fn callback_handler(
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<CallbackQuery>,
) -> Response {
    // The provider reporting a refusal: there is no code to exchange, but the
    // state still names the flow, so the failure can be attributed and the
    // pending entry cleared.
    if let Some(error) = q.error.as_deref() {
        warn!(
            "Provider OAuth failed before completion: {error}{}",
            q.error_description
                .as_deref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        );

        if let Some(state_param) = q.state.as_deref() {
            if let Some(provider) = state.provider_oauth.provider_of_state(state_param).await {
                state.provider_oauth.cancel(&provider).await;
                emit(
                    &state,
                    GatewayEvent::ProviderAuthFailed {
                        provider,
                        reason: error.to_string(),
                    },
                );
            }
        }

        return (
            StatusCode::OK,
            page(
                "Authorization not completed",
                "The provider reported that the authorization did not complete. \
                 You can close this window and try again.",
            ),
        )
            .into_response();
    }

    let (Some(code), Some(state_param)) = (q.code.as_deref(), q.state.as_deref()) else {
        warn!("Provider OAuth callback arrived without a code or a state");
        return (
            StatusCode::BAD_REQUEST,
            page(
                "Authorization not completed",
                "The response was missing the code or the state. \
                 You can close this window and try again.",
            ),
        )
            .into_response();
    };

    // Read the provider before completing: `complete` consumes the state, and a
    // failed exchange is only worth reporting if we know whose it was.
    let provider = state.provider_oauth.provider_of_state(state_param).await;

    match state.provider_oauth.complete(state_param, code).await {
        Ok(provider) => {
            emit(&state, GatewayEvent::ProviderAuthComplete { provider: provider.clone() });
            debug!("Provider '{provider}' authorized via OAuth");
            (
                StatusCode::OK,
                page("Authorized", "This provider is now authorized. You can close this window."),
            )
                .into_response()
        }
        Err(err) => {
            // The reason is ours or the provider's, never secret — but it is
            // logged rather than rendered, see the module note.
            warn!("Provider OAuth callback failed: {err}");
            if let Some(provider) = provider {
                emit(
                    &state,
                    GatewayEvent::ProviderAuthFailed {
                        provider,
                        reason: err.to_string(),
                    },
                );
            }
            (
                StatusCode::BAD_REQUEST,
                page(
                    "Authorization failed",
                    "The authorization could not be completed. \
                     You can close this window and start again.",
                ),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state_with_hermetic_oauth_store;
    use crate::gateway::GatewayConfig;
    use crate::model_router::OAuthConfig;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn oauth_config(token_url: String) -> OAuthConfig {
        OAuthConfig {
            client_id: "test-client".to_string(),
            auth_url: "https://provider.example/authorize".to_string(),
            token_url,
            scope: None,
            client_secret: None,
            redirect_base: None,
            refresh_token: None,
        }
    }

    fn query(code: Option<&str>, state: Option<&str>, error: Option<&str>) -> CallbackQuery {
        CallbackQuery {
            code: code.map(str::to_string),
            state: state.map(str::to_string),
            error: error.map(str::to_string),
            error_description: Some("provider text that must not reach the page".to_string()),
        }
    }

    /// A state whose credential storage is private to this test, plus the temp
    /// dir that backs it (held for the test's lifetime).
    async fn oauth_state() -> (Arc<GatewayState>, tempfile::TempDir) {
        let (state, dir) =
            make_test_state_with_hermetic_oauth_store(GatewayConfig::default()).await;
        (Arc::new(state), dir)
    }

    #[tokio::test]
    async fn callback_rejects_a_response_missing_the_code_or_the_state() {
        let (state, _dir) = oauth_state().await;
        let resp = callback_handler(State(state), Query(query(None, Some("s"), None))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let (state, _dir) = oauth_state().await;
        let resp = callback_handler(State(state), Query(query(Some("c"), None, None))).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn callback_rejects_an_unknown_state_without_announcing_anything() {
        let (state, _dir) = oauth_state().await;
        let mut rx = state.events.tx.subscribe();

        let resp = callback_handler(
            State(state.clone()),
            Query(query(Some("code"), Some("never-issued"), None)),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // There is nobody to attribute it to: the client never received a flow
        // id, so an event would be noise.
        assert!(rx.try_recv().is_err(), "no event should have been emitted");
    }

    #[tokio::test]
    async fn callback_completes_a_started_flow_and_announces_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at",
                "refresh_token": "rt-from-callback",
                "expires_in": 3600,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (state, _dir) = oauth_state().await;
        let started = state
            .provider_oauth
            .start(
                "grok",
                &oauth_config(format!("{}/token", server.uri())),
                "http://127.0.0.1:18080",
            )
            .await;
        let mut rx = state.events.tx.subscribe();

        let resp = callback_handler(
            State(state.clone()),
            Query(query(Some("auth-code"), Some(started.flow_id.as_str()), None)),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        // The credential landed where the resolver will look for it.
        assert!(state.provider_oauth.is_authorized("grok").await);
        assert!(
            matches!(
                rx.try_recv(),
                Ok(GatewayEvent::ProviderAuthComplete { provider }) if provider == "grok"
            ),
            "expected a completion event"
        );
    }

    #[tokio::test]
    async fn callback_reports_a_refusal_and_clears_the_flow() {
        let (state, _dir) = oauth_state().await;
        let started = state
            .provider_oauth
            .start(
                "grok",
                &oauth_config("https://provider.example/token".to_string()),
                "http://127.0.0.1:18080",
            )
            .await;
        let mut rx = state.events.tx.subscribe();

        let resp = callback_handler(
            State(state.clone()),
            Query(query(None, Some(started.flow_id.as_str()), Some("access_denied"))),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            matches!(
                rx.try_recv(),
                Ok(GatewayEvent::ProviderAuthFailed { provider, .. }) if provider == "grok"
            ),
            "expected a failure event"
        );
        // The flow is gone, not merely reported: a refusal must not leave a
        // state that could still be completed.
        assert_eq!(state.provider_oauth.status("grok").await.pending, None);
        assert!(state
            .provider_oauth
            .complete(&started.flow_id, "code")
            .await
            .is_err());
    }
}
