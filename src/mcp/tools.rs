//! Tool wrappers — McpToolWrapper, McpPromptTool, McpConnectionTool

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::RwLock;

use crate::mcp::{
    McpClient, McpManager, McpNotification, McpPrompt, McpSamplingMessage, McpServerConfig,
    McpToolDefinition, McpTransport,
};
use crate::tools::approval::RiskLevel;
use crate::tools::sdk::ToolCapabilities;
use crate::tools::{Tool, ToolContext, ToolExecutionChunk, ToolExecutionResult};

// ─────────────────────────────────────────────
// McpToolWrapper (9.2)
// ─────────────────────────────────────────────

/// Wraps a single MCP tool so the agent can call it through `ToolRegistry`.
/// Tool names are registered as `mcp__{server_id}__{tool_name}`.
#[derive(Debug)]
pub struct McpToolWrapper {
    /// Shared client for the originating server
    client: Arc<RwLock<McpClient>>,
    /// Fully-qualified tool name (e.g. `mcp__filesystem__read_file`)
    qualified_name: String,
    /// Original MCP tool name
    tool_name: String,
    tool_description: String,
    parameters_schema: serde_json::Value,
}

impl McpToolWrapper {
    /// Create a wrapper.  `server_id` is the key from `mcp.servers.*`.
    pub fn new(client: Arc<RwLock<McpClient>>, server_id: &str, tool: &McpToolDefinition) -> Self {
        let qualified_name = format!("mcp__{}__{}", server_id, tool.name);
        Self {
            client,
            qualified_name,
            tool_name: tool.name.clone(),
            tool_description: tool.description.clone(),
            parameters_schema: tool.parameters.clone(),
        }
    }
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.tool_description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters_schema.clone()
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: RiskLevel::High,
            categories: vec!["system".to_string(), "mcp".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, context: &ToolContext) -> bool {
        !context.sandboxed() || !context.allowed_commands().is_empty()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let client = self.client.read().await;
        let result = client.call_tool(&self.tool_name, args).await?;
        Ok(ToolExecutionResult::success(format!("MCP tool result: {}", result)).with_data(result))
    }

