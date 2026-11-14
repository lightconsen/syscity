//! User manager tool — list, add, remove, and modify system users and groups.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::warn;

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Action types for user management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAction {
    List,
    Add,
    Remove,
    Modify,
    Groups,
}

/// A user entry.
#[derive(Debug, Clone, Serialize)]
pub struct UserEntry {
    pub username: String,
    pub uid: String,
    pub gid: String,
    pub home: String,
    pub shell: String,
    pub full_name: Option<String>,
}

/// Tool for managing system users on Linux.
#[derive(Debug)]
pub struct UserManagerTool;

impl Default for UserManagerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl UserManagerTool {
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

    async fn do_list() -> Vec<UserEntry> {
        match Self::run_cmd("getent", &["passwd"], 15).await {
            Some((true, output)) => output
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.len() >= 7 {
                        Some(UserEntry {
                            username: parts[0].to_string(),
                            uid: parts[2].to_string(),
                            gid: parts[3].to_string(),
                            home: parts[5].to_string(),
                            shell: parts[6].to_string(),
                            full_name: Some(parts[4].to_string()).filter(|s| !s.is_empty()),
                        })
                    } else {
                        None
                    }
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    async fn do_add(username: &str, options: Option<&str>) -> (bool, String) {
        let args = vec!["-m", username];
        let opts: Vec<&str> = options
            .map(|s| s.split_whitespace().collect())
            .unwrap_or_default();
        let all_args: Vec<&str> = opts.iter().copied().chain(args).collect();
        match Self::run_cmd("useradd", &all_args, 30).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute useradd".to_string()),
        }
    }

    async fn do_remove(username: &str, remove_home: bool) -> (bool, String) {
        let args = if remove_home {
            vec!["-r", username]
        } else {
            vec![username]
        };
        match Self::run_cmd("userdel", &args, 30).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute userdel".to_string()),
        }
    }

    async fn do_modify(username: &str, options: &str) -> (bool, String) {
        let opts: Vec<&str> = options.split_whitespace().collect();
        let mut args = opts;
        args.push(username);
        match Self::run_cmd("usermod", &args, 30).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute usermod".to_string()),
        }
    }

    async fn do_groups(username: &str) -> (bool, String) {
        match Self::run_cmd("groups", &[username], 15).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute groups".to_string()),
        }
    }
}

#[async_trait]
impl Tool for UserManagerTool {
    fn name(&self) -> &str {
        "user_manager"
    }

    fn description(&self) -> &str {
        "Manage system users and groups on Linux. Supports listing users, adding/removing \
         accounts, modifying attributes, and checking group membership. Requires root privileges \
         for write operations."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::system_change(&["linux", "admin", "users"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Manage system users",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: list | add | remove | modify | groups",
                    "enum": ["list", "add", "remove", "modify", "groups"]
                },
                "username": {
                    "type": "string",
                    "description": "Username for add, remove, modify, groups actions"
                },
                "options": {
                    "type": "string",
                    "description": "Extra options for add/modify (e.g. '-s /bin/bash -G sudo')"
                },
                "remove_home": {
                    "type": "boolean",
                    "description": "Also remove home directory on remove (default false)",
                    "default": false
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
            "add" => UserAction::Add,
            "remove" => UserAction::Remove,
            "modify" => UserAction::Modify,
            "groups" => UserAction::Groups,
            _ => UserAction::List,
        };

        let data = match action {
            UserAction::List => {
                let users = Self::do_list().await;
                serde_json::json!({
                    "action": "list",
                    "count": users.len(),
                    "users": users,
                })
            }
            UserAction::Add => {
                let username = args.get("username").and_then(|v| v.as_str()).unwrap_or("");
                if username.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "'username' is required for add action".to_string(),
                    ));
                }
                let options = args.get("options").and_then(|v| v.as_str());
                let (success, output) = Self::do_add(username, options).await;
                serde_json::json!({
                    "action": "add",
                    "username": username,
                    "success": success,
                    "output": output,
                })
            }
            UserAction::Remove => {
                let username = args.get("username").and_then(|v| v.as_str()).unwrap_or("");
                if username.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "'username' is required for remove action".to_string(),
                    ));
                }
                let remove_home = args
                    .get("remove_home")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let (success, output) = Self::do_remove(username, remove_home).await;
                serde_json::json!({
                    "action": "remove",
                    "username": username,
                    "success": success,
                    "output": output,
                })
            }
            UserAction::Modify => {
                let username = args.get("username").and_then(|v| v.as_str()).unwrap_or("");
                let options = args.get("options").and_then(|v| v.as_str()).unwrap_or("");
                if username.is_empty() || options.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "'username' and 'options' are required for modify action".to_string(),
                    ));
                }
                let (success, output) = Self::do_modify(username, options).await;
                serde_json::json!({
                    "action": "modify",
                    "username": username,
                    "success": success,
                    "output": output,
                })
            }
            UserAction::Groups => {
                let username = args.get("username").and_then(|v| v.as_str()).unwrap_or("");
                if username.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "'username' is required for groups action".to_string(),
                    ));
                }
                let (success, output) = Self::do_groups(username).await;
                serde_json::json!({
                    "action": "groups",
                    "username": username,
                    "success": success,
                    "output": output,
                })
            }
        };

        let message = format!("User '{}' completed", action_str);
        Ok(ToolExecutionResult::success(message).with_data(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_manager_tool_name() {
        let tool = UserManagerTool::new();
        assert_eq!(tool.name(), "user_manager");
    }

    #[test]
    fn test_user_manager_schema() {
        let tool = UserManagerTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }
}
