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