    fn execute_stream<'a>(
        &'a self,
        args: serde_json::Value,
        _context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn tokio_stream::Stream<Item = ToolExecutionChunk> + Send + 'a>> {
        let client = self.client.clone();
        let tool_name = self.tool_name.clone();
        Box::pin(async_stream::stream! {
            let progress_token = uuid::Uuid::new_v4().to_string();
            let (result_tx, mut result_rx) =
                mpsc::unbounded_channel::<crate::Result<serde_json::Value>>();

            // Subscribe to progress notifications before issuing the call.
            let mut progress_rx = {
                let c = client.read().await;
                match c.progress_tx.as_ref() {
                    Some(tx) => tx.subscribe(),
                    None => {
                        // Progress streaming not wired; fall back to buffered execution.
                        match c.call_tool(&tool_name, args).await {
                            Ok(result) => {
                                yield ToolExecutionChunk::Output(
                                    format!("MCP tool result: {}", result),
                                );
                                yield ToolExecutionChunk::Data(result);
                            }
                            Err(e) => yield ToolExecutionChunk::Error(e.to_string()),
                        }
                        return;
                    }
                }
            };

            // Spawn the actual tool call with a progress token.
            let token = progress_token.clone();
            let call_client = client.clone();
            tokio::spawn(async move {
                let c = call_client.read().await;
                let result = c
                    .call_tool_with_progress(&tool_name, args, &token)
                    .await;
                let _ = result_tx.send(result);
            });

            loop {
                tokio::select! {
                    maybe_notification = progress_rx.recv() => {
                        match maybe_notification {
                            Ok(McpNotification::Progress {
                                progress_token: token,
                                progress,
                                total,
                            }) => {
                                if token.as_str() == Some(progress_token.as_str()) {
                                    let msg = match total {
                                        Some(t) => format!("Progress: {}/{}", progress, t),
                                        None => format!("Progress: {}", progress),
                                    };
                                    yield ToolExecutionChunk::Output(msg);
                                }
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    maybe_result = result_rx.recv() => {
                        match maybe_result {
                            Some(Ok(result)) => {
                                yield ToolExecutionChunk::Output(
                                    format!("MCP tool result: {}", result),
                                );
                                yield ToolExecutionChunk::Data(result);
                                yield ToolExecutionChunk::Done;
                                break;
                            }
                            Some(Err(e)) => {
                                yield ToolExecutionChunk::Error(e.to_string());
                                break;
                            }
                            None => break,
                        }
                    }
                }
            }
        })
    }
}

// ─────────────────────────────────────────────
// McpPromptTool
// ─────────────────────────────────────────────

/// Wraps a single MCP prompt so the agent can render it through `ToolRegistry`.
/// Prompt names are registered as `mcp__{server_id}__prompt__{prompt_name}`.
#[derive(Debug)]
pub struct McpPromptTool {
    /// Shared client for the originating server
    client: Arc<RwLock<McpClient>>,
    /// Fully-qualified tool name
    qualified_name: String,
    /// Original MCP prompt name
    prompt_name: String,
    prompt_description: String,
    /// JSON schema for the prompt arguments.
    parameters_schema: serde_json::Value,
}

impl McpPromptTool {
    /// Create a wrapper.  `server_id` is the key from `mcp.servers.*`.
    pub fn new(client: Arc<RwLock<McpClient>>, server_id: &str, prompt: &McpPrompt) -> Self {
        let qualified_name = format!("mcp__{}__prompt__{}", server_id, prompt.name);

        // Build a JSON schema from the prompt arguments.
        let properties = prompt
            .arguments
            .as_ref()
            .map(|args| {
                let mut props = serde_json::Map::new();
                let mut required = Vec::new();
                for arg in args {
                    let mut prop = serde_json::Map::new();
                    prop.insert("type".to_string(), json!("string"));
                    if let Some(desc) = &arg.description {
                        prop.insert("description".to_string(), json!(desc));
                    }
                    props.insert(arg.name.clone(), serde_json::Value::Object(prop));
                    if arg.required {
                        required.push(arg.name.clone());
                    }
                }
                let mut schema = serde_json::Map::new();
                schema.insert("type".to_string(), json!("object"));
                schema.insert("properties".to_string(), serde_json::Value::Object(props));
                if !required.is_empty() {
                    schema.insert("required".to_string(), json!(required));
                }
                serde_json::Value::Object(schema)
            })
            .unwrap_or_else(|| json!({ "type": "object" }));

        Self {
            client,
            qualified_name,
            prompt_name: prompt.name.clone(),
            prompt_description: prompt.description.clone().unwrap_or_default(),
            parameters_schema: properties,
        }
    }
}

#[async_trait]
impl Tool for McpPromptTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.prompt_description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters_schema.clone()
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: RiskLevel::Medium,
            categories: vec!["mcp".to_string(), "prompt".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let arguments = args.as_object().map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect::<HashMap<_, _>>()
        });
        let client = self.client.read().await;
        let result = client.get_prompt(&self.prompt_name, arguments).await?;
        Ok(ToolExecutionResult::success(format!("MCP prompt result: {:?}", result))
            .with_data(serde_json::to_value(result).unwrap_or_default()))
    }
}

// ─────────────────────────────────────────────
// McpConnectionTool
// ─────────────────────────────────────────────

/// Extract the required `connector_id` argument.
fn require_connector_id(args: &serde_json::Value) -> crate::Result<&str> {
    args["connector_id"].as_str().ok_or_else(|| {
        crate::error::SyscityError::Validation(
            "connector_id is required for this action".to_string(),
        )
    })
}

