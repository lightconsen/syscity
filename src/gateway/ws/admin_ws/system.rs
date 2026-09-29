//! WS admin handlers: system.

use std::sync::Arc;

use serde::Deserialize;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── System reload / channels ────────────────────────────────────────────

/// `system.reload` — reload plugins/config/providers/MCP/skills without a
/// restart. `{ scope }` is "all" by default (also "plugins" | "config" |
/// "providers" | "mcp" | "skills" | "channels"). Mirrors the former
/// `POST /api/v1/reload`.
pub(crate) async fn handle_system_reload(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        #[serde(default = "default_reload_scope")]
        scope: String,
    }
    fn default_reload_scope() -> String {
        "all".to_string()
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let result = crate::gateway::handlers::admin::run_reload(state, &p.scope).await;
    WsResponse::ok(&req.id, result)
}

/// `channels.list` — all configured channels and their enabled state.
pub(crate) async fn handle_channels_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let config = state.config.read().await;
    let channels: Vec<serde_json::Value> = config
        .channels
        .iter()
        .map(|(name, cfg)| {
            serde_json::json!({
                "name": name,
                "type": cfg.channel_type,
                "enabled": cfg.enabled,
                "agent_id": cfg.agent_id,
            })
        })
        .collect();
    WsResponse::ok(&req.id, serde_json::json!({ "channels": channels }))
}

/// `channels.enable` — enable a channel (`{ name }`), persisting to config.
pub(crate) async fn handle_channels_enable(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let name = match super::required_str_param(req, "name") {
        Ok(v) => v,
        Err(res) => return res,
    };
    {
        let mut config_guard = state.config.write().await;
        let config = Arc::make_mut(&mut config_guard);
        let Some(channel_config) = config.channels.get_mut(&name) else {
            return WsResponse::err(&req.id, "NOT_FOUND", format!("Channel '{}' not found", name));
        };
        if channel_config.enabled {
            return WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "name": name,
                    "enabled": true,
                    "message": "Channel is already enabled",
                }),
            );
        }
        channel_config.enabled = true;
    }
    if let Err(res) = super::super::persist_config(state).await {
        return res;
    }
    tracing::info!("Enabled channel '{}' via WS", name);
    WsResponse::ok(
        &req.id,
        serde_json::json!({ "name": name, "enabled": true, "message": "Channel enabled" }),
    )
}

/// `channels.disable` — disable a channel (`{ name }`), persisting to config.
pub(crate) async fn handle_channels_disable(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let name = match super::required_str_param(req, "name") {
        Ok(v) => v,
        Err(res) => return res,
    };
    {
        let mut config_guard = state.config.write().await;
        let config = Arc::make_mut(&mut config_guard);
        let Some(channel_config) = config.channels.get_mut(&name) else {
            return WsResponse::err(&req.id, "NOT_FOUND", format!("Channel '{}' not found", name));
        };
        if !channel_config.enabled {
            return WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "name": name,
                    "enabled": false,
                    "message": "Channel is already disabled",
                }),
            );
        }
        channel_config.enabled = false;
    }
    if let Err(res) = super::super::persist_config(state).await {
        return res;
    }
    tracing::info!("Disabled channel '{}' via WS", name);
    WsResponse::ok(
        &req.id,
        serde_json::json!({ "name": name, "enabled": false, "message": "Channel disabled" }),
    )
}
