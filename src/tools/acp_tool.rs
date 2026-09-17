//! ACP (Agent Control Plane) Tool - Subagent Spawning
//!
//! This tool allows agents to spawn subagents for parallel task execution.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::acp::{AcpControlPlane, AcpSessionId, SpawnMode, SubagentConfig, ThreadBinding};
use crate::channels::IncomingMessage;
use crate::tools::sdk::ToolCapabilities;

/// Tool for spawning subagents via ACP
pub struct AcpSpawnTool {
    acp: Arc<AcpControlPlane>,
    session_store: Option<Arc<crate::agent::session_store::SessionStore>>,
}

impl AcpSpawnTool {
    /// Create a new ACP spawn tool
    pub fn new(
        acp: Arc<AcpControlPlane>,
        session_store: Option<Arc<crate::agent::session_store::SessionStore>>,
    ) -> Self {
        Self { acp, session_store }
    }
}

/// Arguments for the acp_spawn tool
#[derive(Debug, Deserialize)]
struct SpawnSubagentArgs {
    /// The task/prompt for the subagent
    pub task: String,
    /// Spawn mode: "run" (one-shot) or "session" (persistent)
    #[serde(default = "default_spawn_mode")]
    pub mode: String,
    /// Thread binding: "new", "parent", "auto", or specific thread ID
    #[serde(default = "default_thread_binding")]
    pub thread_binding: String,
    /// Maximum execution time in seconds
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Session ID to bind this subagent to. Future messages to this session
    /// will be routed to the subagent.
    #[serde(default)]
    pub bind_to_session: Option<String>,
}

fn default_spawn_mode() -> String {
    "run".to_string()
}

fn default_thread_binding() -> String {
    "auto".to_string()
}

#[async_trait]
impl Tool for AcpSpawnTool {
    fn name(&self) -> &str {
        "acp_spawn"
    }