/// Meta-tool the agent can invoke to manage MCP connections at runtime, plus
/// connector lifecycle operations (install/enable/disable/uninstall/auth).
#[derive(Debug)]
pub struct McpConnectionTool {
    manager: Arc<McpManager>,
    connectors: Option<Arc<crate::mcp::connectors::ConnectorManager>>,
}

impl McpConnectionTool {
    pub fn new() -> Self {
        Self {
            manager: Arc::new(McpManager::default()),
            connectors: None,
        }
    }

    /// Create with a shared manager (so gateway can also share it).
    pub fn with_manager(manager: Arc<McpManager>) -> Self {
        Self { manager, connectors: None }
    }

    /// Attach the connector subsystem, enabling `connector_*` actions.
    pub fn with_connectors(
        mut self,
        connectors: Arc<crate::mcp::connectors::ConnectorManager>,
    ) -> Self {
        self.connectors = Some(connectors);
        self
    }

    fn require_connectors(&self) -> crate::Result<Arc<crate::mcp::connectors::ConnectorManager>> {
        self.connectors.clone().ok_or_else(|| {
            crate::error::SyscityError::Validation(
                "connector subsystem is not attached to this tool".to_string(),
            )
        })
    }
}

impl Default for McpConnectionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for McpConnectionTool {
    fn name(&self) -> &str {
        "mcp_connection"
    }

