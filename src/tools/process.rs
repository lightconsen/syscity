//! Process Tool — Background Process Management
//!
//! tool for managing background processes.
//! Unlike ShellTool (one-shot execution), ProcessTool starts processes
//! that can run in the background and be queried/stopped later.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::info;

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::tools::process_runner::{ProcessError, ProcessRequest, WriteFence};
use crate::tools::sdk::ToolCapabilities;

/// Status of a background process
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    Exited { code: Option<i32> },
    Killed,
    Error(String),
}

/// Tracked background process
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedProcess {
    pub id: String,
    pub command: String,
    pub status: ProcessStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub pid: Option<u32>,
}

/// In-memory process registry
#[derive(Debug, Default)]
pub struct ProcessRegistry {
    processes: Arc<RwLock<HashMap<String, TrackedProcess>>>,
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, id: String, command: String, pid: Option<u32>) {
        let proc = TrackedProcess {
            id: id.clone(),
            command,
            status: ProcessStatus::Running,
            started_at: chrono::Utc::now(),
            pid,
        };
        let mut procs = self.processes.write().await;
        procs.insert(id, proc);
    }

    pub async fn update_status(&self, id: &str, status: ProcessStatus) {
        let mut procs = self.processes.write().await;
        if let Some(proc) = procs.get_mut(id) {
            proc.status = status;
        }
    }

    pub async fn get(&self, id: &str) -> Option<TrackedProcess> {
        let procs = self.processes.read().await;
        procs.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<TrackedProcess> {
        let procs = self.processes.read().await;
        procs.values().cloned().collect()
    }

    pub async fn remove(&self, id: &str) {
        let mut procs = self.processes.write().await;
        procs.remove(id);
    }
}

/// Process management tool
pub struct ProcessTool {
    registry: ProcessRegistry,
}

impl ProcessTool {
    pub fn new() -> Self {
        Self {
            registry: ProcessRegistry::new(),
        }
    }
}

