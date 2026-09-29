//! WS admin handlers: auth_profiles.

use std::sync::Arc;

use super::super::{WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Auth profiles (provider API-key state) ──────────────────────────────

/// `auth_profiles.list` — all auth profiles across providers.
pub(crate) async fn handle_auth_profiles_list(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let profiles = state.infra.model_router.list_auth_profiles().await;
    WsResponse::ok(&req.id, serde_json::json!({ "profiles": profiles, "count": profiles.len() }))
}

/// `auth_profiles.get` — auth profile status for a provider (`{ id }`).
pub(crate) async fn handle_auth_profiles_get(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    match state.infra.model_router.get_auth_profile_status(&id).await {
        Some(status) => WsResponse::ok(&req.id, serde_json::to_value(status).unwrap_or_default()),
        None => WsResponse::err(
            &req.id,
            "NOT_FOUND",
            format!("No auth profile found for provider '{}'", id),
        ),
    }
}

/// `auth_profiles.rotate` — rotate a provider's API key (`{ id }`).
pub(crate) async fn handle_auth_profiles_rotate(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    match state.infra.model_router.rotate_auth_key(&id).await {
        Ok(_new_key) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "success": true,
                "provider": id,
                "message": format!("Auth key rotated for provider '{}'", id),
            }),
        ),
        Err(e) => {
            WsResponse::err(&req.id, "BAD_REQUEST", format!("Failed to rotate auth key: {}", e))
        }
    }
}
