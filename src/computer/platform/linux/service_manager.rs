//! Service manager tool — manage systemd services.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Action types for service management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAction {
    Status,
    Start,
    Stop,
    Restart,
    Enable,
    Disable,
    List,
}

/// Result of a service operation.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceResult {
    pub service: String,
    pub action: String,
    pub success: bool,
    pub state: Option<String>,
    pub enabled: Option<bool>,
    pub active_since: Option<String>,
    pub main_pid: Option<u32>,
    pub memory: Option<String>,
    pub error: Option<String>,
}

/// Tool for managing systemd services.
#[derive(Debug)]
pub struct ServiceManagerTool;

impl Default for ServiceManagerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceManagerTool {
    pub fn new() -> Self {
        Self
    }

    async fn run_systemctl(args: &[&str]) -> Option<(bool, String)> {
        let result =
            timeout(Duration::from_secs(30), Command::new("systemctl").args(args).output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = if stderr.is_empty() {
                    stdout
                } else {
                    format!("{stdout}\n{stderr}")
                };
                Some((output.status.success(), combined))
            }
            Ok(Err(e)) => {
                warn!("Failed to run systemctl: {}", e);
                None
            }
            Err(_) => {
                warn!("systemctl timed out");
                None
            }
        }
    }

    async fn get_status(service: &str) -> ServiceResult {
        match Self::run_systemctl(&["status", service, "--no-pager"]).await {
            Some((success, output)) => {
                let mut result = ServiceResult {
                    service: service.to_string(),
                    action: "status".to_string(),
                    success,
                    state: None,
                    enabled: None,
                    active_since: None,
                    main_pid: None,
                    memory: None,
                    error: if success { None } else { Some(output.clone()) },
                };

                if success {
                    for line in output.lines() {
                        if line.starts_with("   Active:") {
                            if let Some(st) = line.split(':').nth(1) {
                                result.state =
                                    Some(st.split_whitespace().next().unwrap_or("").to_string());
                            }
                        } else if line.starts_with("   Loaded:") {
                            result.enabled = Some(line.contains("enabled"));
                        } else if line.starts_with("     Since:") || line.starts_with("  Since:") {
                            if let Some(since) = line.split(':').nth(1) {
                                result.active_since = Some(since.trim().to_string());
                            }
                        } else if line.starts_with(" Main PID:") {
                            if let Some(pid_str) = line.split_whitespace().nth(2) {
                                result.main_pid = pid_str.parse().ok();
                            }
                        } else if line.starts_with("    Memory:") {
                            if let Some(mem) = line.split(':').nth(1) {
                                result.memory = Some(mem.trim().to_string());
                            }
                        }
                    }
                }

                result
            }
            None => ServiceResult {
                service: service.to_string(),
                action: "status".to_string(),
                success: false,
                state: None,
                enabled: None,
                active_since: None,
                main_pid: None,
                memory: None,
                error: Some("Failed to execute systemctl".to_string()),
            },
        }
    }

    async fn do_action(service: &str, action: ServiceAction) -> ServiceResult {
        let action_str = match action {
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
            ServiceAction::Enable => "enable",
            ServiceAction::Disable => "disable",
            _ => "status",
        };

        info!("Service {}: {}", action_str, service);

        match Self::run_systemctl(&[action_str, service]).await {
            Some((success, output)) => ServiceResult {
                service: service.to_string(),
                action: action_str.to_string(),
                success,
                state: None,
                enabled: None,
                active_since: None,
                main_pid: None,
                memory: None,
                error: if success { None } else { Some(output) },
            },
            None => ServiceResult {
                service: service.to_string(),
                action: action_str.to_string(),
                success: false,
                state: None,
                enabled: None,
                active_since: None,
                main_pid: None,
                memory: None,
                error: Some("Failed to execute systemctl".to_string()),
            },
        }
    }

    async fn list_services(limit: usize) -> Vec<ServiceResult> {
        let cmd = format!(
            "systemctl list-units --type=service --no-pager --no-legend | head -n {}",
            limit
        );
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let result =
            timeout(Duration::from_secs(15), Command::new(&shell).arg("-c").arg(&cmd).output())
                .await;

        match result {
            Ok(Ok(output)) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .lines()
                    .take(limit)
                    .filter_map(|line| {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 4 {
                            Some(ServiceResult {
                                service: parts[0].to_string(),
                                action: "list".to_string(),
                                success: true,
                                state: Some(parts[3].to_string()),
                                enabled: None,
                                active_since: None,
                                main_pid: None,
                                memory: None,
                                error: None,
                            })
                        } else {
                            None
                        }
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }
}

#[async_trait]
impl Tool for ServiceManagerTool {
    fn name(&self) -> &str {
        "service_manager"
    }

    fn description(&self) -> &str {
        "Manage systemd services on Linux. Supports status, start, stop, restart, enable, disable, \
         and list. Use when the user asks to check a service, start/stop a daemon, or diagnose why \
         a service is not running."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::system_change(&["linux", "admin", "services"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Manage a systemd service",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action to perform",
                    "enum": ["status", "start", "stop", "restart", "enable", "disable", "list"]
                },
                "service": {
                    "type": "string",
                    "description": "Service name (e.g. nginx, sshd, docker). Omit for 'list' action."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max services to return for 'list' action",
                    "default": 30
                }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action_str = args["action"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'action' argument".to_string())
        })?;

        let action = match action_str {
            "status" => ServiceAction::Status,
            "start" => ServiceAction::Start,
            "stop" => ServiceAction::Stop,
            "restart" => ServiceAction::Restart,
            "enable" => ServiceAction::Enable,
            "disable" => ServiceAction::Disable,
            "list" => ServiceAction::List,
            _ => {
                return Ok(ToolExecutionResult::error(format!(
                    "Unknown action '{}'. Use: status, start, stop, restart, enable, disable, list",
                    action_str
                )));
            }
        };

        if action == ServiceAction::List {
            let limit = args["limit"].as_u64().unwrap_or(30) as usize;
            let services = Self::list_services(limit).await;
            let json = serde_json::to_string_pretty(&services)
                .map_err(crate::error::SyscityError::Serialization)?;
            return Ok(
                ToolExecutionResult::success(json).with_data(serde_json::to_value(services)?)
            );
        }

        let service = args["service"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation(
                "Missing 'service' argument (required for all actions except 'list')".to_string(),
            )
        })?;

        let result = match action {
            ServiceAction::Status => Self::get_status(service).await,
            _ => Self::do_action(service, action).await,
        };

        let json = serde_json::to_string_pretty(&result)
            .map_err(crate::error::SyscityError::Serialization)?;

        let success = result.success;
        let mut exec_result =
            ToolExecutionResult::success(json).with_data(serde_json::to_value(result)?);
        if !success {
            exec_result = ToolExecutionResult::error(exec_result.output.clone())
                .with_data(exec_result.data.unwrap_or(Value::Null));
        }

        Ok(exec_result)
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        cfg!(target_os = "linux") && std::path::Path::new("/run/systemd/system").exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_manager_creation() {
        let tool = ServiceManagerTool::new();
        assert_eq!(tool.name(), "service_manager");
        assert!(!tool.description().is_empty());
    }
}
