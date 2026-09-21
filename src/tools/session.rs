//! Session management tools
//!
//! Tools for listing, querying, sending to, yielding, and inspecting sessions.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::tools::sdk::ToolCapabilities;

// ── sessions_list ────────────────────────────────────────────────────────────

/// List all sessions from persistent storage.
pub struct SessionsListTool {
    store: Option<Arc<crate::agent::session_store::SessionStore>>,
}

impl SessionsListTool {
    pub fn new(store: Option<Arc<crate::agent::session_store::SessionStore>>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for SessionsListTool {
    fn name(&self) -> &str {
        "sessions_list"
    }

    fn description(&self) -> &str {
        "List all sessions from persistent storage with metadata."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Medium,
            categories: vec!["system".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();

        let store = match &self.store {
            Some(s) => s,
            None => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some("Persistent session storage is not available".to_string()),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        match store.find_sessions(None, None, None, false).await {
            Ok(sessions) => {
                let list: Vec<_> = sessions
                    .iter()
                    .map(|s| {
                        let mut obj = serde_json::json!({
                            "session_id": s.session_id,
                            "agent_id": s.agent_id,
                            "channel": s.channel,
                            "channel_id": s.channel_id,
                            "created_at": s.created_at.to_rfc3339(),
                            "last_activity": s.last_activity.to_rfc3339(),
                            "is_active": s.is_active,
                            "message_count": s.message_count,
                        });
                        if let Some(name) = &s.name {
                            obj["name"] = serde_json::Value::String(name.clone());
                        }
                        if let Some(bound) = &s.bound_agent_id {
                            obj["bound_agent_id"] = serde_json::Value::String(bound.clone());
                        }
                        obj
                    })
                    .collect();

                Ok(ToolExecutionResult {
                    success: true,
                    output: format!("Found {} session(s)", list.len()),
                    error: None,
                    data: Some(serde_json::json!({ "sessions": list })),
                    execution_time: start.elapsed(),
                })
            }
            Err(e) => Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to list sessions: {}", e)),
                data: None,
                execution_time: start.elapsed(),
            }),
        }
    }
}

// ── sessions_history ─────────────────────────────────────────────────────────

/// Get chat message history for a session from persistent storage.
pub struct SessionsHistoryTool {
    store: Option<Arc<crate::agent::session_store::SessionStore>>,
}

impl SessionsHistoryTool {
    pub fn new(store: Option<Arc<crate::agent::session_store::SessionStore>>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
struct SessionsHistoryArgs {
    session_id: String,
    #[serde(default = "default_history_limit")]
    limit: i64,
}

fn default_history_limit() -> i64 {
    50
}

#[async_trait]
impl Tool for SessionsHistoryTool {
    fn name(&self) -> &str {
        "sessions_history"
    }

    fn description(&self) -> &str {
        "Get chat message history for a session. Returns user and assistant messages ordered \
         oldest first."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session ID"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of messages to return",
                    "default": 50
                }
            },
            "required": ["session_id"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Medium,
            categories: vec!["system".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let args: SessionsHistoryArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid arguments: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        let store = match &self.store {
            Some(s) => s,
            None => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some("Persistent session storage is not available".to_string()),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        match store.get_messages(&args.session_id, args.limit, None).await {
            Ok(messages) => {
                let history: Vec<_> = messages
                    .iter()
                    .map(
                        |(
                            id,
                            role,
                            content,
                            reasoning,
                            tool_calls,
                            created_at,
                            _transcript_id,
                            _run_id,
                            _turn_id,
                        )| {
                            let mut msg = serde_json::json!({
                                "id": id,
                                "role": role,
                                "content": content,
                                "created_at": created_at.to_rfc3339(),
                            });
                            if let Some(r) = reasoning {
                                msg["reasoning_content"] = serde_json::Value::String(r.clone());
                            }
                            if let Some(t) = tool_calls {
                                msg["tool_calls_json"] = serde_json::Value::String(t.clone());
                            }
                            msg
                        },
                    )
                    .collect();

                Ok(ToolExecutionResult {
                    success: true,
                    output: format!("Session {} has {} message(s)", args.session_id, history.len()),
                    error: None,
                    data: Some(serde_json::json!({
                        "session_id": args.session_id,
                        "messages": history,
                    })),
                    execution_time: start.elapsed(),
                })
            }
            Err(e) => Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to load session history: {}", e)),
                data: None,
                execution_time: start.elapsed(),
            }),
        }
    }
}

// ── sessions_send ────────────────────────────────────────────────────────────

/// Send a message to a subagent in a session.
pub struct SessionsSendTool {
    acp: Arc<crate::acp::AcpControlPlane>,
}

impl SessionsSendTool {
    pub fn new(acp: Arc<crate::acp::AcpControlPlane>) -> Self {
        Self { acp }
    }
}

#[derive(Debug, Deserialize)]
struct SessionsSendArgs {
    session_id: String,
    subagent_id: String,
    message: String,
}

#[async_trait]
impl Tool for SessionsSendTool {
    fn name(&self) -> &str {
        "sessions_send"
    }

    fn description(&self) -> &str {
        "Send a message to a specific subagent within an ACP session."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "ACP session ID"
                },
                "subagent_id": {
                    "type": "string",
                    "description": "Target subagent ID"
                },
                "message": {
                    "type": "string",
                    "description": "Message to send"
                }
            },
            "required": ["session_id", "subagent_id", "message"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: crate::tools::approval::RiskLevel::High,
            categories: vec!["communication".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let args: SessionsSendArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid arguments: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        let msg = crate::channels::IncomingMessage::new(
            context.user_id.clone(),
            context.conversation_id.clone(),
            args.message,
        );

        match self.acp.send_message(&args.subagent_id, msg).await {
            Ok(response) => Ok(ToolExecutionResult {
                success: true,
                output: response.clone(),
                error: None,
                data: Some(serde_json::json!({
                    "subagent_id": args.subagent_id,
                    "session_id": args.session_id,
                    "response": response,
                })),
                execution_time: start.elapsed(),
            }),
            Err(e) => Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to send message: {}", e)),
                data: None,
                execution_time: start.elapsed(),
            }),
        }
    }
}

// ── sessions_yield ───────────────────────────────────────────────────────────

/// Yield (cancel/pause) a subagent in a session.
pub struct SessionsYieldTool {
    acp: Arc<crate::acp::AcpControlPlane>,
}

impl SessionsYieldTool {
    pub fn new(acp: Arc<crate::acp::AcpControlPlane>) -> Self {
        Self { acp }
    }
}

#[derive(Debug, Deserialize)]
struct SessionsYieldArgs {
    subagent_id: String,
}

#[async_trait]
impl Tool for SessionsYieldTool {
    fn name(&self) -> &str {
        "sessions_yield"
    }

