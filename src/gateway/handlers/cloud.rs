//! Cloud session HTTP endpoint (feature `cloud`). The OAuth login must be a
//! browser redirect, so it stays HTTP; the web SPA reads `#token=` on the
//! callback and persists it over WS (`cloud.token`). All other session actions
//! (token/logout/subscription/usage) are WS-only via `ws/admin_ws.rs`.

#![cfg(feature = "cloud")]

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::cloud::session;
use crate::gateway::state::GatewayState;

#[derive(Debug, Deserialize)]
pub struct ProviderQuery {
    #[serde(default)]
    pub provider: Option<String>,
    /// Which UI started the sign-in: `desktop` (the shell hands the URL to
    /// the system browser, so the callback page has no opener and must not
    /// navigate anywhere), `popup` (a browser tab that can postMessage back
    /// and close itself) or `tab` (the current tab, which goes back to `/`).
    /// Unknown values are dropped rather than rejected: an old client that
    /// sends nothing keeps the previous behavior.
    #[serde(default)]
    pub flow: Option<String>,
}

/// The `flow` values the callback page understands. Anything else is dropped
/// (not an error) — the value is spliced into a redirect URL by the console,
/// so only a fixed set may travel.
const LOGIN_FLOWS: [&str; 3] = ["desktop", "popup", "tab"];

/// Keep `raw` only if it is one of [`LOGIN_FLOWS`].
fn accept_flow(raw: Option<&str>) -> Option<&str> {
    raw.filter(|f| LOGIN_FLOWS.contains(f))
}

/// GET /api/v1/login?provider=github|google|wechat → 302 to cloud OAuth.
pub async fn login_handler(
    State(state): State<Arc<GatewayState>>,
    Query(q): Query<ProviderQuery>,
) -> Response {
    let cfg = { state.config.read().await.cloud.clone() };
    if !cfg.enabled {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "cloud is not enabled (config cloud.enabled)".to_string(),
        )
            .into_response();
    }
    let provider = q.provider.as_deref().unwrap_or("github");
    let url = session::login_url(&cfg, provider, accept_flow(q.flow.as_deref()));
    Redirect::temporary(&url).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_flows_travel() {
        for f in ["desktop", "popup", "tab"] {
            assert_eq!(accept_flow(Some(f)), Some(f));
        }
        assert_eq!(accept_flow(None), None);
        // Anything else is dropped, not passed through: the value ends up
        // inside a URL the console hands to the provider.
        assert_eq!(accept_flow(Some("https://evil.example")), None);
        assert_eq!(accept_flow(Some("Desktop")), None);
        assert_eq!(accept_flow(Some("")), None);
    }
}