impl Default for ProcessTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ProcessAction {
    Start {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        working_dir: Option<String>,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
    },
    Status {
        process_id: String,
    },
    Stop {
        process_id: String,
        #[serde(default)]
        force: bool,
    },
    List,
}

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &str {
        "process"
    }

    fn description(&self) -> &str {
        "Manage background processes. Start long-running commands, check status, and stop them \
         gracefully or forcefully. Unlike shell, this tracks processes over time."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "status", "stop", "list"],
                    "description": "Process action"
                },
                "command": {
                    "type": "string",
                    "description": "Command to start (required for start)"
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Command arguments"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Working directory"
                },
                "env": {
                    "type": "object",
                    "description": "Environment variables"
                },
                "process_id": {
                    "type": "string",
                    "description": "Process ID (required for status/stop)"
                },
                "force": {
                    "type": "boolean",
                    "description": "Force kill (for stop)",
                    "default": false
                }
            },
            "required": ["action"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: crate::tools::approval::RiskLevel::High,
            categories: vec!["system".to_string(), "process".to_string()],
            idempotent: false,
            compensation: None,
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        // Subprocess management; no mobile equivalent (mobile migration §4.4).
        !cfg!(mobile_os)
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let action: ProcessAction = match serde_json::from_value(args) {
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
            ProcessAction::Start {
                command,
                args: cmd_args,
                working_dir,
                env,
            } => {
                if !context.is_command_allowed(&command) {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Command '{}' is not allowed", command)),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                let req = ProcessRequest {
                    argv: std::iter::once(command.clone())
                        .chain(cmd_args.iter().cloned())
                        .collect(),
                    cwd: Some(match working_dir {
                        Some(dir) => std::path::PathBuf::from(dir),
                        None => context.working_directory().clone(),
                    }),
                    env: env.unwrap_or_default(),
                    fence: context.workspace_only().then(|| {
                        WriteFence::new(
                            context.workspace_root().clone(),
                            context.allowed_paths().to_vec(),
                            context.fence_network(),
                            context.fence_namespaces(),
                        )
                    }),
                    ..Default::default()
                };

                match crate::tools::process_runner::spawn(&req).await {
                    Ok(child) => {
                        let pid = child.id();
                        let process_id = uuid::Uuid::new_v4().to_string();
                        let full_cmd = format!("{} {}", command, cmd_args.join(" "));

                        self.registry
                            .register(process_id.clone(), full_cmd.clone(), pid)
                            .await;

                        info!(
                            "Started background process {} (pid={:?}): {}",
                            process_id, pid, full_cmd
                        );

                        // Spawn a detached task to wait for the child and update status
                        let registry = self.registry.clone();
                        let id = process_id.clone();
                        tokio::spawn(async move {
                            let mut child = child;
                            match child.wait().await {
                                Ok(status) => {
                                    registry
                                        .update_status(
                                            &id,
                                            ProcessStatus::Exited { code: status.code() },
                                        )
                                        .await;
                                }
                                Err(e) => {
                                    registry
                                        .update_status(&id, ProcessStatus::Error(e.to_string()))
                                        .await;
                                }
                            }
                        });

                        Ok(ToolExecutionResult {
                            success: true,
                            output: format!("Started process {}: {}", process_id, full_cmd),
                            error: None,
                            data: Some(serde_json::json!({
                                "process_id": process_id,
                                "command": full_cmd,
                                "pid": pid,
                            })),
                            execution_time: start.elapsed(),
                        })
                    }
                    Err(e) => {
                        let msg = match e {
                            ProcessError::Spawn { source, .. } => {
                                format!("Failed to start process: {}", source)
                            }
                            other => format!("Failed to start process: {}", other),
                        };
                        Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(msg),
                            data: None,
                            execution_time: start.elapsed(),
                        })
                    }
                }
            }
            ProcessAction::Status { process_id } => match self.registry.get(&process_id).await {
                Some(proc) => Ok(ToolExecutionResult {
                    success: true,
                    output: format!("Process {}: {:?}", process_id, proc.status),
                    error: None,
                    data: Some(serde_json::to_value(proc).unwrap_or_default()),
                    execution_time: start.elapsed(),
                }),
                None => Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Process {} not found", process_id)),
                    data: None,
                    execution_time: start.elapsed(),
                }),
            },
            ProcessAction::Stop { process_id, force } => {
                let proc = match self.registry.get(&process_id).await {
                    Some(p) => p,
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Process {} not found", process_id)),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                if let Some(pid) = proc.pid {
                    #[cfg(unix)]
                    {
                        use nix::sys::signal::{self, Signal};
                        use nix::unistd::Pid;

                        let sig = if force {
                            Signal::SIGKILL
                        } else {
                            Signal::SIGTERM
                        };
                        match signal::kill(Pid::from_raw(pid as i32), sig) {
                            Ok(_) => {
                                self.registry
                                    .update_status(&process_id, ProcessStatus::Killed)
                                    .await;
                                Ok(ToolExecutionResult {
                                    success: true,
                                    output: format!("Process {} stopped", process_id),
                                    error: None,
                                    data: None,
                                    execution_time: start.elapsed(),
                                })
                            }
                            Err(e) => Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Failed to stop process: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            }),
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        // On non-Unix, we can't send signals; just mark as killed
                        self.registry
                            .update_status(&process_id, ProcessStatus::Killed)
                            .await;
                        Ok(ToolExecutionResult {
                            success: true,
                            output: format!(
                                "Process {} marked as stopped (signal not supported)",
                                process_id
                            ),
                            error: None,
                            data: None,
                            execution_time: start.elapsed(),
                        })
                    }
                } else {
                    self.registry
                        .update_status(&process_id, ProcessStatus::Killed)
                        .await;
                    Ok(ToolExecutionResult {
                        success: true,
                        output: format!(
                            "Process {} marked as stopped (no PID available)",
                            process_id
                        ),
                        error: None,
                        data: None,
                        execution_time: start.elapsed(),
                    })
                }
            }
            ProcessAction::List => {
                let procs = self.registry.list().await;
                let running = procs
                    .iter()
                    .filter(|p| matches!(p.status, ProcessStatus::Running))
                    .count();

                let list: Vec<_> = procs
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "id": p.id,
                            "command": p.command,
                            "status": serde_json::to_value(&p.status).unwrap_or_default(),
                            "pid": p.pid,
                            "started_at": p.started_at.to_rfc3339(),
                        })
                    })
                    .collect();

                Ok(ToolExecutionResult {
                    success: true,
                    output: format!("{} process(s), {} running", procs.len(), running),
                    error: None,
                    data: Some(serde_json::json!({ "processes": list, "running": running })),
                    execution_time: start.elapsed(),
                })
            }
        }
    }
}

