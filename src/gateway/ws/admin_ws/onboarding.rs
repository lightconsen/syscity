//! WS admin handlers: onboarding.

use std::sync::Arc;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Onboarding ──────────────────────────────────────────────────────────

/// `onboarding.status` — `{ "status": "pending" | "done" }`.
pub(crate) async fn handle_onboarding_status(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let dir = state.paths.workspace_data_dir();
    match crate::memory::onboarding::status(&dir).await {
        Ok(crate::memory::onboarding::OnboardingStatus::Done) => {
            WsResponse::ok(&req.id, serde_json::json!({ "status": "done" }))
        }
        Ok(crate::memory::onboarding::OnboardingStatus::Pending) => {
            WsResponse::ok(&req.id, serde_json::json!({ "status": "pending" }))
        }
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `onboarding.apply` — `{ ok: true }` on success.
pub(crate) async fn handle_onboarding_apply(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let payload: crate::memory::onboarding::OnboardingPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let dir = state.paths.workspace_data_dir();
    match crate::memory::onboarding::apply(&dir, &payload).await {
        Ok(()) => WsResponse::ok(&req.id, serde_json::json!({ "ok": true })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}
