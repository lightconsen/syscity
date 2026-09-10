//! WebSocket Protocol for Syscity Gateway
//!
//! Implements the WebSocket-native RPC protocol (docs/protocol.md).
//!
//! Protocol flow:
//!   1. Client opens WebSocket to /ws
//!   2. Server validates auth (session cookie or shared token) - rejects with
//!      401 if missing
//!   3. Server accepts WebSocket connection
//!   4. Client sends `connect` req as first frame
//!   5. Server validates auth + protocol version, replies `hello-ok`
//!   6. Client sends method calls (e.g. `chat.send`), server replies `res`
//!   7. Server pushes events (`chat.delta`, `tool.calling`, etc.)
//!      asynchronously
//!
//! Layout:
//!   core.rs      connection lifecycle, auth middleware, dispatcher
//!   handshake.rs connect / hello / device-auth / ping / session titles
//!   chat.rs      chat.send / chat.history / chat.abort
//!   sessions.rs  session CRUD + subscribe / unsubscribe
//!   agents.rs    agent list/get/registry, health, system presence
//!   acp.rs       ACP (sub-agent) control-plane
//!   config_ws.rs config.get / config.set
//!   models.rs    model management
//!   mcp_ws.rs    MCP server management
//!   tasks.rs     cron + task scheduler
//!   skills_ws.rs skills.list / skills.install
//!   kb_ws.rs     kb.* (local Knowledge Base collections/docs/ingest/delete)
//!   logs.rs      logs.subscribe / logs.unsubscribe
//!   workspace.rs workspace.list / workspace.read (agent workspace browser)

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    middleware::Next,
    response::IntoResponse,
};
use base64::Engine;
use chrono::DateTime;
use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn, Instrument};
use uuid::Uuid;

use crate::agent::session_store::AppendMessageParams;
// The core tracing context is aliased: the name `RequestContext` in this module
// (and, via the glob `use super::*`, in its submodules) refers to the security
// request identity context.
use crate::core::context::RequestContext as TracingContext;
use crate::gateway::handlers::config::persist_config_atomic;
use crate::gateway::protocol::*;
use crate::gateway::{GatewayEvent, GatewayState};
use crate::providers::Message as ProviderMessage;
use crate::security::request_context::{AuthSource, RequestContext};
use crate::security::UserId;

/// Query parameters for WebSocket upgrade
#[derive(Debug, Deserialize)]
pub struct WsConnectQuery {
    /// Optional: pre-subscribe to a session on connect
    pub session_id: Option<String>,
    /// Optional: client identifier hint
    pub client: Option<String>,
    /// Optional: authentication token (alternative to cookie/Bearer)
    pub token: Option<String>,
}

#[derive(Debug, Clone)]
enum WsCommand {
    SendResponse(String),
    SendEvent(String),
    Subscribe(Vec<String>),
    Unsubscribe(Vec<String>),
}

/// Pre-validated WebSocket authentication result, injected via Extension
/// by the `ws_auth_middleware` BEFORE the WebSocket upgrade happens.
#[derive(Debug, Clone)]
pub struct WsAuthResult {
    pub user_id: UserId,
    pub scopes: Vec<String>,
}

#[allow(clippy::result_large_err)]
fn parse_params<T: serde::de::DeserializeOwned>(req: &WsRequest) -> Result<T, WsResponse> {
    match &req.params {
        Some(p) => match serde_json::from_value::<T>(p.clone()) {
            Ok(v) => Ok(v),
            Err(e) => Err(error_invalid_request(&req.id, format!("Invalid params: {}", e))),
        },
        None => Err(error_invalid_request(&req.id, "Missing params")),
    }
}

/// Persist GatewayConfig to config.toml.
async fn persist_config(state: &Arc<GatewayState>) -> Result<(), WsResponse> {
    if let Some(config_path) = state.config_path.clone() {
        let config_guard = state.config.read().await;
        match toml::to_string_pretty(&*config_guard) {
            Ok(toml_str) => {
                let tmp_path = config_path.with_extension("toml.tmp");
                if let Err(e) = tokio::fs::write(&tmp_path, toml_str).await {
                    return Err(WsResponse::err(
                        "persist",
                        "PERSIST_FAILED",
                        format!("Failed to write temporary config: {}", e),
                    ));
                }
                if let Err(e) = tokio::fs::rename(&tmp_path, &config_path).await {
                    return Err(WsResponse::err(
                        "persist",
                        "PERSIST_FAILED",
                        format!("Failed to atomically replace config: {}", e),
                    ));
                }
            }
            Err(e) => {
                return Err(WsResponse::err(
                    "persist",
                    "PERSIST_FAILED",
                    format!("TOML serialization failed: {}", e),
                ));
            }
        }
    }
    Ok(())
}

