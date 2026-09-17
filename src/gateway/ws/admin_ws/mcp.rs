//! WS admin handlers: mcp.

use std::sync::Arc;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── MCP ─────────────────────────────────────────────────────────────────

/// `mcp.tools` — list a server's tools (`{ server_id }`).
pub(crate) async fn handle_mcp_tools(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let server_id = match parse_params::<serde_json::Value>(req) {
        Ok(v) => v["server_id"].as_str().unwrap_or("").to_string(),
        Err(res) => return res,
    };
    match state.tools.mcp_manager.get_client(&server_id).await {
        Some(client_arc) => {
            let client = client_arc.read().await;
            let tools = client.get_tools().to_vec();
            WsResponse::ok(&req.id, serde_json::json!({ "tools": tools }))
        }
        None => WsResponse::err(&req.id, "NOT_FOUND", "MCP server not connected"),
    }
}

/// `mcp.call_tool` — invoke a tool on a connected server (`{ server_id, tool, args }`).
///
/// Execution goes through the tool registry, not straight to the MCP client:
/// that is where blocked and degraded prefixes, policy hooks, the approval
/// route and the content filter live, and the agent path already goes through
/// it. Calling the client directly meant that being able to *list* a tool was
/// enough to run it, past every policy the operator had set — the registry's
/// classification of the same tool was simply never consulted.
///
/// The context carries no ask channel, so a tool that merely advertises
/// `requires_approval` does not stop to ask a human: the caller here *is* the
/// operator who asked for it. A policy hook that returns `NeedsApproval` is
/// still honoured — `execute_call` routes that through the full flow.
pub(crate) async fn handle_mcp_call_tool(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &crate::security::request_context::RequestContext,
) -> WsResponse {
    let server_id = match parse_params::<serde_json::Value>(req) {
        Ok(v) => v["server_id"].as_str().unwrap_or("").to_string(),
        Err(res) => return res,
    };
    let tool_name = match parse_params::<serde_json::Value>(req) {
        Ok(v) => v["tool"].as_str().unwrap_or("").to_string(),
        Err(res) => return res,
    };
    let args = req
        .params
        .clone()
        .and_then(|p| p["args"].clone().into())
        .unwrap_or_else(|| serde_json::json!({}));

    // MCP tools are registered under this name (`mcp/tools.rs`).
    let qualified = format!("mcp__{}__{}", server_id, tool_name);

    if state.tools.registry.has(&qualified) {
        let call = crate::providers::FunctionCall {
            name: qualified,
            arguments: args.to_string(),
        };
        let tool_ctx = crate::tools::ToolContext::new(ctx.user_id(), &req.id);
        return match state.tools.registry.execute_call(&call, &tool_ctx).await {
            Ok(result) => {
                // Keep the shape this method has always returned: the tool's
                // own payload under `result`.
                let payload = result.data.clone().unwrap_or_else(|| {
                    serde_json::to_value(&result).unwrap_or(serde_json::Value::Null)
                });
                WsResponse::ok(&req.id, serde_json::json!({ "result": payload }))
            }
            Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
        };
    }

    // Not registered: either the server is not connected, or it exposes more
    // tools than `max_tools` registered. The blocked list is name-based and so
    // still applies here; the rest of the policy needs a registry entry.
    if state.tools.registry.is_name_blocked(&qualified) {
        return WsResponse::err(&req.id, "FORBIDDEN", format!("Tool '{}' is blocked", qualified));
    }

    match state.tools.mcp_manager.get_client(&server_id).await {
        Some(client_arc) => {
            let client = client_arc.read().await;
            match client.call_tool(&tool_name, args).await {
                Ok(result) => WsResponse::ok(&req.id, serde_json::json!({ "result": result })),
                Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
            }
        }
        None => {
            WsResponse::err(&req.id, "NOT_FOUND", format!("MCP server '{}' not found", server_id))
        }
    }
}

/// `mcp.resources` — list a server's resources (`{ server_id }`).
pub(crate) async fn handle_mcp_resources(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let server_id = match parse_params::<serde_json::Value>(req) {
        Ok(v) => v["server_id"].as_str().unwrap_or("").to_string(),
        Err(res) => return res,
    };
    match state.tools.mcp_manager.get_client(&server_id).await {
        Some(client_arc) => {
            let client = client_arc.read().await;
            match client.list_resources().await {
                Ok(resources) => {
                    WsResponse::ok(&req.id, serde_json::json!({ "resources": resources }))
                }
                Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
            }
        }
        None => WsResponse::err(&req.id, "NOT_FOUND", "MCP server not connected"),
    }
}

/// `mcp.auth_status` — whether a server has a stored OAuth token (`{ server_id }`).
pub(crate) async fn handle_mcp_auth_status(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let server_id = match parse_params::<serde_json::Value>(req) {
        Ok(v) => v["server_id"].as_str().unwrap_or("").to_string(),
        Err(res) => return res,
    };
    let authorized = state.tools.mcp_manager.has_stored_token(&server_id).await;
    WsResponse::ok(&req.id, serde_json::json!({ "server_id": server_id, "authorized": authorized }))
}
