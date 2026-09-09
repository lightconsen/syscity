//! connectors.* WebSocket handlers — expose MCP connector management to
//! frontends (list / install / enable / disable / uninstall / auth / sync).
//!
//! Mirrors the `connector_*` actions on `McpConnectionTool`, but as a direct
//! gateway surface so the UI does not need to ask the agent to run a tool.

use std::path::Path;

use crate::mcp::ConnectorSummary;

use super::*;

pub(super) async fn handle_connectors_list(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let connectors = match state.tools.connector_manager.list().await {
        Ok(list) => list,
        Err(e) => return connector_error(&req.id, e),
    };
    let entries: Vec<_> = connectors
        .iter()
        .map(|s| serde_json::to_value(s).unwrap_or_default())
        .collect();
    WsResponse::ok(&req.id, serde_json::json!({ "connectors": entries, "count": entries.len() }))
}

pub(super) async fn handle_connectors_install(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct InstallPayload {
        source_dir: String,
    }
    let payload: InstallPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if payload.source_dir.trim().is_empty() {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "source_dir is required");
    }
    match state
        .tools
        .connector_manager
        .install_from_dir(Path::new(&payload.source_dir))
        .await
    {
        Ok(summary) => {
            let state_name = lifecycle_state(&summary);
            emit_connector_event(state, &summary.id, state_name, Some(&summary)).await;
            WsResponse::ok(&req.id, serde_json::to_value(&summary).unwrap_or_default())
        }
        Err(e) => connector_error(&req.id, e),
    }
}

pub(super) async fn handle_connectors_enable(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match connector_id_param(req) {
        Ok(id) => id,
        Err(res) => return res,
    };
    match state.tools.connector_manager.enable(&id).await {
        Ok(summary) => {
            emit_connector_event(state, &id, "enabled", Some(&summary)).await;
            WsResponse::ok(&req.id, serde_json::to_value(&summary).unwrap_or_default())
        }
        Err(e) => connector_error(&req.id, e),
    }
}

pub(super) async fn handle_connectors_disable(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match connector_id_param(req) {
        Ok(id) => id,
        Err(res) => return res,
    };
    match state.tools.connector_manager.disable(&id).await {
        Ok(summary) => {
            emit_connector_event(state, &id, "disabled", Some(&summary)).await;
            WsResponse::ok(&req.id, serde_json::to_value(&summary).unwrap_or_default())
        }
        Err(e) => connector_error(&req.id, e),
    }
}

pub(super) async fn handle_connectors_uninstall(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match connector_id_param(req) {
        Ok(id) => id,
        Err(res) => return res,
    };
    match state.tools.connector_manager.uninstall(&id).await {
        Ok(()) => {
            emit_connector_event(state, &id, "uninstalled", None).await;
            WsResponse::ok(&req.id, serde_json::json!({ "id": id, "state": "uninstalled" }))
        }
        Err(e) => connector_error(&req.id, e),
    }
}

pub(super) async fn handle_connectors_auth_status(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match connector_id_param(req) {
        Ok(id) => id,
        Err(res) => return res,
    };
    match state.tools.connector_manager.auth_status(&id).await {
        Ok(text) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "id": id, "authenticated": text.is_some(), "text": text }),
        ),
        Err(e) => connector_error(&req.id, e),
    }
}

