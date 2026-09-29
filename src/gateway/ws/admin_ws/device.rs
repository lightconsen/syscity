//! WS admin handlers: device.

use std::sync::Arc;

use super::super::{WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Device pairing ──────────────────────────────────────────────────────

/// `device.pairing.pending` — pending pairing requests.
pub(crate) async fn handle_device_pairing_pending(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let pending = state.auth.device_pairing_store.list_pending().await;
    WsResponse::ok(&req.id, serde_json::json!({ "pending": pending }))
}

/// `device.pairing.authorized` — authorized devices.
pub(crate) async fn handle_device_pairing_authorized(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let devices = state.auth.device_pairing_store.list_authorized().await;
    WsResponse::ok(&req.id, serde_json::json!({ "devices": devices }))
}

/// `device.pairing.approve` — approve a pairing request (`{ code }`).
pub(crate) async fn handle_device_pairing_approve(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let code = match super::required_str_param(req, "code") {
        Ok(v) => v,
        Err(res) => return res,
    };
    match state
        .auth
        .device_pairing_store
        .approve(&code, Some("admin"))
        .await
    {
        Some(_) => WsResponse::ok(&req.id, serde_json::json!({ "status": "approved" })),
        None => WsResponse::err(&req.id, "NOT_FOUND", "pairing request not found or expired"),
    }
}

/// `device.pairing.reject` — reject a pairing request (`{ code }`).
pub(crate) async fn handle_device_pairing_reject(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let code = match super::required_str_param(req, "code") {
        Ok(v) => v,
        Err(res) => return res,
    };
    match state.auth.device_pairing_store.reject(&code).await {
        Some(_) => WsResponse::ok(&req.id, serde_json::json!({ "status": "rejected" })),
        None => WsResponse::err(&req.id, "NOT_FOUND", "pairing request not found"),
    }
}

/// `device.pairing.revoke` — revoke an authorized device (`{ device_id }`).
pub(crate) async fn handle_device_pairing_revoke(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let device_id = match super::required_str_param(req, "device_id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    match state.auth.device_pairing_store.revoke(&device_id).await {
        true => WsResponse::ok(&req.id, serde_json::json!({ "status": "revoked" })),
        false => WsResponse::err(&req.id, "NOT_FOUND", "device not found"),
    }
}

/// `device.pairing.qr` — the pairing QR SVG for a pending code
/// (`{ code }`, returns `{ svg }`). SVG is text/XML so it fits in a WS
/// payload; formerly `GET /api/v1/device/pairing/qr/:code`.
pub(crate) async fn handle_device_pairing_qr(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let code = match super::required_str_param(req, "code") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let pending = state.auth.device_pairing_store.list_pending().await;
    if !pending.iter().any(|r| r.code == code) {
        return WsResponse::err(&req.id, "NOT_FOUND", "pairing code not found or expired");
    }
    let uri = crate::security::device_pairing::DevicePairingStore::pairing_uri(&code);
    match crate::security::device_pairing::DevicePairingStore::generate_qr_svg(&uri) {
        Ok(svg) => WsResponse::ok(&req.id, serde_json::json!({ "code": code, "svg": svg })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", &e),
    }
}

/// `device.pairing.setup` — decode a base64url setup token and return the
/// pending request details (`{ setup_code }`). Formerly
/// `GET /api/v1/device/pairing/setup/:setup_code`.
pub(crate) async fn handle_device_pairing_setup(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    use std::time::SystemTime;
    let setup_code = match super::required_str_param(req, "setup_code") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let code =
        match crate::security::device_pairing::DevicePairingStore::decode_setup_code(&setup_code) {
            Some(code) => code,
            None => return WsResponse::err(&req.id, "INVALID_PARAMS", "invalid setup code"),
        };
    let pending = state.auth.device_pairing_store.list_pending().await;
    match pending.into_iter().find(|r| r.code == code) {
        Some(req_) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "code": req_.code,
                "device_id": req_.device_id,
                "display_name": req_.display_name,
                "expires_at": req_.expires_at.duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            }),
        ),
        None => WsResponse::err(&req.id, "NOT_FOUND", "pairing code not found or expired"),
    }
}
