//! MCP wire types (JSON-RPC 2.0), server capabilities, tool/resource/prompt
//! definitions, server-initiated notifications, lifecycle events, and health
//! tracking types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────
// Wire types (JSON-RPC 2.0)
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct McpRequest {
    pub(crate) jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<u64>,
    pub(crate) method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct McpResponse {
    pub(crate) jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) params: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<McpJsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct McpJsonRpcError {
    pub(crate) code: i32,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct McpServerInfo {
    pub(crate) name: String,
    pub(crate) version: String,
}

// ─────────────────────────────────────────────
// Capabilities
// ─────────────────────────────────────────────

/// Tool capability details returned in the MCP `initialize` response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpToolsCapability {
    /// The server supports `notifications/tools/list_changed`.
    #[serde(default, rename = "listChanged")]
    pub list_changed: bool,
}

/// Resource capability details returned in the MCP `initialize` response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpResourcesCapability {
    /// The server supports `resources/subscribe` and resource update
    /// notifications.
    #[serde(default)]
    pub subscribe: bool,
    /// The server supports `notifications/resources/list_changed`.
    #[serde(default, rename = "listChanged")]
    pub list_changed: bool,
}

/// Prompt capability details returned in the MCP `initialize` response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpPromptsCapability {
    /// The server supports `notifications/prompts/list_changed`.
    #[serde(default, rename = "listChanged")]
    pub list_changed: bool,
}

/// Server capabilities returned in the MCP `initialize` response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerCapabilities {
    /// Tool support and sub-capabilities.
    #[serde(default)]
    pub tools: Option<McpToolsCapability>,
    /// Resource support and sub-capabilities.
    #[serde(default)]
    pub resources: Option<McpResourcesCapability>,
    /// Prompt support and sub-capabilities.
    #[serde(default)]
    pub prompts: Option<McpPromptsCapability>,
    /// Logging support (e.g. `setLevel`).
    #[serde(default)]
    pub logging: Option<serde_json::Value>,
    /// Any additional capabilities.
    #[serde(default, flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl McpServerCapabilities {
    /// Returns true if the server supports tools.
    pub fn supports_tools(&self) -> bool {
        self.tools.is_some()
    }

    /// Returns true if the server supports tool list-change notifications.
    pub fn supports_tool_list_changed(&self) -> bool {
        self.tools.as_ref().map(|c| c.list_changed).unwrap_or(false)
    }

    /// Returns true if the server supports resources.
    pub fn supports_resources(&self) -> bool {
        self.resources.is_some()
    }

    /// Returns true if the server supports resource subscriptions.
    pub fn supports_resource_subscribe(&self) -> bool {
        self.resources
            .as_ref()
            .map(|c| c.subscribe)
            .unwrap_or(false)
    }

    /// Returns true if the server supports resource list-change notifications.
    pub fn supports_resource_list_changed(&self) -> bool {
        self.resources
            .as_ref()
            .map(|c| c.list_changed)
            .unwrap_or(false)
    }

    /// Returns true if the server supports prompts.
    pub fn supports_prompts(&self) -> bool {
        self.prompts.is_some()
    }

    /// Returns true if the server supports prompt list-change notifications.
    pub fn supports_prompt_list_changed(&self) -> bool {
        self.prompts
            .as_ref()
            .map(|c| c.list_changed)
            .unwrap_or(false)
    }
}

/// Full result of an MCP `initialize` handshake.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct McpInitializeResult {
    #[serde(rename = "serverInfo")]
    pub(crate) server_info: McpServerInfo,
    #[serde(default)]
    pub(crate) capabilities: McpServerCapabilities,
}

// ─────────────────────────────────────────────
// Tool definition
// ─────────────────────────────────────────────

/// MCP `ToolAnnotations` from the spec (camelCase on the wire).
///
/// Each hint is `Option<bool>` rather than `bool` so the wrapper can tell
/// "the server said nothing" apart from "the server said false" — the
/// conservative default posture only applies when the server is silent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpToolAnnotations {
    /// `readOnlyHint`: the tool only observes state and never mutates anything.
    #[serde(default, rename = "readOnlyHint")]
    pub read_only_hint: Option<bool>,
    /// `destructiveHint`: the tool may irreversibly destroy or overwrite
    /// state. Per the MCP spec it defaults to `true` when `readOnlyHint` is
    /// false, so `None` from a non-read-only tool is treated as destructive.
    #[serde(default, rename = "destructiveHint")]
    pub destructive_hint: Option<bool>,
    /// `idempotentHint`: calling the tool twice has the same effect as once.
    #[serde(default, rename = "idempotentHint")]
    pub idempotent_hint: Option<bool>,
}

