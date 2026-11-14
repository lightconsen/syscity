//! Cron manager tool — list, add, and remove cron jobs and systemd timers.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::warn;

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Action types for cron management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronAction {
    List,
    Add,
    Remove,
    Timers,
}

/// A cron job entry.
#[derive(Debug, Clone, Serialize)]
pub struct CronEntry {
    pub minute: String,
    pub hour: String,
    pub day_of_month: String,
    pub month: String,
    pub day_of_week: String,
    pub command: String,
    pub user: Option<String>,
}

/// A systemd timer entry.
#[derive(Debug, Clone, Serialize)]
pub struct TimerEntry {
    pub unit: String,
    pub next_run: String,
    pub last_run: String,
    pub passed: bool,
}

/// Tool for managing cron jobs and systemd timers on Linux.
#[derive(Debug)]
pub struct CronManagerTool;

impl Default for CronManagerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl CronManagerTool {
    pub fn new() -> Self {
        Self
    }

    async fn run_cmd(cmd: &str, args: &[&str], timeout_secs: u64) -> Option<(bool, String)> {
        let result =
            timeout(Duration::from_secs(timeout_secs), Command::new(cmd).args(args).output()).await;

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
                warn!("Failed to run {}: {}", cmd, e);
                None
            }
            Err(_) => {
                warn!("{} timed out", cmd);
                None
            }
        }
    }

    async fn do_list(user: Option<&str>) -> Vec<CronEntry> {
        let args: Vec<&str> = if let Some(u) = user {
            vec!["-u", u, "-l"]
        } else {
            vec!["-l"]
        };

        match Self::run_cmd("crontab", &args, 15).await {
            Some((true, output)) => output
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
                .filter_map(|line| {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 6 {
                        Some(CronEntry {
                            minute: parts[0].to_string(),
                            hour: parts[1].to_string(),
                            day_of_month: parts[2].to_string(),
                            month: parts[3].to_string(),
                            day_of_week: parts[4].to_string(),
                            command: parts[5..].join(" "),
                            user: user.map(|s| s.to_string()),
                        })
                    } else {
                        None
                    }
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    async fn do_add(expression: &str, command: &str, user: Option<&str>) -> (bool, String) {
        // Read existing crontab
        let list_args: Vec<&str> = if let Some(u) = user {
            vec!["-u", u, "-l"]
        } else {
            vec!["-l"]
        };

        let existing = match Self::run_cmd("crontab", &list_args, 15).await {
            Some((true, output)) => output,
            _ => String::new(),
        };

        let new_line = format!("{} {}\n", expression, command);
        let new_crontab = format!("{}{}", existing, new_line);

        // Write via stdin using a shell heredoc is complex with tokio::process;
        // Instead use a temporary file.
        let tmp = format!("/tmp/syscity_cron_{}.tmp", uuid::Uuid::new_v4());
        if let Err(e) = tokio::fs::write(&tmp, &new_crontab).await {
            tracing::warn!("Failed to write temp file '{}': {}", tmp, e);
        }

        let install_args: Vec<&str> = if let Some(u) = user {
            vec!["-u", u, &tmp]
        } else {
            vec![&tmp]
        };

        let result = match Self::run_cmd("crontab", &install_args, 15).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to install crontab".to_string()),
        };

        if let Err(e) = tokio::fs::remove_file(&tmp).await {
            tracing::warn!("Failed to cleanup temp file '{}': {}", tmp, e);
        }
        result
    }

    async fn do_remove(line_pattern: &str, user: Option<&str>) -> (bool, String) {
        let list_args: Vec<&str> = if let Some(u) = user {
            vec!["-u", u, "-l"]
        } else {
            vec!["-l"]
        };

        let existing = match Self::run_cmd("crontab", &list_args, 15).await {
            Some((true, output)) => output,
            _ => return (false, "Failed to read existing crontab".to_string()),
        };

        let filtered: String = existing
            .lines()
            .filter(|l| !l.contains(line_pattern))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";

        let tmp = format!("/tmp/syscity_cron_{}.tmp", uuid::Uuid::new_v4());
        if let Err(e) = tokio::fs::write(&tmp, &filtered).await {
            tracing::warn!("Failed to write temp file '{}': {}", tmp, e);
        }

        let install_args: Vec<&str> = if let Some(u) = user {
            vec!["-u", u, &tmp]
        } else {
            vec![&tmp]
        };

        let result = match Self::run_cmd("crontab", &install_args, 15).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to install crontab".to_string()),
        };

        if let Err(e) = tokio::fs::remove_file(&tmp).await {
            tracing::warn!("Failed to cleanup temp file '{}': {}", tmp, e);
        }
        result
    }

    async fn do_timers() -> Vec<TimerEntry> {
        match Self::run_cmd("systemctl", &["list-timers", "--no-pager", "--no-legend"], 15).await {
            Some((true, output)) => output
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 5 {
                        Some(TimerEntry {
                            next_run: parts[0].to_string(),
                            last_run: parts[1].to_string(),
                            passed: parts.get(2).map(|s| *s == "*").unwrap_or(false),
                            unit: parts.last()?.to_string(),
                        })
                    } else {
                        None
                    }
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[async_trait]
impl Tool for CronManagerTool {
    fn name(&self) -> &str {
        "cron_manager"
    }

    fn description(&self) -> &str {
        "Manage cron jobs and systemd timers on Linux. Supports listing cron entries, \
         adding/removing jobs, and listing systemd timers."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::system_change(&["linux", "admin", "cron"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Manage cron jobs and timers",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: list | add | remove | timers",
                    "enum": ["list", "add", "remove", "timers"]
                },
                "user": {
                    "type": "string",
                    "description": "Optional user for crontab operations (default: current user)"
                },
                "expression": {
                    "type": "string",
                    "description": "Cron expression for 'add' (e.g. '0 2 * * *')"
                },
                "command": {
                    "type": "string",
                    "description": "Command to run for 'add' action"
                },
                "pattern": {
                    "type": "string",
                    "description": "Pattern to match for 'remove' (removes lines containing this string)"
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
        let action_str = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        let action = match action_str {
            "add" => CronAction::Add,
            "remove" => CronAction::Remove,
            "timers" => CronAction::Timers,
            _ => CronAction::List,
        };

        let user = args.get("user").and_then(|v| v.as_str());

        let data = match action {
            CronAction::List => {
                let entries = Self::do_list(user).await;
                serde_json::json!({
                    "action": "list",
                    "user": user,
                    "count": entries.len(),
                    "entries": entries,
                })
            }
            CronAction::Add => {
                let expression = args
                    .get("expression")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                if expression.is_empty() || command.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "'expression' and 'command' are required for add action".to_string(),
                    ));
                }
                let (success, output) = Self::do_add(expression, command, user).await;
                serde_json::json!({
                    "action": "add",
                    "user": user,
                    "success": success,
                    "output": output,
                })
            }
            CronAction::Remove => {
                let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                if pattern.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "'pattern' is required for remove action".to_string(),
                    ));
                }
                let (success, output) = Self::do_remove(pattern, user).await;
                serde_json::json!({
                    "action": "remove",
                    "user": user,
                    "success": success,
                    "output": output,
                })
            }
            CronAction::Timers => {
                let timers = Self::do_timers().await;
                serde_json::json!({
                    "action": "timers",
                    "count": timers.len(),
                    "timers": timers,
                })
            }
        };

        let message = format!("Cron '{}' completed", action_str);
        Ok(ToolExecutionResult::success(message).with_data(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cron_manager_tool_name() {
        let tool = CronManagerTool::new();
        assert_eq!(tool.name(), "cron_manager");
    }

    #[test]
    fn test_cron_manager_schema() {
        let tool = CronManagerTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }
}
