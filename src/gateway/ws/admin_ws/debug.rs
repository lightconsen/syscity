//! WS admin handlers: debug surfaces.
//!
//! These exist to make platform behaviour testable without a model. The
//! gateway is the only thing that can drive the device bridge from outside an
//! agent turn, so an on-device feature like the virtual display is otherwise
//! only reachable by spending a real model call on it.

use std::sync::Arc;

use super::super::{required_str_param, WsRequest, WsResponse};
use crate::gateway::GatewayState;

/// The environment variable that turns [`handle_debug_tool_invoke`] on.
///
/// A method that runs a platform tool runs it outside the agent's approval
/// flow — the irreversible-action gate lives in the tool path an agent turn
/// builds, not in the tool itself. That is acceptable for a debugging aid and
/// not acceptable by default, so the operator has to ask for it.
pub(super) const DEBUG_TOOL_INVOKE_ENV: &str = "SYSCITY_DEBUG_TOOL_INVOKE";

/// `debug.tool.invoke` — run one platform tool and return its result.
///
/// Payload: `{ "tool": "<name>", "arguments": { ... } }`.
///
/// Only **platform** tools are reachable (screen, device bridge, the Android
/// display). Shell, file, browser and channel tools are not in these sets, so
/// this cannot become a general "run anything" endpoint.
pub(crate) async fn handle_debug_tool_invoke(
    req: &WsRequest,
    _state: &Arc<GatewayState>,
) -> WsResponse {
    let enabled = debug_enabled(std::env::var(DEBUG_TOOL_INVOKE_ENV).ok().as_deref());
    invoke_platform_tool(req, enabled).await
}

/// Whether the operator switched the method on.
///
/// Split out so the gate can be tested without touching the process
/// environment — which a test could not do anyway: `set_var` is `unsafe` since
/// the 2024 edition, and this crate denies unsafe code.
pub(crate) fn debug_enabled(value: Option<&str>) -> bool {
    value == Some("1")
}

/// The method proper, with the gate already decided.
pub(crate) async fn invoke_platform_tool(req: &WsRequest, enabled: bool) -> WsResponse {
    if !enabled {
        return WsResponse::err(
            &req.id,
            "DISABLED",
            format!(
                "debug.tool.invoke is off. Set {DEBUG_TOOL_INVOKE_ENV}=1 on the gateway to \
                 enable it — it runs a platform tool without an agent turn, and therefore \
                 without the approval flow."
            ),
        );
    }

    let tool_name = match required_str_param(req, "tool") {
        Ok(name) => name,
        Err(response) => return response,
    };
    let arguments = match super::super::parse_params::<serde_json::Value>(req) {
        Ok(value) => value
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
        Err(response) => return response,
    };

    let available: Vec<String> = crate::computer::platform::all_known_toolsets()
        .into_iter()
        .filter(|set| set.is_available())
        .flat_map(|set| set.tools())
        .map(|tool| tool.name().to_string())
        .collect();

    let tool = crate::computer::platform::all_known_toolsets()
        .into_iter()
        .filter(|set| set.is_available())
        .flat_map(|set| set.tools())
        .find(|tool| tool.name() == tool_name);

    let Some(tool) = tool else {
        return WsResponse::err(
            &req.id,
            "TOOL_NOT_FOUND",
            format!(
                "no platform tool named {tool_name:?} is available on this host; available: {}",
                available.join(", ")
            ),
        );
    };

    // A default context: these tools take no identity, no provider, and no
    // approval queue. That is exactly the caveat above.
    let context = crate::tools::ToolContext::default();
    match tool.execute(arguments, &context).await {
        Ok(result) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "tool": tool_name,
                "success": result.success,
                "output": result.output,
                "error": result.error,
                "data": result.data,
            }),
        ),
        Err(e) => WsResponse::err(&req.id, "TOOL_FAILED", format!("{e}")),
    }
}