    fn description(&self) -> &str {
        "Spawn a subagent to handle a specific task. The subagent can operate in 'run' mode \
         (one-shot execution) or 'session' mode (persistent conversation). Use this for parallel \
         task execution or delegating work to specialized agents."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task or prompt to give to the subagent"
                },
                "mode": {
                    "type": "string",
                    "enum": ["run", "session"],
                    "description": "Spawn mode: 'run' for one-shot execution, 'session' for persistent agent",
                    "default": "run"
                },
                "thread_binding": {
                    "type": "string",
                    "description": "Thread binding: 'new' for isolated thread, 'parent' to bind to parent, 'auto' for automatic",
                    "default": "auto"
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Maximum execution time in seconds (only for run mode)",
                    "minimum": 1,
                    "maximum": 3600
                },
                "bind_to_session": {
                    "type": "string",
                    "description": "Session ID to bind this subagent to. Future messages to this session will be routed to the subagent."
                }
            },
            "required": ["task"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: crate::tools::approval::RiskLevel::High,
            categories: vec!["system".to_string(), "acp".to_string()],
            idempotent: false,
            compensation: None,
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();

        let args: SpawnSubagentArgs = match serde_json::from_value(args) {
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

        // Parse spawn mode
        let mode = match args.mode.as_str() {
            "session" => SpawnMode::Session,
            _ => SpawnMode::Run,
        };

        // Parse thread binding
        let thread_binding = match args.thread_binding.as_str() {
            "new" => ThreadBinding::New,
            "parent" => ThreadBinding::Parent,
            "auto" => ThreadBinding::Auto,
            id => ThreadBinding::Thread(id.to_string()),
        };

        // Create ACP session
        let session_id = AcpSessionId::new();
        let parent_id = format!("agent-{}", context.conversation_id);

        // Build subagent config
        let config = SubagentConfig {
            mode,
            thread_binding,
            system_prompt: None,
            timeout_seconds: args.timeout_seconds.or(Some(300)),
        };

        info!("Spawning subagent for task: {} (mode: {:?})", args.task, config.mode);

        // Spawn the subagent
        match self
            .acp
            .spawn_subagent(session_id.clone(), parent_id.clone(), config)
            .await
        {
            Ok(handle) => {
                let subagent_id = handle.id.clone();

                // Bind subagent to session if requested (unified session model)
                if let Some(ref target_session) = args.bind_to_session {
                    if let Some(ref store) = self.session_store {
                        if let Ok(Some(mut ps)) = store.load_session(target_session).await {
                            ps.metadata.bound_agent_id = Some(subagent_id.clone());
                            if let Err(e) = store
                                .save_session(target_session, &ps.metadata, &ps.state_json)
                                .await
                            {
                                warn!(
                                    "Failed to bind subagent {} to session {}: {}",
                                    subagent_id, target_session, e
                                );
                            } else {
                                info!(
                                    "Bound subagent {} to session {}",
                                    subagent_id, target_session
                                );
                            }
                        } else {
                            warn!("Cannot bind subagent: session {} not found", target_session);
                        }
                    } else {
                        warn!("Cannot bind subagent: session store not available");
                    }
                }

                // Create message for the subagent
                let message = IncomingMessage::new(
                    context.user_id.clone(),
                    context.conversation_id.clone(),
                    args.task,
                );

                // Send task to subagent and wait for response
                match self.acp.send_message(&subagent_id, message).await {
                    Ok(response) => {
                        // For Run mode, the subagent terminates after completion
                        // For Session mode, the subagent remains available
                        let mode_info = match handle.mode {
                            SpawnMode::Run => "Subagent completed and terminated",
                            SpawnMode::Session => "Subagent remains active in session",
                        };

                        Ok(ToolExecutionResult {
                            success: true,
                            output: response.to_string(),
                            error: None,
                            data: Some(serde_json::json!({
                                "subagent_id": subagent_id,
                                "session_id": session_id.to_string(),
                                "mode": format!("{:?}", handle.mode),
                                "status": mode_info,
                                "response": response,
                            })),
                            execution_time: start.elapsed(),
                        })
                    }
                    Err(e) => {
                        warn!("Subagent {} failed to process task: {}", subagent_id, e);

                        // Try to shutdown the subagent
                        if let Err(e) = self.acp.shutdown_subagent(&subagent_id).await {
                            warn!("Failed to shutdown failed subagent {}: {}", subagent_id, e);
                        }

                        Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Subagent failed: {}", e)),
                            data: Some(serde_json::json!({
                                "subagent_id": subagent_id,
                                "error": e.to_string(),
                            })),
                            execution_time: start.elapsed(),
                        })
                    }
                }
            }
            Err(e) => {
                warn!("Failed to spawn subagent: {}", e);
                Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to spawn subagent: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                })
            }
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        // ACP tool is always available if the ACP is enabled
        true
    }
}

/// Tool for managing ACP sessions
pub struct AcpSessionTool {
    acp: Arc<AcpControlPlane>,
}

impl AcpSessionTool {
    /// Create a new ACP session management tool
    pub fn new(acp: Arc<AcpControlPlane>) -> Self {
        Self { acp }
    }
}

/// Arguments for session management
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum SessionAction {
    /// List active sessions
    List,
    /// Get session info
    Get { session_id: String },
    /// Terminate a session
    Terminate { session_id: String },
    /// Kill a subagent immediately
    Kill { subagent_id: String },
    /// Steer a subagent — cancel current execution and send a new message
    Steer {
        subagent_id: String,
        message: String,
    },
    /// Send message to a session subagent
    Message {
        session_id: String,
        subagent_id: String,
        message: String,
    },
}

#[async_trait]
impl Tool for AcpSessionTool {
    fn name(&self) -> &str {
        "acp_session"
    }

