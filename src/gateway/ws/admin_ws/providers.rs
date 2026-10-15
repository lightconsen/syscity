//! WS admin handlers: providers.

use std::sync::Arc;

use serde::Deserialize;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Providers ───────────────────────────────────────────────────────────

/// `providers.health` — one provider's health (`{ id }`).
pub(crate) async fn handle_providers_health(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    match state.infra.model_router.get_provider_health(&id).await {
        Some(health) => WsResponse::ok(&req.id, serde_json::json!({ "id": id, "health": health })),
        None => WsResponse::err(&req.id, "NOT_FOUND", "provider not found"),
    }
}

/// `providers.check` — force a health check (`{ id }`).
pub(crate) async fn handle_providers_check(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    match state.infra.model_router.check_provider_health(&id).await {
        Ok(r) => WsResponse::ok(&req.id, serde_json::json!({ "id": id, "healthy": r })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `providers.switch` — set the default model (`{ model }`).
pub(crate) async fn handle_providers_switch(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let model = match super::required_str_param(req, "model") {
        Ok(v) => v,
        Err(res) => return res,
    };
    match state.infra.model_router.switch_default_model(&model).await {
        Ok(()) => WsResponse::ok(&req.id, serde_json::json!({ "success": true, "model": model })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `models.default` — the current default model.
pub(crate) async fn handle_models_default(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let default = state.infra.model_router.get_default_model().await;
    WsResponse::ok(&req.id, serde_json::json!({ "default_model": default }))
}

/// `providers.fallback` — the fallback chain for a model (`{ model_id }`).
pub(crate) async fn handle_providers_fallback(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let model_id = match super::required_str_param(req, "model_id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let chain = state.infra.model_router.get_fallback_chain(&model_id).await;
    WsResponse::ok(&req.id, serde_json::json!({ "model_id": model_id, "fallback_chain": chain }))
}

// ── Providers ───────────────────────────────────────────────────────────

/// `providers.list` — configured model providers.
pub(crate) async fn handle_providers_list(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let providers = state.infra.model_router.list_providers().await;
    WsResponse::ok(&req.id, serde_json::json!({ "providers": providers }))
}

/// `providers.enable` / `providers.disable` — toggle a provider.
pub(crate) async fn handle_providers_set_enabled(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    enabled: bool,
) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        id: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let result = if enabled {
        state.infra.model_router.enable_provider(&p.id).await
    } else {
        state.infra.model_router.disable_provider(&p.id).await
    };
    match result {
        Ok(()) => WsResponse::ok(&req.id, serde_json::json!({ "success": true })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `providers.usage` — provider usage snapshots with quota.
pub(crate) async fn handle_providers_usage(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let snapshots = state.infra.model_router.all_snapshots_with_quota().await;
    WsResponse::ok(&req.id, serde_json::json!({ "usage": snapshots }))
}

// ── Provider OAuth (authorization code + PKCE) ───────────────────────────

/// The origin a browser is sent back to when the provider's config does not
/// name one.
///
/// This is the gateway's own loopback origin — the same set
/// `ws_origin::local_origins` defines for browser Origin checks, reused rather
/// than restated. A wildcard bind (`0.0.0.0`) is deliberately not guessed at:
/// no browser can reach it, and a deployment reached by a LAN name or a tunnel
/// has to set `redirect_base` to the URI it registered with the provider.
fn default_redirect_base(config: &crate::gateway::GatewayConfig) -> String {
    crate::gateway::auth::ws_origin::local_origins(config.port)
        .into_iter()
        .find(|origin| origin.starts_with("http://127.0.0.1:"))
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", config.port))
}

/// `providers.auth_start` — begin an OAuth authorization (`{ id, mode? }`).
///
/// `mode` is `"browser"` (the default) or `"device"`. The browser flow returns
/// the URL to open and completes when the provider redirects to the gateway's
/// own callback route; the device flow returns a code to type at a verification
/// URI and completes when the client polls `providers.auth_status`. A provider
/// that declares only one of the two endpoints picks the mode for itself.
pub(crate) async fn handle_providers_auth_start(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let mode = match crate::gateway::ws::parse_params::<serde_json::Value>(req)
        .ok()
        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(str::to_string))
    {
        Some(mode) => mode,
        None => "browser".to_string(),
    };
    if mode != "browser" && mode != "device" {
        return WsResponse::err(
            &req.id,
            "INVALID_PARAMS",
            format!("unknown auth mode '{mode}': expected \"browser\" or \"device\""),
        );
    }

    // Scoped: the config lock is not held across the flow start.
    let (oauth, base) = {
        let config = state.config.read().await;
        if !config.providers.contains_key(&id) {
            return WsResponse::err(&req.id, "NOT_FOUND", format!("unknown provider '{id}'"));
        }
        let oauth = config.providers.get(&id).and_then(|p| p.oauth.clone());
        let Some(oauth) = oauth else {
            return WsResponse::err(
                &req.id,
                "INVALID_PARAMS",
                format!(
                    "provider '{id}' has no [providers.{id}.oauth] block, so there is nothing \
                     to authorize against"
                ),
            );
        };
        (oauth, default_redirect_base(&config))
    };

    // A provider that names one endpoint and not the other is asked for the one
    // it has, rather than refused for the mode the caller left at its default.
    let wants_device = match (mode.as_str(), &oauth.device_auth_url, &oauth.auth_url) {
        ("device", _, _) => true,
        ("browser", None, Some(_)) => false,
        ("browser", Some(_), None) => true,
        ("browser", ..) => false,
        _ => unreachable!("mode is validated above"),
    };

    if wants_device {
        let Some(device_auth_url) = oauth.device_auth_url.as_deref() else {
            return WsResponse::err(
                &req.id,
                "INVALID_PARAMS",
                format!(
                    "provider '{id}' declares no device_auth_url, so there is no device flow to \
                     start; use mode \"browser\""
                ),
            );
        };
        return match state
            .provider_oauth
            .start_device(&id, &oauth, device_auth_url)
            .await
        {
            Ok(started) => WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "id": id,
                    "flow_id": started.flow_id,
                    "device": {
                        "user_code": started.prompt.user_code,
                        "verification_uri": started.prompt.verification_uri,
                        "interval_secs": started.prompt.interval_secs,
                        "expires_in_secs": started.expires_in_secs,
                    },
                }),
            ),
            Err(e) => WsResponse::err(&req.id, "OAUTH_DEVICE_START_FAILED", e.to_string()),
        };
    }

    match state.provider_oauth.start(&id, &oauth, &base).await {
        Ok(started) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "id": id,
                "flow_id": started.flow_id,
                "auth_url": started.auth_url,
            }),
        ),
        Err(e) => WsResponse::err(&req.id, "OAUTH_START_FAILED", e.to_string()),
    }
}

/// `providers.auth_status` — is this provider authorized? (`{ id }`).
///
/// For a pending *device* flow this call also polls the provider, so a client
/// that asks repeatedly is what advances the login.
pub(crate) async fn handle_providers_auth_status(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };

    let status = state.provider_oauth.status(&id).await;
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "id": id,
            "authorized": status.authorized,
            "pending": status.pending,
            "device": status.device.map(|prompt| serde_json::json!({
                "user_code": prompt.user_code,
                "verification_uri": prompt.verification_uri,
                "interval_secs": prompt.interval_secs,
            })),
            "poll_error": status.poll_error,
        }),
    )
}

/// `providers.auth_cancel` — abandon a pending authorization (`{ id }`).
pub(crate) async fn handle_providers_auth_cancel(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };

    let cancelled = state.provider_oauth.cancel(&id).await;
    WsResponse::ok(&req.id, serde_json::json!({ "id": id, "cancelled": cancelled }))
}
