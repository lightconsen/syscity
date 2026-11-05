//! Device capability WS handlers (mobile-migration §4.1/§4.2/§4.5, §4.12).
//!
//! These methods forward to the native [`DeviceBridge`] (when present). On
//! desktop the bridge is always `None`, so every handler returns
//! `UNSUPPORTED_PLATFORM` — the SPA hides the Devices tab when not on mobile.

use serde_json::json;

use super::*;

/// Logical capabilities surfaced in the SPA Devices tab. Permission-free
/// capabilities (haptics / SAF picker / adb pairing) report `granted: true`
/// unconditionally; the rest query the bridge's `permission.status`.
const CAPABILITIES: &[(&str, &str)] = &[
    ("camera", "Camera"),
    ("location", "Location"),
    ("notifications", "Notifications"),
    ("haptics", "Haptics"),
    ("file_pick", "Document picker"),
    ("adb", "Wireless debugging"),
];

fn needs_runtime_permission(cap_id: &str) -> bool {
    matches!(cap_id, "camera" | "location" | "notifications")
}

/// Clone the bridge, or return the standard unsupported-platform response.
async fn bridge_or_unsupported(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> Result<Arc<dyn crate::device::DeviceBridge>, WsResponse> {
    match state.device.bridge.read().await.as_ref() {
        Some(b) => Ok(b.clone()),
        None => Err(WsResponse::err(&req.id, "UNSUPPORTED_PLATFORM", crate::device::NO_BRIDGE_MSG)),
    }
}

/// `device.capabilities` — list device capabilities with grant state.
pub(super) async fn handle_device_capabilities(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let bridge = match bridge_or_unsupported(req, state).await {
        Ok(b) => b,
        Err(e) => return e,
    };

    let mut capabilities = Vec::new();
    for (id, label) in CAPABILITIES {
        let granted = if needs_runtime_permission(id) {
            match bridge
                .call(crate::device::CMD_PERMISSION_STATUS, json!({ "permission": id }))
                .await
            {
                Ok(v) => v.get("granted").and_then(|g| g.as_bool()).unwrap_or(false),
                Err(_) => false,
            }
        } else {
            true
        };
        capabilities.push(json!({
            "id": id,
            "label": label,
            "available": true,
            "granted": granted,
        }));
    }
    WsResponse::ok(&req.id, json!({ "capabilities": capabilities }))
}

/// `device.permission.status` — report a runtime permission's grant state.
pub(super) async fn handle_device_permission_status(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct StatusPayload {
        permission: String,
    }
    let payload: StatusPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let bridge = match bridge_or_unsupported(req, state).await {
        Ok(b) => b,
        Err(e) => return e,
    };
    match bridge
        .call(
            crate::device::CMD_PERMISSION_STATUS,
            json!({ "permission": payload.permission }),
        )
        .await
    {
        Ok(data) => WsResponse::ok(&req.id, data),
        Err(e) => WsResponse::err(&req.id, "DEVICE_COMMAND_FAILED", format!("{}", e)),
    }
}

/// `device.permission.request` — ask the user to grant a runtime permission.
pub(super) async fn handle_device_permission_request(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct RequestPayload {
        permission: String,
    }
    let payload: RequestPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let bridge = match bridge_or_unsupported(req, state).await {
        Ok(b) => b,
        Err(e) => return e,
    };
    match bridge
        .call(
            crate::device::CMD_REQUEST_PERMISSION,
            json!({ "permission": payload.permission }),
        )
        .await
    {
        Ok(data) => WsResponse::ok(&req.id, data),
        Err(e) => WsResponse::err(&req.id, "DEVICE_COMMAND_FAILED", format!("{}", e)),
    }
}

/// `device.shortcut.run` — hand off to the Shortcuts app (§4.6).
pub(super) async fn handle_device_shortcut_run(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct RunPayload {
        name: String,
        #[serde(default)]
        input: Option<String>,
    }
    let payload: RunPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let bridge = match bridge_or_unsupported(req, state).await {
        Ok(b) => b,
        Err(e) => return e,
    };
    match bridge
        .call(
            crate::device::CMD_RUN_SHORTCUT,
            json!({ "name": payload.name, "input": payload.input }),
        )
        .await
    {
        Ok(data) => WsResponse::ok(&req.id, data),
        Err(e) => WsResponse::err(&req.id, "DEVICE_COMMAND_FAILED", format!("{}", e)),
    }
}

/// `device.shortcut.results` — list + consume outputs from SyscityOutputIntent.
pub(super) async fn handle_device_shortcut_results(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let bridge = match bridge_or_unsupported(req, state).await {
        Ok(b) => b,
        Err(e) => return e,
    };
    match bridge
        .call(crate::device::CMD_SHORTCUT_RESULTS, json!({}))
        .await
    {
        Ok(data) => WsResponse::ok(&req.id, data),
        Err(e) => WsResponse::err(&req.id, "DEVICE_COMMAND_FAILED", format!("{}", e)),
    }
}

/// `device.shortcut.inbox` — list + consume AskSyscity prompts.
pub(super) async fn handle_device_shortcut_inbox(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let bridge = match bridge_or_unsupported(req, state).await {
        Ok(b) => b,
        Err(e) => return e,
    };
    match bridge
        .call(crate::device::CMD_SHORTCUT_INBOX, json!({}))
        .await
    {
        Ok(data) => WsResponse::ok(&req.id, data),
        Err(e) => WsResponse::err(&req.id, "DEVICE_COMMAND_FAILED", format!("{}", e)),
    }
}

/// `device.adb.status` — report loopback adb pairing status (§4.5).
pub(super) async fn handle_device_adb_status(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    // Gate on the device bridge: absent on desktop, where adb runs on the
    // host's PATH and this surface does not exist.
    if state.device.bridge.read().await.is_none() {
        return WsResponse::err(&req.id, "UNSUPPORTED_PLATFORM", crate::device::NO_BRIDGE_MSG);
    }
    match crate::computer::platform::android::adb::adb_status().await {
        Ok(data) => WsResponse::ok(&req.id, data),
        Err(e) => WsResponse::err(&req.id, "DEVICE_COMMAND_FAILED", format!("{}", e)),
    }
}

/// `device.adb.pair` — pair with the local wireless-debugging adb server.
pub(super) async fn handle_device_adb_pair(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct PairPayload {
        port: u16,
        code: String,
        #[serde(default)]
        connect_port: Option<u16>,
    }
    let payload: PairPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if state.device.bridge.read().await.is_none() {
        return WsResponse::err(&req.id, "UNSUPPORTED_PLATFORM", crate::device::NO_BRIDGE_MSG);
    }
    match crate::computer::platform::android::adb::adb_pair(
        payload.port,
        &payload.code,
        payload.connect_port,
    )
    .await
    {
        Ok(data) => WsResponse::ok(&req.id, data),
        Err(e) => WsResponse::err(&req.id, "DEVICE_COMMAND_FAILED", format!("{}", e)),
    }
}