    fn description(&self) -> &str {
        r#"Connect to and use MCP (Model Context Protocol) servers.

Actions:
- connect: Connect to an MCP server
- disconnect: Disconnect from an MCP server
- list: List connected MCP servers
- tools: List available tools from a server
- resources: List resources available from a server
- resource_read: Read a resource by URI
- subscribe: Subscribe to resource change notifications
- unsubscribe: Unsubscribe from resource change notifications
- prompts: List available prompts from a server
- prompt_get: Render a prompt by name
- sampling: Create a sampling message through the server"#
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["connect", "disconnect", "list", "tools", "resources", "resource_read", "subscribe", "unsubscribe", "prompts", "prompt_get", "sampling", "connector_list", "connector_install", "connector_enable", "connector_disable", "connector_uninstall", "connector_auth_status", "connector_auth_login", "connector_auth_logout", "connector_check_version", "connector_updates"],
                    "description": "Action to perform. `connector_*` actions manage marketplace-style connector packages (install/enable/auth); they need the subsystem attached."
                },
                "server_id": {
                    "type": "string",
                    "description": "Unique identifier for the server connection"
                },
                "command": {
                    "type": "string",
                    "description": "Command to run the MCP server (stdio transport)"
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Arguments for the command"
                },
                "url": {
                    "type": "string",
                    "description": "URL for SSE / streamable-HTTP transport"
                },
                "transport": {
                    "type": "string",
                    "enum": ["stdio", "sse", "streamable_http"],
                    "description": "Transport type (default: stdio)"
                },
                "uri": {
                    "type": "string",
                    "description": "Resource URI (for resource_read / subscribe / unsubscribe)"
                },
                "prompt": {
                    "type": "string",
                    "description": "Prompt name (for prompt_get)"
                },
                "arguments": {
                    "type": "object",
                    "description": "Prompt arguments (for prompt_get)"
                },
                "messages": {
                    "type": "array",
                    "description": "Sampling messages (for sampling)"
                },
                "max_tokens": {
                    "type": "integer",
                    "description": "Maximum tokens for sampling (for sampling)",
                    "default": 1024
                },
                "model_hints": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Model hints for sampling (for sampling)"
                }
            },
            "required": ["action"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: RiskLevel::High,
            categories: vec!["network".to_string(), "mcp".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, context: &ToolContext) -> bool {
        !context.sandboxed() || !context.allowed_commands().is_empty()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action = args["action"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("action is required".to_string())
        })?;

        match action {
            "connect" => {
                let server_id = args["server_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "server_id is required for connect".to_string(),
                    )
                })?;

                let transport = match args["transport"].as_str().unwrap_or("stdio") {
                    "sse" => McpTransport::Sse,
                    "streamable_http" => McpTransport::StreamableHttp,
                    "in_process" => McpTransport::InProcess,
                    _ => McpTransport::Stdio,
                };

                let config = McpServerConfig {
                    transport,
                    command: args["command"].as_str().map(String::from),
                    args: args["args"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    url: args["url"].as_str().map(String::from),
                    ..Default::default()
                };

                let tools = self.manager.connect(server_id, config).await?;
                Ok(ToolExecutionResult::success(format!(
                    "Connected to MCP server '{}'. {} tools available.",
                    server_id,
                    tools.len()
                ))
                .with_data(json!({ "tools": tools.iter().map(|t| &t.name).collect::<Vec<_>>() })))
            }

            "disconnect" => {
                let server_id = args["server_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "server_id is required for disconnect".to_string(),
                    )
                })?;
                if self.manager.get_client(server_id).await.is_none() {
                    return Ok(ToolExecutionResult::error(format!(
                        "MCP server '{}' is not connected",
                        server_id
                    )));
                }
                self.manager.disconnect(server_id).await?;
                Ok(ToolExecutionResult::success(format!(
                    "Disconnected from MCP server '{}'",
                    server_id
                )))
            }

            "list" => {
                let servers = self.manager.list_servers().await;
                Ok(ToolExecutionResult::success(format!("{} MCP servers connected", servers.len()))
                    .with_data(json!({ "servers": servers })))
            }

            "tools" => {
                let server_id = args["server_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "server_id is required for tools".to_string(),
                    )
                })?;
                match self.manager.get_client(server_id).await {
                    Some(client_arc) => {
                        let client = client_arc.read().await;
                        let tools = client.get_tools().to_vec();
                        Ok(ToolExecutionResult::success(format!(
                            "{} tools from '{}'",
                            tools.len(),
                            server_id
                        ))
                        .with_data(json!({ "tools": tools })))
                    }
                    None => Ok(ToolExecutionResult::error(format!(
                        "MCP server not found: {}",
                        server_id
                    ))),
                }
            }

            "resources" => {
                let server_id = args["server_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "server_id is required for resources".to_string(),
                    )
                })?;
                match self.manager.get_client(server_id).await {
                    Some(client_arc) => {
                        let client = client_arc.read().await;
                        let resources = client.list_resources().await?;
                        Ok(ToolExecutionResult::success(format!(
                            "{} resources from '{}'",
                            resources.len(),
                            server_id
                        ))
                        .with_data(json!({ "resources": resources })))
                    }
                    None => Ok(ToolExecutionResult::error(format!(
                        "MCP server not found: {}",
                        server_id
                    ))),
                }
            }

            "resource_read" => {
                let server_id = args["server_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "server_id is required for resource_read".to_string(),
                    )
                })?;
                let uri = args["uri"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "uri is required for resource_read".to_string(),
                    )
                })?;
                match self.manager.get_client(server_id).await {
                    Some(client_arc) => {
                        let client = client_arc.read().await;
                        let contents = client.read_resource(uri).await?;
                        Ok(ToolExecutionResult::success(format!(
                            "Read {} content blocks from '{}'",
                            contents.len(),
                            uri
                        ))
                        .with_data(json!({ "contents": contents })))
                    }
                    None => Ok(ToolExecutionResult::error(format!(
                        "MCP server not found: {}",
                        server_id
                    ))),
                }
            }

            "subscribe" => {
                let server_id = args["server_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "server_id is required for subscribe".to_string(),
                    )
                })?;
                let uri = args["uri"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "uri is required for subscribe".to_string(),
                    )
                })?;
                match self.manager.get_client(server_id).await {
                    Some(client_arc) => {
                        let client = client_arc.read().await;
                        client.subscribe_resource(uri).await?;
                        Ok(ToolExecutionResult::success(format!(
                            "Subscribed to resource updates for '{}' on '{}'",
                            uri, server_id
                        )))
                    }
                    None => Ok(ToolExecutionResult::error(format!(
                        "MCP server not found: {}",
                        server_id
                    ))),
                }
            }

            "unsubscribe" => {
                let server_id = args["server_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "server_id is required for unsubscribe".to_string(),
                    )
                })?;
                let uri = args["uri"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "uri is required for unsubscribe".to_string(),
                    )
                })?;
                match self.manager.get_client(server_id).await {
                    Some(client_arc) => {
                        let client = client_arc.read().await;
                        client.unsubscribe_resource(uri).await?;
                        Ok(ToolExecutionResult::success(format!(
                            "Unsubscribed from resource updates for '{}' on '{}'",
                            uri, server_id
                        )))
                    }
                    None => Ok(ToolExecutionResult::error(format!(
                        "MCP server not found: {}",
                        server_id
                    ))),
                }
            }

            "prompts" => {
                let server_id = args["server_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "server_id is required for prompts".to_string(),
                    )
                })?;
                match self.manager.get_client(server_id).await {
                    Some(client_arc) => {
                        let client = client_arc.read().await;
                        let prompts = client.list_prompts().await?;
                        Ok(ToolExecutionResult::success(format!(
                            "{} prompts from '{}'",
                            prompts.len(),
                            server_id
                        ))
                        .with_data(json!({ "prompts": prompts })))
                    }
                    None => Ok(ToolExecutionResult::error(format!(
                        "MCP server not found: {}",
                        server_id
                    ))),
                }
            }

            "prompt_get" => {
                let server_id = args["server_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "server_id is required for prompt_get".to_string(),
                    )
                })?;
                let prompt_name = args["prompt"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "prompt is required for prompt_get".to_string(),
                    )
                })?;
                let arguments = args["arguments"].as_object().map(|obj| {
                    obj.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect::<HashMap<_, _>>()
                });
                match self.manager.get_client(server_id).await {
                    Some(client_arc) => {
                        let client = client_arc.read().await;
                        let result = client.get_prompt(prompt_name, arguments).await?;
                        Ok(ToolExecutionResult::success(format!(
                            "Rendered prompt '{}' from '{}'",
                            prompt_name, server_id
                        ))
                        .with_data(json!(result)))
                    }
                    None => Ok(ToolExecutionResult::error(format!(
                        "MCP server not found: {}",
                        server_id
                    ))),
                }
            }

            "sampling" => {
                let server_id = args["server_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "server_id is required for sampling".to_string(),
                    )
                })?;
                let messages: Vec<McpSamplingMessage> =
                    serde_json::from_value(args["messages"].clone()).unwrap_or_default();
                let max_tokens = args["max_tokens"].as_i64().unwrap_or(1024);
                let model_hints = args["model_hints"].as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                });
                match self.manager.get_client(server_id).await {
                    Some(client_arc) => {
                        let client = client_arc.read().await;
                        let result = client
                            .sampling_create_message(messages, max_tokens, model_hints)
                            .await?;
                        Ok(ToolExecutionResult::success(format!(
                            "Sampling result from '{}'",
                            server_id
                        ))
                        .with_data(json!(result)))
                    }
                    None => Ok(ToolExecutionResult::error(format!(
                        "MCP server not found: {}",
                        server_id
                    ))),
                }
            }

            // ── Connector lifecycle actions ────────────────────────────────
            "connector_list" => {
                let connectors = self.require_connectors()?;
                let list = connectors.list().await?;
                let names: Vec<String> = list
                    .iter()
                    .map(|c| format!("{} v{} ({:?})", c.id, c.version, c.state))
                    .collect();
                Ok(ToolExecutionResult::success(format!(
                    "{} connector(s): {}",
                    list.len(),
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    }
                ))
                .with_data(json!({ "connectors": list })))
            }
            "connector_install" => {
                let connectors = self.require_connectors()?;
                let source = args["source_dir"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "source_dir is required for connector_install \
                             (path to a package directory containing connector.json)"
                            .to_string(),
                    )
                })?;
                let summary = connectors
                    .install_from_dir(std::path::Path::new(source))
                    .await?;
                Ok(ToolExecutionResult::success(format!(
                    "Installed connector {} v{} ({:?})",
                    summary.id, summary.version, summary.state,
                ))
                .with_data(serde_json::to_value(&summary).unwrap_or_default()))
            }
            "connector_enable" | "connector_disable" | "connector_uninstall" => {
                let connectors = self.require_connectors()?;
                let id = require_connector_id(&args)?;
                let verb = action.trim_start_matches("connector_");
                let result: serde_json::Value = match verb {
                    "enable" => serde_json::to_value(connectors.enable(id).await?)?,
                    "disable" => serde_json::to_value(connectors.disable(id).await?)?,
                    _ => {
                        connectors.uninstall(id).await?;
                        json!({ "id": id, "state": "uninstalled" })
                    }
                };
                let state = result
                    .get("state")
                    .and_then(|s| s.as_str())
                    .unwrap_or(verb)
                    .to_string();
                Ok(ToolExecutionResult::success(format!("Connector {id}: {state}"))
                    .with_data(result))
            }
            "connector_auth_status" | "connector_auth_login" | "connector_auth_logout" => {
                let connectors = self.require_connectors()?;
                let id = require_connector_id(&args)?;
                let which = action.trim_start_matches("connector_auth_");
                match which {
                    "status" => {
                        let status = connectors.auth_status(id).await?;
                        let text = status.unwrap_or_else(|| {
                            "no auth.status command declared or not authenticated".to_string()
                        });
                        Ok(ToolExecutionResult::success(format!("Connector {id} auth: {text}"))
                            .with_data(json!({ "id": id, "authenticated_text": text })))
                    }
                    "login" => {
                        let out = connectors.auth_login(id).await?;
                        Ok(ToolExecutionResult::success(format!(
                            "Connector {id} auth login exited with code {}",
                            out.code
                        ))
                        .with_data(json!({
                            "code": out.code,
                            "stdout": out.stdout,
                            "stderr": out.stderr,
                        })))
                    }
                    _ => {
                        connectors.auth_logout(id).await?;
                        Ok(ToolExecutionResult::success(format!("Connector {id} logged out")))
                    }
                }
            }
            "connector_check_version" => {
                let connectors = self.require_connectors()?;
                let id = require_connector_id(&args)?;
                let outcome = connectors.check_version(id).await?;
                Ok(ToolExecutionResult::success(format!(
                    "Connector {id} version check: {outcome:?}"
                ))
                .with_data(json!({ "outcome": format!("{outcome:?}") })))
            }
            "connector_updates" => {
                let connectors = self.require_connectors()?;
                let auto_only = args["auto_only"].as_bool().unwrap_or(false);
                if args["apply"].as_bool().unwrap_or(false) {
                    let applied = connectors.apply_updates(auto_only).await?;
                    Ok(ToolExecutionResult::success(format!("Applied {} update(s)", applied.len()))
                        .with_data(json!({ "applied": applied })))
                } else {
                    let pending = connectors.check_updates().await?;
                    Ok(ToolExecutionResult::success(format!("{} pending update(s)", pending.len()))
                        .with_data(json!({
                            "pending": pending.iter().map(|u| json!({
                                "id": u.id,
                                "current_version": u.current_version,
                                "latest_version": u.latest_version,
                                "auto_update": u.entry.auto_update,
                            })).collect::<Vec<_>>(),
                        })))
                }
            }
            _ => Err(crate::error::SyscityError::Validation(format!("Unknown action: {}", action))),
        }
    }
}