/// MCP tool definition discovered from `tools/list`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub parameters: serde_json::Value,
    /// Server-declared tool annotations, absent when the server is silent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
}

// ─────────────────────────────────────────────
// Resource types (9.7)
// ─────────────────────────────────────────────

/// MCP resource descriptor returned by `resources/list`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Content returned by `resources/read`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>, // base64
}

// ─────────────────────────────────────────────
// Prompt types (2024-11-05 spec)
// ─────────────────────────────────────────────

/// Argument schema for an MCP prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// MCP prompt descriptor returned by `prompts/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPrompt {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<McpPromptArgument>>,
}

/// A single message inside a rendered prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptMessage {
    pub role: String,
    pub content: serde_json::Value,
}

/// Result of `prompts/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpGetPromptResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub messages: Vec<McpPromptMessage>,
}

// ─────────────────────────────────────────────
// Sampling types (2024-11-05 spec)
// ─────────────────────────────────────────────

/// A sampling message sent to the server via `sampling/createMessage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSamplingMessage {
    pub role: String,
    pub content: serde_json::Value,
}

/// Result of `sampling/createMessage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSamplingResult {
    pub role: String,
    pub content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

// ─────────────────────────────────────────────
// Server-initiated notifications (2024-11-05 spec)
// ─────────────────────────────────────────────

/// A server-initiated MCP notification.
#[derive(Debug, Clone)]
pub enum McpNotification {
    /// `notifications/resources/updated`
    ResourceUpdated { uri: String },
    /// `notifications/resources/list_changed`
    ResourceListChanged,
    /// `notifications/tools/list_changed`
    ToolListChanged,
    /// `notifications/progress`
    Progress {
        progress_token: serde_json::Value,
        progress: f64,
        total: Option<f64>,
    },
    /// `notifications/message`
    Message {
        level: String,
        data: serde_json::Value,
    },
    /// Any other notification, preserved as raw JSON.
    Other {
        method: String,
        params: Option<serde_json::Value>,
    },
}

// ─────────────────────────────────────────────
// Lifecycle events
// ─────────────────────────────────────────────

/// Lifecycle events emitted by `McpManager`.
#[derive(Debug, Clone)]
pub enum McpEvent {
    /// A server connected successfully.
    Connected {
        server_id: String,
        tools: usize,
        prompts: usize,
        resources: usize,
    },
    /// A server disconnected or was marked unhealthy.
    Disconnected { server_id: String, reason: String },
    /// A server recovered after an automatic reconnect.
    Recovered { server_id: String, attempt: u32 },
    /// A subscribed resource changed on the server.
    ResourceChanged { server_id: String, uri: String },
    /// OAuth authorization is required to connect.
    AuthRequired { server_id: String, auth_url: String },
    /// OAuth authorization completed successfully.
    AuthComplete { server_id: String },
    /// OAuth authorization failed.
    AuthFailed { server_id: String, reason: String },
    /// An OAuth token was silently refreshed.
    TokenRefreshed { server_id: String },
}

// ─────────────────────────────────────────────
// Health tracking
// ─────────────────────────────────────────────

/// Health status of a single MCP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpHealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

/// Mutable health record for one MCP connection.
#[derive(Debug)]
pub struct McpHealth {
    pub status: McpHealthStatus,
    pub last_heartbeat: chrono::DateTime<chrono::Utc>,
    pub consecutive_failures: u32,
}

impl McpHealth {
    pub fn new() -> Self {
        Self {
            status: McpHealthStatus::Healthy,
            last_heartbeat: chrono::Utc::now(),
            consecutive_failures: 0,
        }
    }
}

impl Default for McpHealth {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn annotations_deserialize_camel_case_hints() {
        let def: McpToolDefinition = serde_json::from_value(json!({
            "name": "send_email",
            "description": "Send an email",
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false
            }
        }))
        .expect("deserialize");
        let a = def.annotations.expect("annotations present");
        assert_eq!(a.read_only_hint, Some(false));
        assert_eq!(a.destructive_hint, Some(true));
        assert_eq!(a.idempotent_hint, Some(false));
    }

    #[test]
    fn absent_annotations_deserialize_to_none() {
        let def: McpToolDefinition = serde_json::from_value(json!({
            "name": "read_file",
            "description": "Read a file"
        }))
        .expect("deserialize");
        assert!(def.annotations.is_none());
    }
}