    fn description(&self) -> &str {
        "Yield (cancel/pause) an active subagent. This sends a cancel signal to stop the current \
         operation without terminating the subagent."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "subagent_id": {
                    "type": "string",
                    "description": "Subagent ID to yield"
                }
            },
            "required": ["subagent_id"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Medium,
            categories: vec!["system".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let args: SessionsYieldArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid arguments: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        tracing::info!("Yielding subagent {}", args.subagent_id);

        match self.acp.shutdown_subagent(&args.subagent_id).await {
            Ok(true) => Ok(ToolExecutionResult {
                success: true,
                output: format!("Subagent {} yielded", args.subagent_id),
                error: None,
                data: Some(serde_json::json!({
                    "subagent_id": args.subagent_id,
                    "action": "yield",
                })),
                execution_time: start.elapsed(),
            }),
            Ok(false) => Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Subagent {} not found", args.subagent_id)),
                data: None,
                execution_time: start.elapsed(),
            }),
            Err(e) => Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to yield subagent: {}", e)),
                data: None,
                execution_time: start.elapsed(),
            }),
        }
    }
}

// ── session_status ───────────────────────────────────────────────────────────

/// Get the status of a session and optionally override its model.
pub struct SessionStatusTool {
    store: Option<Arc<crate::agent::session_store::SessionStore>>,
}

