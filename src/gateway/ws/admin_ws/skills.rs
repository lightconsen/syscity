//! WS admin handlers: skills.

use std::sync::Arc;

use super::super::{WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Skills ──────────────────────────────────────────────────────────────

/// `skills.get` — one skill (`{ name }`).
pub(crate) async fn handle_skills_get(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let name = match super::required_str_param(req, "name") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.get_skill(&name).await {
        Some(skill) => WsResponse::ok(&req.id, serde_json::to_value(&skill).unwrap_or_default()),
        None => WsResponse::err(&req.id, "NOT_FOUND", "skill not found"),
    }
}

/// `skills.enable` / `skills.disable` — `{ id, enabled }`.
pub(crate) async fn handle_skills_set_enabled(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    enabled: bool,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let mut sm = state.tools.skills_manager.write().await;
    match sm.set_skill_enabled(&id, enabled).await {
        Ok(()) => WsResponse::ok(&req.id, serde_json::json!({ "success": true, "id": id })),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}

/// `skills.uninstall` — remove a skill (`{ name }`).
pub(crate) async fn handle_skills_uninstall(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let name = match super::required_str_param(req, "name") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.uninstall_skill(&name).await {
        Ok(_) => WsResponse::ok(&req.id, serde_json::json!({ "success": true })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `skills.run` — activate a skill (`{ id }`).
pub(crate) async fn handle_skills_run(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.activate_skill(&id).await {
        Ok(_) => WsResponse::ok(&req.id, serde_json::json!({ "success": true, "id": id })),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}
