//! MCP (Model Context Protocol) Integration
//!
//! This module implements a client for the Model Context Protocol,
//! allowing Syscity to connect to MCP servers and use their tools.
//!
//! Supported transports:
//! - `stdio` – spawn a subprocess and communicate over stdin/stdout
//! - `sse` – connect to an HTTP server via Server-Sent Events
//! - `streamable_http` – POST requests with SSE response bodies
//!
//! The [`connectors`] submodule layers marketplace-style packaging on top:
//! versioned packages with declarative lifecycle hooks, bundled skills,
//! a persistent state machine, and remote catalog sync. Unlike the rest of
//! this module it owns durable local state (`~/.syscity/connectors/`).
// INVARIANTS-NONE: server registry rebuilds wholesale on reconnect; durable
// state lives only under connectors/ (states.json), owned by that submodule.

mod client;
mod config;
mod manager;
mod oauth;
mod tools;
mod types;

pub mod connectors;

/// The default MCP presets embedded in the binary.
pub const DEFAULT_PRESETS_TOML: &str = include_str!("presets.toml");

// ─────────────────────────────────────────────
// Re-exports — preserve the `crate::mcp::*` API surface
// ─────────────────────────────────────────────

pub use client::McpClient;
pub use config::{McpConfig, McpServerConfig, McpSettings, McpTransport};
pub use connectors::{catalog as connector_catalog, ConnectorManager, ConnectorSummary};
pub use manager::{McpConnectionMeta, McpManager};
pub use oauth::{
    mcp_tokens_dir, migrate_legacy_mcp_tokens, token_path_for, OAuthManager, OAuthTokens,
};
pub(crate) use oauth::{OAuthCommand, OAuthManagerActor};
pub use tools::{McpConnectionTool, McpPromptTool, McpToolWrapper};
pub use types::{
    McpEvent, McpGetPromptResult, McpHealth, McpHealthStatus, McpNotification, McpPrompt,
    McpPromptArgument, McpPromptMessage, McpPromptsCapability, McpResource, McpResourceContent,
    McpResourcesCapability, McpSamplingMessage, McpSamplingResult, McpServerCapabilities,
    McpToolAnnotations, McpToolDefinition, McpToolsCapability,
};
pub(crate) use types::{McpInitializeResult, McpRequest, McpResponse, McpServerInfo};

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    use crate::mcp::client::McpInProcessHandler;
    use crate::mcp::types::McpJsonRpcError;
    use crate::tools::Tool;

    #[test]
    fn test_mcp_client_default() {
        let client = McpClient::default();
        assert!(!client.is_connected());
        assert!(client.get_tools().is_empty());
    }

    #[test]
    fn test_mcp_server_config_defaults() {
        let config = McpServerConfig::default();
        assert_eq!(config.timeout_secs, 30);
        assert!(config.auto_connect);
        assert!(config.auto_reconnect);
        assert_eq!(config.health_check_interval_secs, 30);
        assert_eq!(config.max_reconnect_attempts, 5);
        assert!(config.command.is_none());
    }

    #[test]
    fn test_env_resolution() {
        // Set a temp env var
        std::env::set_var("MCP_TEST_VAR", "hello");
        let mut env = HashMap::new();
        env.insert("KEY".to_string(), "$MCP_TEST_VAR".to_string());
        env.insert("LITERAL".to_string(), "world".to_string());

        let resolved = McpClient::resolve_env(&env);
        assert_eq!(resolved["KEY"], "hello");
        assert_eq!(resolved["LITERAL"], "world");
        std::env::remove_var("MCP_TEST_VAR");
    }

    #[test]
    fn test_merged_env_does_not_expand_literal_tokens() {
        // A literal stored token that begins with `$` must NOT be run through
        // env-var expansion (regression for the env-store feature).
        std::env::set_var("HOME", "/fake/home");
        let mut env = HashMap::new();
        env.insert("REF".to_string(), "$HOME".to_string());
        let mut resolved_env = HashMap::new();
        resolved_env.insert("TOKEN".to_string(), "$HOME_literal".to_string());

        let config = McpServerConfig {
            env,
            resolved_env,
            ..Default::default()
        };
        let merged = McpClient::merged_env(&config);
        assert_eq!(merged["REF"], "/fake/home");
        assert_eq!(merged["TOKEN"], "$HOME_literal");
        std::env::remove_var("HOME");
    }

    #[test]
    fn test_tool_wrapper_name() {
        let client = Arc::new(RwLock::new(McpClient::new()));
        let def = McpToolDefinition {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({}),
            annotations: None,
        };
        let wrapper = McpToolWrapper::new(client, "filesystem", &def);
        assert_eq!(wrapper.name(), "mcp__filesystem__read_file");
    }

    #[test]
    fn test_server_capabilities_deserialization() {
        let caps: McpServerCapabilities = serde_json::from_value(json!({
            "tools": { "listChanged": true },
            "resources": { "subscribe": true, "listChanged": false },
            "prompts": { "listChanged": true }
        }))
        .unwrap();
        assert!(caps.supports_tools());
        assert!(caps.supports_tool_list_changed());
        assert!(caps.supports_resources());
        assert!(caps.supports_resource_subscribe());
        assert!(!caps.supports_resource_list_changed());
        assert!(caps.supports_prompts());
        assert!(caps.supports_prompt_list_changed());
    }

    #[test]
    fn test_initialize_result_deserialization() {
        let result: McpInitializeResult = serde_json::from_value(json!({
            "serverInfo": { "name": "test-server", "version": "1.0.0" },
            "capabilities": { "tools": {} }
        }))
        .unwrap();
        assert_eq!(result.server_info.name, "test-server");
        assert!(result.capabilities.supports_tools());
    }

    /// Minimal in-process MCP server implementing initialize / tools/list /
    /// tools/call over the in-process channel transport.
    #[derive(Debug)]
    struct FakeInProcessMcpServer;

    #[async_trait]
    impl McpInProcessHandler for FakeInProcessMcpServer {
        async fn handle(&self, request: McpRequest) -> McpResponse {
            let id = request.id;
            match request.method.as_str() {
                "initialize" => McpResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    method: None,
                    params: None,
                    result: Some(json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "fake", "version": "1.0.0" },
                    })),
                    error: None,
                },
                "tools/list" => McpResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    method: None,
                    params: None,
                    result: Some(json!({
                        "tools": [{
                            "name": "echo",
                            "description": "Echo text",
                            "inputSchema": { "type": "object" },
                            "annotations": { "readOnlyHint": true }
                        }]
                    })),
                    error: None,
                },
                "tools/call" => {
                    let name = request
                        .params
                        .as_ref()
                        .and_then(|p| p.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let args = request
                        .params
                        .as_ref()
                        .and_then(|p| p.get("arguments"))
                        .cloned()
                        .unwrap_or(json!({}));
                    McpResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        method: None,
                        params: None,
                        result: Some(json!({
                            "content": [{ "type": "text", "text": format!("{}: {}", name, args) }],
                            "isError": false
                        })),
                        error: None,
                    }
                }
                _ => McpResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    method: None,
                    params: None,
                    result: None,
                    error: Some(McpJsonRpcError {
                        code: -32601,
                        message: format!("Method not found: {}", request.method),
                        data: None,
                    }),
                },
            }
        }
    }

    #[tokio::test]
    async fn test_in_process_transport_connect_and_call() {
        let mut client = McpClient::new();
        client.set_in_process_handler(Arc::new(FakeInProcessMcpServer));
        let config = McpServerConfig {
            transport: McpTransport::InProcess,
            timeout_secs: 5,
            ..Default::default()
        };

        client.connect(config).await.unwrap();
        assert!(client.is_connected());

        let tools = client.get_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        // The fake server declares readOnlyHint on its echo tool; the
        // annotation must survive the in-process round trip.
        let ann = tools[0].annotations.as_ref().expect("annotations present");
        assert_eq!(ann.read_only_hint, Some(true));

        let result = client
            .call_tool("echo", json!({ "text": "hi" }))
            .await
            .unwrap();
        let text = result
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .unwrap();
        assert!(text.contains("hi"));
    }

    #[tokio::test]
    async fn test_in_process_transport_requires_handler() {
        let mut client = McpClient::new();
        let config = McpServerConfig {
            transport: McpTransport::InProcess,
            ..Default::default()
        };
        let err = client.connect(config).await.unwrap_err();
        assert!(err.to_string().contains("registered handler"));
    }

    #[test]
    fn test_mcp_settings_deserialization() {
        let toml_str = r#"
[servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem"]
timeout_secs = 60
auto_connect = true
"#;
        let settings: McpSettings = toml::from_str(toml_str).unwrap();
        assert!(settings.servers.contains_key("filesystem"));
        let fs = &settings.servers["filesystem"];
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.timeout_secs, 60);
    }
}
