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

// ── Install lifecycle (pin / rollback / provenance) ──────────────────────

/// `skills.versions` — a skill's install record and rollback history (`{ id }`).
pub(crate) async fn handle_skills_versions(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.skill_install_state(&id).await {
        Ok((record, history)) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "id": id,
                "version": record.version,
                "source": record.source,
                "sha256": record.sha256,
                "pinned": record.pinned,
                "installed_at": record.installed_at,
                "usage": record.usage,
                "history": history,
            }),
        ),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}

/// `skills.pin` — hold or release a skill's version (`{ id, pinned }`).
pub(crate) async fn handle_skills_pin(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(serde::Deserialize)]
    struct Params {
        id: String,
        #[serde(default = "default_true")]
        pinned: bool,
    }
    fn default_true() -> bool {
        true
    }
    let p: Params = match super::super::parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.pin_skill(&p.id, p.pinned).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "success": true, "id": p.id, "pinned": p.pinned }),
        ),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}

/// `skills.rollback` — restore a version kept under `.history/` (`{ id, version }`).
pub(crate) async fn handle_skills_rollback(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(serde::Deserialize)]
    struct Params {
        id: String,
        version: String,
    }
    let p: Params = match super::super::parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.rollback_skill(&p.id, &p.version).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "success": true, "id": p.id, "version": p.version }),
        ),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}