    fn description(&self) -> &str {
        "Manage ACP (Agent Control Plane) sessions. List active sessions, get session info, \
         terminate sessions, kill subagents, steer running subagents, or send messages to active \
         subagents."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "get", "terminate", "kill", "steer", "message"],
                    "description": "Action to perform"
                },
                "session_id": {
                    "type": "string",
                    "description": "Session ID (required for get, terminate, message)"
                },
                "subagent_id": {
                    "type": "string",
                    "description": "Subagent ID (required for kill, steer, message action)"
                },
                "message": {
                    "type": "string",
                    "description": "Message to send (required for steer, message action)"
                }
            },
            "required": ["action"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: crate::tools::approval::RiskLevel::High,
            categories: vec!["network".to_string(), "acp".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();

        let action: SessionAction = match serde_json::from_value(args) {
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

        match action {
            SessionAction::List => {
                // List all subagents as a proxy for sessions
                let subagents = self.acp.list_subagents().await;

                let session_info: Vec<_> = subagents
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "subagent_id": s.id,
                            "session_id": s.session_id.to_string(),
                            "parent_id": s.parent_id,
                            "mode": format!("{:?}", s.mode),
                            "status": format!("{:?}", s.status),
                            "thread_id": s.thread_id,
                        })
                    })
                    .collect();

                Ok(ToolExecutionResult {
                    success: true,
                    output: format!("Found {} active subagent(s)", subagents.len()),
                    error: None,
                    data: Some(serde_json::json!({ "subagents": session_info })),
                    execution_time: start.elapsed(),
                })
            }
            SessionAction::Get { session_id } => {
                let session_id = AcpSessionId(session_id);

                match self.acp.get_session_info(&session_id).await {
                    Some(info) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!(
                            "Session {} has {} subagent(s)",
                            info.id, info.subagent_count
                        ),
                        error: None,
                        data: Some(serde_json::json!({
                            "id": info.id.to_string(),
                            "parent_agent_id": info.parent_agent_id,
                            "subagent_count": info.subagent_count,
                            "created_at": info.created_at.to_rfc3339(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    None => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Session {} not found", session_id)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }
            SessionAction::Terminate { session_id } => {
                let session_id = AcpSessionId(session_id);

                match self.acp.terminate_session(&session_id).await {
                    Ok(count) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!(
                            "Terminated {} subagent(s) in session {}",
                            count, session_id
                        ),
                        error: None,
                        data: Some(serde_json::json!({
                            "terminated_count": count,
                            "session_id": session_id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to terminate session: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }
            SessionAction::Kill { subagent_id } => {
                match self.acp.kill_subagent(&subagent_id).await {
                    Ok(true) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Killed subagent {}", subagent_id),
                        error: None,
                        data: Some(serde_json::json!({
                            "subagent_id": subagent_id,
                            "action": "kill",
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Ok(false) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Subagent {} not found", subagent_id)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to kill subagent: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }
            SessionAction::Steer { subagent_id, message } => {
                match self.acp.steer_subagent(&subagent_id, message).await {
                    Ok(response) => Ok(ToolExecutionResult {
                        success: true,
                        output: response.clone(),
                        error: None,
                        data: Some(serde_json::json!({
                            "subagent_id": subagent_id,
                            "response": response,
                            "action": "steer",
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to steer subagent: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }
            SessionAction::Message {
                session_id,
                subagent_id,
                message,
            } => {
                let _session_id = AcpSessionId(session_id);
                let incoming = IncomingMessage::new(
                    "user".to_string(),
                    "tool-invocation".to_string(),
                    message,
                );

                match self.acp.send_message(&subagent_id, incoming).await {
                    Ok(response) => Ok(ToolExecutionResult {
                        success: true,
                        output: response.clone(),
                        error: None,
                        data: Some(serde_json::json!({
                            "subagent_id": subagent_id,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_subagent_args_defaults() {
        let args: SpawnSubagentArgs = serde_json::from_value(serde_json::json!({
            "task": "Do something"
        }))
        .unwrap();
        assert_eq!(args.task, "Do something");
        assert_eq!(args.mode, "run");
        assert_eq!(args.thread_binding, "auto");
        assert_eq!(args.timeout_seconds, None);
    }

    #[test]
    fn test_spawn_subagent_args_custom() {
        let args: SpawnSubagentArgs = serde_json::from_value(serde_json::json!({
            "task": "Research topic",
            "mode": "session",
            "thread_binding": "new",
            "agent_type": "researcher",
            "timeout_seconds": 120
        }))
        .unwrap();
        assert_eq!(args.task, "Research topic");
        assert_eq!(args.mode, "session");
        assert_eq!(args.thread_binding, "new");
        assert_eq!(args.timeout_seconds, Some(120));
    }

    #[test]
    fn test_session_action_parsing() {
        let action: SessionAction = serde_json::from_value(serde_json::json!({
            "action": "list"
        }))
        .unwrap();
        assert!(matches!(action, SessionAction::List));

        let action: SessionAction = serde_json::from_value(serde_json::json!({
            "action": "get",
            "session_id": "sess-1"
        }))
        .unwrap();
        assert!(matches!(action, SessionAction::Get { session_id } if session_id == "sess-1"));

        let action: SessionAction = serde_json::from_value(serde_json::json!({
            "action": "terminate",
            "session_id": "sess-1"
        }))
        .unwrap();
        assert!(
            matches!(action, SessionAction::Terminate { session_id } if session_id == "sess-1")
        );

        let action: SessionAction = serde_json::from_value(serde_json::json!({
            "action": "message",
            "session_id": "sess-1",
            "subagent_id": "sub-1",
            "message": "hello"
        }))
        .unwrap();
        assert!(matches!(action, SessionAction::Message { session_id, subagent_id, message }
            if session_id == "sess-1" && subagent_id == "sub-1" && message == "hello"
        ));
    }

    #[tokio::test]
    async fn test_acp_spawn_tool_name_and_schema() {
        let acp = Arc::new(AcpControlPlane::new(50));
        let tool = AcpSpawnTool::new(acp, None);
        assert_eq!(tool.name(), "acp_spawn");
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
        let req = schema.get("required").unwrap().as_array().unwrap();
        assert!(req.contains(&serde_json::json!("task")));
    }

    #[tokio::test]
    async fn test_acp_session_tool_name_and_schema() {
        let acp = Arc::new(AcpControlPlane::new(50));
        let tool = AcpSessionTool::new(acp);
        assert_eq!(tool.name(), "acp_session");
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }

    #[tokio::test]
    async fn test_acp_session_tool_list_empty() {
        let acp = Arc::new(AcpControlPlane::new(50));
        let tool = AcpSessionTool::new(acp);
        let ctx = ToolContext::new("user", "conv");
        let result = tool
            .execute(serde_json::json!({ "action": "list" }), &ctx)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("0 active subagent"));
        let data = result.data.unwrap();
        let subagents = data.get("subagents").unwrap().as_array().unwrap();
        assert_eq!(subagents.len(), 0);
    }

    #[tokio::test]
    async fn test_acp_session_tool_get_not_found() {
        let acp = Arc::new(AcpControlPlane::new(50));
        let tool = AcpSessionTool::new(acp);
        let ctx = ToolContext::new("user", "conv");
        let result = tool
            .execute(serde_json::json!({ "action": "get", "session_id": "nonexistent" }), &ctx)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_acp_session_tool_invalid_args() {
        let acp = Arc::new(AcpControlPlane::new(50));
        let tool = AcpSessionTool::new(acp);
        let ctx = ToolContext::new("user", "conv");
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid arguments"));
    }

    #[tokio::test]
    async fn test_acp_spawn_tool_no_agent_builder() {
        let acp = Arc::new(AcpControlPlane::new(50));
        let tool = AcpSpawnTool::new(acp, None);
        let ctx = ToolContext::new("user", "conv");
        let result = tool
            .execute(
                serde_json::json!({
                    "task": "Do something"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap()
            .contains("No agent builder configured"));
    }

    #[tokio::test]
    async fn test_acp_spawn_tool_invalid_args() {
        let acp = Arc::new(AcpControlPlane::new(50));
        let tool = AcpSpawnTool::new(acp, None);
        let ctx = ToolContext::new("user", "conv");
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid arguments"));
    }

    #[tokio::test]
    async fn test_acp_session_tool_terminate_not_found() {
        let acp = Arc::new(AcpControlPlane::new(50));
        let tool = AcpSessionTool::new(acp);
        let ctx = ToolContext::new("user", "conv");
        let result = tool
            .execute(
                serde_json::json!({ "action": "terminate", "session_id": "no-such-session" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }
}
