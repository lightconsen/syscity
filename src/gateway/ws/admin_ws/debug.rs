//! WS admin handlers: debug surfaces.
//!
//! These exist to make platform behaviour testable without a model. The
//! gateway is the only thing that can drive the device bridge from outside an
//! agent turn, so an on-device feature like the virtual display is otherwise
//! only reachable by spending a real model call on it.

use std::sync::Arc;

use super::super::{required_str_param, WsRequest, WsResponse};
use crate::gateway::GatewayState;
use crate::tools::ask_user::AskQueue;

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
    state: &Arc<GatewayState>,
) -> WsResponse {
    let enabled = debug_enabled(std::env::var(DEBUG_TOOL_INVOKE_ENV).ok().as_deref());
    invoke_platform_tool_with(req, enabled, Some(state.tools.ask_queue.clone())).await
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
/// The method proper, with the gate decided and an optional ask queue.
///
/// `ask_queue` is what makes `android_hand_to_user` reachable here: without it
/// the tool correctly refuses ("nobody to hand this to"), which is right for a
/// background context and makes the tool untestable from this surface.
pub(crate) async fn invoke_platform_tool_with(
    req: &WsRequest,
    enabled: bool,
    ask_queue: Option<Arc<AskQueue>>,
) -> WsResponse {
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

    // One construction of the tool sets, not two: building them allocates every
    // tool and asks `is_available()` (which probes for adb), and this used to do
    // it twice per call — 1.2s per invocation on a phone, against a 70ms
    // round-trip.
    let tools: Vec<Box<dyn crate::tools::Tool>> = crate::computer::platform::all_known_toolsets()
        .into_iter()
        .filter(|set| set.is_available())
        .flat_map(|set| set.tools())
        .collect();
    let available: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();

    let Some(tool) = tools.into_iter().find(|tool| tool.name() == tool_name) else {
        return WsResponse::err(
            &req.id,
            "TOOL_NOT_FOUND",
            format!(
                "no platform tool named {tool_name:?} is available on this host; available: {}",
                available.join(", ")
            ),
        );
    };

    // A default context — these tools take no identity and no provider — plus
    // the ask queue, so a tool that needs a human can find one. The approval
    // flow is still absent: that is the caveat this method documents.
    let context = match ask_queue {
        Some(queue) => crate::tools::ToolContext::default().with_ask_queue(queue),
        None => crate::tools::ToolContext::default(),
    };
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