impl SessionStatusTool {
    pub fn new(store: Option<Arc<crate::agent::session_store::SessionStore>>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
struct SessionStatusArgs {
    #[serde(default)]
    session_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[async_trait]
impl Tool for SessionStatusTool {
    fn name(&self) -> &str {
        "session_status"
    }

    fn description(&self) -> &str {
        "Get the status of a session. Use session_key to target a specific session ('current', \
         'main', or a session ID). Use model to override the session's model (e.g. \
         'anthropic/claude-opus' or 'default' to reset)."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_key": {
                    "type": "string",
                    "description": "Session key: 'current', 'main', or a specific session ID"
                },
                "model": {
                    "type": "string",
                    "description": "Model override, e.g. 'provider/model' or 'default' to reset"
                }
            }
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Medium,
            categories: vec!["system".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let args: SessionStatusArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid arguments: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        let store = match &self.store {
            Some(s) => s,
            None => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some("Persistent session storage is not available".to_string()),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        // Resolve session key
        let key_raw = args.session_key.as_deref().unwrap_or("current");
        let target_id = if key_raw == "current" {
            if context.conversation_id.is_empty() {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "No current session available (conversation_id is empty)".to_string(),
                    ),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
            context.conversation_id.clone()
        } else if key_raw == "main" {
            match store.find_sessions(None, None, None, true).await {
                Ok(sessions) if !sessions.is_empty() => sessions[0].session_id.clone(),
                _ => {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some("No main session found".to_string()),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }
            }
        } else {
            key_raw.to_string()
        };

        // Load session
        let mut ps = match store.load_session(&target_id).await {
            Ok(Some(ps)) => ps,
            Ok(None) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Session {} not found", target_id)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to load session: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        let m = &ps.metadata;
        let mut changed_model = false;

        // Handle model override
        if let Some(model_raw) = args.model {
            let model_trim = model_raw.trim();
            if model_trim.eq_ignore_ascii_case("default") {
                if let Ok(mut state) = serde_json::from_str::<serde_json::Value>(&ps.state_json) {
                    if let Some(obj) = state.as_object_mut() {
                        obj.remove("providerOverride");
                        obj.remove("modelOverride");
                        ps.state_json = serde_json::to_string(&state).unwrap_or_default();
                        changed_model = true;
                    }
                }
            } else {
                let parts: Vec<&str> = model_trim.split('/').collect();
                let (provider, model) = if parts.len() >= 2 {
                    (parts[0].to_string(), parts[1..].join("/"))
                } else {
                    (String::new(), model_trim.to_string())
                };

                if let Ok(mut state) = serde_json::from_str::<serde_json::Value>(&ps.state_json) {
                    if let Some(obj) = state.as_object_mut() {
                        if !provider.is_empty() {
                            obj.insert(
                                "providerOverride".to_string(),
                                serde_json::Value::String(provider),
                            );
                        } else {
                            obj.remove("providerOverride");
                        }
                        obj.insert("modelOverride".to_string(), serde_json::Value::String(model));
                        ps.state_json = serde_json::to_string(&state).unwrap_or_default();
                        changed_model = true;
                    }
                }
            }

            if changed_model {
                if let Err(e) = store
                    .save_session(&target_id, &ps.metadata, &ps.state_json)
                    .await
                {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to save session model override: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }
            }
        }

        let runtime_model = serde_json::from_str::<serde_json::Value>(&ps.state_json)
            .ok()
            .and_then(|v| {
                v.get("modelOverride")
                    .and_then(|m| m.as_str())
                    .map(String::from)
            });
        let runtime_provider = serde_json::from_str::<serde_json::Value>(&ps.state_json)
            .ok()
            .and_then(|v| {
                v.get("providerOverride")
                    .and_then(|m| m.as_str())
                    .map(String::from)
            });

        let primary_model_label = match (&runtime_provider, &runtime_model) {
            (Some(p), Some(m)) => format!("{}/{}", p, m),
            (None, Some(m)) => m.clone(),
            _ => "default".to_string(),
        };

        let mut lines = vec![
            format!("Session: {}", m.session_id),
            format!("Agent: {}", m.agent_id),
            format!("Channel: {}:{}", m.channel, m.channel_id),
            format!("Messages: {}", m.message_count),
            format!("Model: {}", primary_model_label),
            format!("Active: {}", m.is_active),
        ];
        if changed_model {
            lines.push("Model override updated".to_string());
        }
        if let Some(name) = &m.name {
            lines.push(format!("Name: {}", name));
        }
        if let Some(bound) = &m.bound_agent_id {
            lines.push(format!("Bound agent: {}", bound));
        }
        lines.push(format!(
            "Created: {} | Last activity: {}",
            m.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
            m.last_activity.format("%Y-%m-%d %H:%M:%S UTC")
        ));

        let status_text = lines.join("\n");
        let data = serde_json::json!({
            "ok": true,
            "session_key": target_id,
            "changed_model": changed_model,
            "status_text": status_text,
            "metadata": {
                "session_id": m.session_id,
                "agent_id": m.agent_id,
                "channel": m.channel,
                "channel_id": m.channel_id,
                "created_at": m.created_at.to_rfc3339(),
                "last_activity": m.last_activity.to_rfc3339(),
                "is_active": m.is_active,
                "message_count": m.message_count,
                "model": primary_model_label,
                "name": m.name,
                "bound_agent_id": m.bound_agent_id,
            }
        });

        Ok(ToolExecutionResult {
            success: true,
            output: status_text,
            error: None,
            data: Some(data),
            execution_time: start.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sessions_history_args_parsing() {
        let args: SessionsHistoryArgs = serde_json::from_value(serde_json::json!({
            "session_id": "sess-123"
        }))
        .unwrap();
        assert_eq!(args.session_id, "sess-123");
    }

    #[test]
    fn test_sessions_send_args_parsing() {
        let args: SessionsSendArgs = serde_json::from_value(serde_json::json!({
            "session_id": "sess-123",
            "subagent_id": "sub-456",
            "message": "hello"
        }))
        .unwrap();
        assert_eq!(args.session_id, "sess-123");
        assert_eq!(args.subagent_id, "sub-456");
        assert_eq!(args.message, "hello");
    }

    #[test]
    fn test_sessions_yield_args_parsing() {
        let args: SessionsYieldArgs = serde_json::from_value(serde_json::json!({
            "subagent_id": "sub-789"
        }))
        .unwrap();
        assert_eq!(args.subagent_id, "sub-789");
    }

    #[test]
    fn test_session_status_args_parsing() {
        let args: SessionStatusArgs = serde_json::from_value(serde_json::json!({
            "session_key": "sess-1"
        }))
        .unwrap();
        assert_eq!(args.session_key, Some("sess-1".to_string()));
    }
}