pub(super) async fn handle_connectors_updates(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct UpdatesPayload {
        #[serde(default)]
        auto_only: bool,
        #[serde(default)]
        apply: bool,
    }
    let payload: UpdatesPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if payload.apply {
        match state
            .tools
            .connector_manager
            .apply_updates(payload.auto_only)
            .await
        {
            Ok(applied) => {
                for id in &applied {
                    emit_connector_event(state, id, "updated", None).await;
                }
                WsResponse::ok(&req.id, serde_json::json!({ "applied": applied }))
            }
            Err(e) => connector_error(&req.id, e),
        }
    } else {
        match state.tools.connector_manager.check_updates().await {
            Ok(pending) => {
                let entries: Vec<_> = pending
                    .iter()
                    .map(|u| {
                        serde_json::json!({
                            "id": u.id,
                            "current_version": u.current_version,
                            "latest_version": u.latest_version,
                            "auto_update": u.entry.auto_update,
                        })
                    })
                    .collect();
                WsResponse::ok(&req.id, serde_json::json!({ "pending": entries }))
            }
            Err(e) => connector_error(&req.id, e),
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────

#[allow(clippy::result_large_err)]
fn connector_id_param(req: &WsRequest) -> std::result::Result<String, WsResponse> {
    #[derive(Debug, Deserialize)]
    struct IdPayload {
        id: String,
    }
    let payload: IdPayload = parse_params(req)?;
    let id = payload.id.trim().to_string();
    if id.is_empty() {
        return Err(WsResponse::err(&req.id, "INVALID_PARAMS", "id is required"));
    }
    Ok(id)
}

/// Lowercase lifecycle state ("installed"/"enabled"/"disabled"/"error").
pub(crate) fn lifecycle_state(summary: &ConnectorSummary) -> &'static str {
    match summary.state {
        crate::mcp::connectors::state::StateKind::Installed => "installed",
        crate::mcp::connectors::state::StateKind::Enabled => "enabled",
        crate::mcp::connectors::state::StateKind::Disabled => "disabled",
        crate::mcp::connectors::state::StateKind::Error => "error",
    }
}

fn connector_error(id: &str, e: crate::error::SyscityError) -> WsResponse {
    use crate::error::SyscityError;
    let code = match &e {
        SyscityError::NotFound { .. } => "CONNECTOR_NOT_FOUND",
        SyscityError::Validation(_) => "CONNECTOR_VALIDATION",
        _ => "CONNECTOR_ERROR",
    };
    WsResponse::err(id, code, e.to_string())
}

/// Broadcast a connector lifecycle change to subscribed frontends.
pub(crate) async fn emit_connector_event(
    state: &Arc<GatewayState>,
    id: &str,
    state_name: &str,
    summary: Option<&ConnectorSummary>,
) {
    let summary = summary.and_then(|s| serde_json::to_value(s).ok());
    let event = crate::gateway::GatewayEvent::ConnectorChanged {
        id: id.to_string(),
        state: state_name.to_string(),
        summary,
    };
    if let Err(e) = state.events.tx.send(event) {
        debug!("No receivers for connector event: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::protocol::gateway_event_to_ws;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::{GatewayConfig, GatewayEvent};

    fn req(id: &str, method: &str, params: Option<serde_json::Value>) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    const CONNECTOR_JSON: &str = r#"{
      "connector": {
        "id": "test-connector",
        "display_name": "Test Connector",
        "description": "A test connector",
        "version": "0.1.0"
      },
      "mcp": { "transport": "stdio", "command": "true", "args": [], "auto_connect": false },
      "skills": []
    }"#;

    #[tokio::test]
    async fn connectors_list_empty() {
        let state = state().await;
        let resp = handle_connectors_list(&req("r1", "connectors.list", None), &state).await;
        assert!(resp.ok, "list failed: {:?}", resp.error);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["count"], 0);
    }

    #[tokio::test]
    async fn connectors_enable_missing_id_errors() {
        let state = state().await;
        let resp = handle_connectors_enable(
            &req("r1", "connectors.enable", Some(serde_json::json!({ "id": "" }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn connectors_enable_unknown_not_found() {
        let state = state().await;
        let resp = handle_connectors_enable(
            &req("r1", "connectors.enable", Some(serde_json::json!({ "id": "nope" }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "CONNECTOR_NOT_FOUND");
    }

    #[tokio::test]
    async fn connectors_disable_unknown_not_found() {
        let state = state().await;
        let resp = handle_connectors_disable(
            &req("r1", "connectors.disable", Some(serde_json::json!({ "id": "nope" }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "CONNECTOR_NOT_FOUND");
    }

    #[tokio::test]
    async fn connectors_auth_status_unknown_not_found() {
        let state = state().await;
        let resp = handle_connectors_auth_status(
            &req("r1", "connectors.auth_status", Some(serde_json::json!({ "id": "nope" }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "CONNECTOR_NOT_FOUND");
    }

    #[tokio::test]
    async fn connectors_install_then_list() {
        let state = state().await;
        let pkg = std::env::temp_dir().join(format!("conn_pkg_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("connector.json"), CONNECTOR_JSON).unwrap();

        let resp = handle_connectors_install(
            &req(
                "r1",
                "connectors.install",
                Some(serde_json::json!({ "source_dir": pkg.display().to_string() })),
            ),
            &state,
        )
        .await;
        assert!(resp.ok, "install failed: {:?}", resp.error);
        let summary = resp.payload.as_ref().unwrap();
        assert_eq!(summary["id"], "test-connector");
        assert_eq!(summary["state"], "installed");
        // MCP-capable connector probed live: installed-but-not-enabled → false.
        assert_eq!(summary["connected"], false);

        let resp = handle_connectors_list(&req("r2", "connectors.list", None), &state).await;
        assert!(resp.ok);
        let connectors = resp.payload.unwrap()["connectors"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(connectors.len(), 1);
        assert_eq!(connectors[0]["id"], "test-connector");
        // auto_connect: false → still `installed`, live connection false.
        assert_eq!(connectors[0]["state"], "installed");
        assert_eq!(connectors[0]["connected"], false);

        let _ = std::fs::remove_dir_all(&pkg);
    }

    #[test]
    fn connector_event_maps_to_ws_name() {
        let evt = GatewayEvent::ConnectorChanged {
            id: "c1".to_string(),
            state: "enabled".to_string(),
            summary: None,
        };
        let (name, payload) = gateway_event_to_ws(&evt).unwrap();
        assert_eq!(name, "connector.enabled");
        assert_eq!(payload["id"], "c1");
        assert_eq!(payload["state"], "enabled");
    }
}