mod acp;
mod admin_ws;
mod agents;
mod ask;
mod chat;
mod config_ws;
pub(crate) mod connectors_ws;
mod core;
mod device_ws;
mod eval_ws;
mod feedback;
mod handshake;
mod kb_ws;
mod logs;
mod mcp_ws;
mod models;
mod sessions;
mod skills_ws;
mod tasks;
mod workspace;

pub(crate) use config_ws::push_default_agent_update;
pub use core::{ws_auth_middleware, ws_handler};

#[cfg(test)]
mod tests {
    use super::device_ws;
    use super::mcp_ws::{
        handle_mcp_add, handle_mcp_connect, handle_mcp_disconnect, handle_mcp_list,
        handle_mcp_remove,
    };
    use super::models::{
        handle_models_fetch_remote, models_endpoint_url, parse_data_models, parse_gemini_models,
    };
    use super::*;

    fn make_req(id: &str, method: &str, params: serde_json::Value) -> WsRequest {
        WsRequest {
            frame_type: "req".to_string(),
            id: id.to_string(),
            method: method.to_string(),
            params: Some(params),
        }
    }

    #[test]
    fn test_models_endpoint_url() {
        use crate::providers::Protocol;
        assert_eq!(
            models_endpoint_url(Protocol::OpenAi, "https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_endpoint_url(Protocol::Anthropic, "https://api.anthropic.com/"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            models_endpoint_url(
                Protocol::Gemini,
                "https://generativelanguage.googleapis.com/v1beta"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
    }

    #[test]
    fn test_parse_data_models() {
        let body = serde_json::json!({
            "object": "list",
            "data": [
                { "id": "gpt-4o", "object": "model" },
                { "id": "gpt-4o-mini", "object": "model" },
                { "object": "model" }
            ]
        });
        assert_eq!(parse_data_models(&body), vec!["gpt-4o", "gpt-4o-mini"]);
        assert!(parse_data_models(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn test_parse_gemini_models() {
        let body = serde_json::json!({
            "models": [
                { "name": "models/gemini-2.0-flash" },
                { "name": "models/gemini-1.5-pro" }
            ]
        });
        assert_eq!(parse_gemini_models(&body), vec!["gemini-2.0-flash", "gemini-1.5-pro"]);
        assert!(parse_gemini_models(&serde_json::json!({})).is_empty());
    }

    #[tokio::test]
    async fn test_fetch_remote_unknown_provider_requires_protocol() {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        let req = make_req(
            "r1",
            "models.fetch_remote",
            serde_json::json!({ "provider": "no-such-provider" }),
        );
        let res = handle_models_fetch_remote(&req, &state).await;
        assert!(!res.ok);
    }

    #[tokio::test]
    async fn test_fetch_remote_invalid_base_url() {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        let req = make_req(
            "r1",
            "models.fetch_remote",
            serde_json::json!({ "provider": "openai", "base_url": "ftp://example.com" }),
        );
        let res = handle_models_fetch_remote(&req, &state).await;
        assert!(!res.ok);
    }

    #[tokio::test]
    async fn test_fetch_remote_unreachable_falls_back_to_static() {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        let req = make_req(
            "r1",
            "models.fetch_remote",
            serde_json::json!({
                "provider": "openai",
                // Port 1 is reserved and unreachable, so the request fails fast.
                "base_url": "http://127.0.0.1:1"
            }),
        );
        let res = handle_models_fetch_remote(&req, &state).await;
        assert!(res.ok);
        let payload = res.payload.unwrap();
        assert_eq!(payload.get("source").unwrap(), "static");
        let models = payload.get("models").unwrap().as_array().unwrap();
        assert!(models.iter().any(|m| m == "gpt-4o"));
    }

    #[tokio::test]
    async fn test_handle_mcp_list_empty() {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        let req = make_req("r1", "mcp.list", serde_json::json!({}));
        let res = handle_mcp_list(&req, &state).await;
        assert!(res.ok);
        let payload = res.payload.unwrap();
        let servers = payload.get("servers").unwrap().as_array().unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn test_handle_mcp_add_and_list() {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        let req = make_req(
            "r1",
            "mcp.add",
            serde_json::json!({
                "id": "test-server",
                "transport": "stdio",
                "command": "echo",
                "args": ["hello"],
                "auto_connect": false,
            }),
        );
        let res = handle_mcp_add(&req, &state).await;
        assert!(res.ok, "add failed: {:?}", res.error);
        assert_eq!(res.payload.unwrap().get("status").unwrap(), "added");

        let req = make_req("r2", "mcp.list", serde_json::json!({}));
        let res = handle_mcp_list(&req, &state).await;
        assert!(res.ok);
        let servers = res
            .payload
            .unwrap()
            .get("servers")
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].get("id").unwrap(), "test-server");
        assert_eq!(servers[0].get("transport").unwrap(), "stdio");
        assert_eq!(servers[0].get("connected").unwrap(), false);
    }

    #[tokio::test]
    async fn test_handle_mcp_remove() {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        // Add first
        let req = make_req(
            "r1",
            "mcp.add",
            serde_json::json!({
                "id": "to-remove",
                "transport": "stdio",
                "command": "echo",
                "args": [],
                "auto_connect": false,
            }),
        );
        let res = handle_mcp_add(&req, &state).await;
        assert!(res.ok);

        // Remove
        let req = make_req("r2", "mcp.remove", serde_json::json!({ "id": "to-remove" }));
        let res = handle_mcp_remove(&req, &state).await;
        assert!(res.ok);
        assert_eq!(res.payload.unwrap().get("status").unwrap(), "removed");

        // List should be empty
        let req = make_req("r3", "mcp.list", serde_json::json!({}));
        let res = handle_mcp_list(&req, &state).await;
        let payload = res.payload.unwrap();
        let servers = payload.get("servers").unwrap().as_array().unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn test_handle_mcp_connect_not_found() {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        let req = make_req("r1", "mcp.connect", serde_json::json!({ "id": "missing" }));
        let res = handle_mcp_connect(&req, &state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "MCP_NOT_FOUND");
    }

    #[tokio::test]
    async fn test_handle_mcp_disconnect_not_connected() {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        let req = make_req("r1", "mcp.disconnect", serde_json::json!({ "id": "nobody" }));
        let res = handle_mcp_disconnect(&req, &state).await;
        assert!(res.ok);
        assert_eq!(res.payload.unwrap().get("status").unwrap(), "disconnected");
    }

    #[test]
    fn test_ws_request_deserialize() {
        let json = r#"{"type":"req","id":"r1","method":"chat.send","params":{"message":"hello"}}"#;
        let req: WsRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "r1");
        assert_eq!(req.method, "chat.send");
        assert!(req.params.is_some());
    }

    #[test]
    fn test_ws_response_serialize() {
        let res = WsResponse::ok("r1", serde_json::json!({"status": "ok"}));
        let json = serde_json::to_string(&res).unwrap();
        assert!(json.contains("\"type\":\"res\""));
        assert!(json.contains("\"ok\":true"));
    }

    #[test]
    fn test_ws_event_serialize() {
        let evt = WsEvent::new("chat.delta", serde_json::json!({"content": "hi"}), 1);
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"event\""));
        assert!(json.contains("\"event\":\"chat.delta\""));
    }

    #[test]
    fn test_connect_params_deserialize() {
        let json = r#"{"protocol_version":1,"client":{"id":"web","version":"1.0"},"auth":{"token":"abc"},"scopes":["chat","read"]}"#;
        let params: ConnectParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.protocol_version, 1);
        assert_eq!(params.client.as_ref().unwrap().id, "web");
        assert_eq!(params.scopes, vec!["chat", "read"]);
    }

    // ── Device handlers ──────────────────────────────────────────────────────

    /// Build a test state whose device bridge returns the given canned
    /// response for every command, then returns it alongside the bridge.
    async fn device_test_state(
        response: serde_json::Value,
    ) -> (Arc<GatewayState>, Arc<crate::device::tests::MockDeviceBridge>) {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        let bridge: Arc<crate::device::tests::MockDeviceBridge> =
            Arc::new(crate::device::tests::MockDeviceBridge::new(response));
        *state.device.bridge.write().await = Some(bridge.clone());
        (state, bridge)
    }

    #[tokio::test]
    async fn test_device_handlers_unsupported_without_bridge() {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        // Every handler returns UNSUPPORTED_PLATFORM when the bridge is None.
        let req = make_req("r1", "device.capabilities", serde_json::json!({}));
        let res = device_ws::handle_device_capabilities(&req, &state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "UNSUPPORTED_PLATFORM");

        let req = make_req(
            "r1",
            "device.permission.request",
            serde_json::json!({ "permission": "camera" }),
        );
        let res = device_ws::handle_device_permission_request(&req, &state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "UNSUPPORTED_PLATFORM");

        let req =
            make_req("r1", "device.adb.pair", serde_json::json!({ "port": 5555, "code": "1" }));
        let res = device_ws::handle_device_adb_pair(&req, &state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "UNSUPPORTED_PLATFORM");

        let req = make_req("r1", "device.adb.status", serde_json::json!({}));
        let res = device_ws::handle_device_adb_status(&req, &state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "UNSUPPORTED_PLATFORM");
    }

    #[tokio::test]
    async fn test_device_capabilities_reports_grants() {
        let (state, bridge) = device_test_state(serde_json::json!({ "granted": true })).await;
        let req = make_req("r1", "device.capabilities", serde_json::json!({}));
        let res = device_ws::handle_device_capabilities(&req, &state).await;
        assert!(res.ok);
        let caps = res.payload.as_ref().unwrap()["capabilities"]
            .as_array()
            .unwrap();
        // Camera/location/notifications query permission.status; the mock
        // returns granted:true. Permission-free caps report granted:true.
        assert_eq!(caps.len(), 6);
        let camera = caps.iter().find(|c| c["id"] == "camera").unwrap();
        assert_eq!(camera["granted"], true);
        assert_eq!(bridge.calls().len(), 3); // camera, location, notifications
    }

    #[tokio::test]
    async fn test_device_capabilities_missing_permissions_reported_denied() {
        let (state, _bridge) = device_test_state(serde_json::json!({ "granted": false })).await;
        let req = make_req("r1", "device.capabilities", serde_json::json!({}));
        let res = device_ws::handle_device_capabilities(&req, &state).await;
        assert!(res.ok);
        let caps = res.payload.as_ref().unwrap()["capabilities"]
            .as_array()
            .unwrap();
        let location = caps.iter().find(|c| c["id"] == "location").unwrap();
        assert_eq!(location["granted"], false);
        // Permission-free capabilities stay granted.
        let haptics = caps.iter().find(|c| c["id"] == "haptics").unwrap();
        assert_eq!(haptics["granted"], true);
    }

    #[tokio::test]
    async fn test_device_permission_request_forwards() {
        let (state, bridge) =
            device_test_state(serde_json::json!({ "permission": "camera", "granted": true })).await;
        let req = make_req(
            "r1",
            "device.permission.request",
            serde_json::json!({ "permission": "camera" }),
        );
        let res = device_ws::handle_device_permission_request(&req, &state).await;
        assert!(res.ok);
        assert_eq!(res.payload.as_ref().unwrap()["granted"], true);
        assert_eq!(bridge.calls().len(), 1);
        assert_eq!(bridge.calls()[0].0, crate::device::CMD_REQUEST_PERMISSION);
        assert_eq!(bridge.calls()[0].1["permission"], "camera");
    }

    #[tokio::test]
    async fn test_device_adb_pair_payload_is_validated() {
        // `device.adb.pair` runs the local adb client Rust-side (not through
        // the bridge), so on desktop it is gated to UNSUPPORTED_PLATFORM when
        // the bridge is absent — but only after the payload parses. A missing
        // port/code is a request error, not an unsupported-platform one.
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );
        let req = make_req("r1", "device.adb.pair", serde_json::json!({}));
        let res = device_ws::handle_device_adb_pair(&req, &state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "INVALID_REQUEST");

        let req = make_req(
            "r1",
            "device.adb.pair",
            serde_json::json!({ "port": 43455, "code": "123456", "connect_port": 5555 }),
        );
        let res = device_ws::handle_device_adb_pair(&req, &state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "UNSUPPORTED_PLATFORM");
    }
}