impl Clone for ProcessRegistry {
    fn clone(&self) -> Self {
        Self {
            processes: Arc::clone(&self.processes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_process_registry_register_and_get() {
        let registry = ProcessRegistry::new();
        registry
            .register("p1".to_string(), "echo hello".to_string(), Some(1234))
            .await;

        let proc = registry.get("p1").await;
        assert!(proc.is_some());
        let proc = proc.unwrap();
        assert_eq!(proc.id, "p1");
        assert_eq!(proc.command, "echo hello");
        assert_eq!(proc.pid, Some(1234));
        assert!(matches!(proc.status, ProcessStatus::Running));
    }

    #[tokio::test]
    async fn test_process_registry_update_status() {
        let registry = ProcessRegistry::new();
        registry
            .register("p1".to_string(), "cmd".to_string(), None)
            .await;

        registry
            .update_status("p1", ProcessStatus::Exited { code: Some(0) })
            .await;

        let proc = registry.get("p1").await.unwrap();
        assert!(matches!(proc.status, ProcessStatus::Exited { code: Some(0) }));
    }

    #[tokio::test]
    async fn test_process_registry_list_and_remove() {
        let registry = ProcessRegistry::new();
        registry
            .register("p1".to_string(), "cmd1".to_string(), None)
            .await;
        registry
            .register("p2".to_string(), "cmd2".to_string(), None)
            .await;

        let list = registry.list().await;
        assert_eq!(list.len(), 2);

        registry.remove("p1").await;
        assert!(registry.get("p1").await.is_none());
        assert_eq!(registry.list().await.len(), 1);
    }

    #[tokio::test]
    async fn test_process_registry_get_missing() {
        let registry = ProcessRegistry::new();
        assert!(registry.get("missing").await.is_none());
    }

    #[tokio::test]
    async fn test_process_tool_list_empty() {
        let tool = ProcessTool::new();
        let ctx = ToolContext::new("user", "conv");

        let result = tool
            .execute(serde_json::json!({ "action": "list" }), &ctx)
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("0 process"));
    }

    #[tokio::test]
    async fn test_process_tool_status_not_found() {
        let tool = ProcessTool::new();
        let ctx = ToolContext::new("user", "conv");

        let result = tool
            .execute(serde_json::json!({ "action": "status", "process_id": "noexist" }), &ctx)
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_process_tool_stop_not_found() {
        let tool = ProcessTool::new();
        let ctx = ToolContext::new("user", "conv");

        let result = tool
            .execute(serde_json::json!({ "action": "stop", "process_id": "noexist" }), &ctx)
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_process_tool_invalid_args() {
        let tool = ProcessTool::new();
        let ctx = ToolContext::new("user", "conv");

        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid arguments"));
    }

    #[tokio::test]
    async fn test_process_tool_start_command_not_allowed() {
        let tool = ProcessTool::new();
        // Only allow "echo", not "rm"
        let ctx = ToolContext::new("user", "conv").allow_command("echo");

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "start",
                    "command": "rm",
                    "args": ["-rf", "/"]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not allowed"));
    }

    #[test]
    fn test_process_status_serialization() {
        let running = ProcessStatus::Running;
        let json = serde_json::to_string(&running).unwrap();
        assert!(json.contains("running"));

        let exited = ProcessStatus::Exited { code: Some(0) };
        let json = serde_json::to_string(&exited).unwrap();
        assert!(json.contains("exited"));
    }

    #[test]
    fn test_process_registry_clone_shares_state() {
        let registry = ProcessRegistry::new();
        let cloned = registry.clone();

        // Clone should share the same underlying data
        // (This is a smoke test to verify Clone works)
        assert_eq!(std::sync::Arc::strong_count(&registry.processes), 2);
        drop(cloned);
        assert_eq!(std::sync::Arc::strong_count(&registry.processes), 1);
    }
}
