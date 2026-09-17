//! The one HTTP endpoint the WebSocket protocol needs: exchanging a credential
//! for a single-use upgrade ticket.
//!
//! CLAUDE.md's rule is "do not add a REST endpoint when a WS method can serve
//! the caller", and this is the case where one cannot: the caller has no
//! connection yet, and a browser cannot set headers on a WebSocket upgrade — so
//! without this, the only way to hand a credential over is to put it in the
//! upgrade URL. The endpoint is authenticated with the same Bearer credential
//! the WS upgrade accepts, and what it returns is worth that credential's own
//! scopes and nothing more (see [`crate::gateway::ws::tickets`]).

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Extension, Json};

use crate::gateway::middleware::RestAuthContext;
use crate::gateway::ws::tickets::TICKET_TTL;
use crate::gateway::GatewayState;

/// `POST /api/v1/ws-ticket` — mint a single-use ticket for a `/ws` upgrade.
///
/// The response is `{ "ticket": "...", "expires_in": <seconds> }`; the client
/// connects to `ws://…/ws?ticket=<ticket>` and the ticket is consumed by that
/// upgrade.
pub async fn ws_ticket_handler(
    State(state): State<Arc<GatewayState>>,
    auth: Option<Extension<RestAuthContext>>,
) -> impl IntoResponse {
    let (user_id, scopes) = match auth {
        Some(Extension(ctx)) => (ctx.user_id, ctx.scopes),
        // No credential was required, so the caller is an anonymous local
        // client — and gets exactly what such a client gets on the WS path.
        None => {
            let local_scopes = {
                let config = state.config.read().await;
                config.security.local_scopes.clone()
            };
            ("anonymous".to_string(), local_scopes)
        }
    };

    let ticket = state.ws_tickets.issue(user_id, scopes).await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ticket": ticket,
            "expires_in": TICKET_TTL.as_secs(),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    async fn ticket_from(response: axum::response::Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["expires_in"].as_u64().unwrap() > 0);
        json["ticket"].as_str().expect("a ticket").to_string()
    }

    /// With no credential required, the caller is an anonymous local client and
    /// the ticket is worth exactly what such a client gets on the WS path.
    #[tokio::test]
    async fn an_unauthenticated_caller_gets_a_local_scope_ticket() {
        let state = state().await;
        let ticket = ticket_from(
            ws_ticket_handler(State(state.clone()), None)
                .await
                .into_response(),
        )
        .await;

        let grant = state.ws_tickets.consume(&ticket).await.expect("consumable");
        assert_eq!(grant.user_id, "anonymous");
        let expected = state.config.read().await.security.local_scopes.clone();
        assert_eq!(grant.scopes, expected, "no wider than an anonymous local client");
    }

    /// The anti-widening property, at the endpoint: a ticket is a handover of
    /// the minting credential's own scopes.
    #[tokio::test]
    async fn a_ticket_carries_the_minting_credentials_scopes() {
        let state = state().await;
        let ctx = RestAuthContext {
            user_id: "read-only-session".to_string(),
            scopes: vec!["read".to_string()],
        };

        let ticket = ticket_from(
            ws_ticket_handler(State(state.clone()), Some(Extension(ctx)))
                .await
                .into_response(),
        )
        .await;

        let grant = state.ws_tickets.consume(&ticket).await.expect("consumable");
        assert_eq!(grant.user_id, "read-only-session");
        assert_eq!(
            grant.scopes,
            vec!["read".to_string()],
            "minting a ticket must not upgrade a read-only credential"
        );
    }
}
