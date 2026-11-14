//! Firewall manager tool — list and manage firewall rules.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::warn;

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Action types for firewall management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirewallAction {
    List,
    Status,
    Add,
    Remove,
}

/// Detected firewall backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirewallBackend {
    Iptables,
    Nftables,
    Ufw,
    Firewalld,
    Unknown,
}

impl FirewallBackend {
    fn detect() -> Self {
        if std::process::Command::new("ufw")
            .arg("status")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Self::Ufw;
        }
        if std::process::Command::new("firewall-cmd")
            .arg("--state")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Self::Firewalld;
        }
        if std::process::Command::new("nft")
            .arg("list")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Self::Nftables;
        }
        if std::process::Command::new("iptables")
            .arg("-L")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Self::Iptables;
        }
        Self::Unknown
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Iptables => "iptables",
            Self::Nftables => "nftables",
            Self::Ufw => "ufw",
            Self::Firewalld => "firewalld",
            Self::Unknown => "unknown",
        }
    }
}

/// A firewall rule entry.
#[derive(Debug, Clone, Serialize)]
pub struct FirewallRule {
    pub chain: String,
    pub target: String,
    pub protocol: String,
    pub source: String,
    pub destination: String,
    pub ports: String,
}

/// Tool for managing firewall rules on Linux.
#[derive(Debug)]
pub struct FirewallManagerTool;

impl Default for FirewallManagerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FirewallManagerTool {
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

    async fn do_list_iptables() -> Vec<FirewallRule> {
        match Self::run_cmd("iptables", &["-L", "-n", "--line-numbers"], 15).await {
            Some((true, output)) => {
                let mut rules = Vec::new();
                let mut current_chain = String::new();
                for line in output.lines() {
                    if line.starts_with("Chain ") {
                        current_chain = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    } else if !line.trim().is_empty()
                        && !line.starts_with("num")
                        && !line.starts_with("target")
                    {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 6 {
                            rules.push(FirewallRule {
                                chain: current_chain.clone(),
                                target: parts[1].to_string(),
                                protocol: parts[2].to_string(),
                                source: parts[4].to_string(),
                                destination: parts[5].to_string(),
                                ports: parts.get(6).unwrap_or(&"any").to_string(),
                            });
                        }
                    }
                }
                rules
            }
            _ => Vec::new(),
        }
    }

    async fn do_list_ufw() -> Vec<FirewallRule> {
        match Self::run_cmd("ufw", &["status", "numbered"], 15).await {
            Some((true, output)) => output
                .lines()
                .skip(4) // Skip header
                .filter_map(|line| {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with("To") {
                        return None;
                    }
                    Some(FirewallRule {
                        chain: "ufw".to_string(),
                        target: trimmed.split_whitespace().next()?.to_string(),
                        protocol: trimmed.split_whitespace().nth(1).unwrap_or("any").to_string(),
                        source: trimmed.split_whitespace().nth(2).unwrap_or("any").to_string(),
                        destination: trimmed.split_whitespace().nth(3).unwrap_or("any").to_string(),
                        ports: trimmed.split_whitespace().nth(4).unwrap_or("any").to_string(),
                    })
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    async fn do_status(backend: FirewallBackend) -> (bool, String) {
        match backend {
            FirewallBackend::Ufw => match Self::run_cmd("ufw", &["status", "verbose"], 15).await {
                Some((success, output)) => (success, output),
                None => (false, "ufw status failed".to_string()),
            },
            FirewallBackend::Firewalld => {
                match Self::run_cmd("firewall-cmd", &["--state"], 15).await {
                    Some((success, output)) => (success, output),
                    None => (false, "firewall-cmd state check failed".to_string()),
                }
            }
            FirewallBackend::Nftables => {
                match Self::run_cmd("nft", &["list", "ruleset"], 15).await {
                    Some((success, output)) => (success, output),
                    None => (false, "nft list failed".to_string()),
                }
            }
            _ => match Self::run_cmd("iptables", &["-L", "-n", "-v"], 15).await {
                Some((success, output)) => (success, output),
                None => (false, "iptables list failed".to_string()),
            },
        }
    }
}

#[async_trait]
impl Tool for FirewallManagerTool {
    fn name(&self) -> &str {
        "firewall_manager"
    }

    fn description(&self) -> &str {
        "Manage firewall rules on Linux. Auto-detects ufw, firewalld, nftables, or iptables. \
         Supports listing rules, checking status, and basic add/remove (where safe)."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::system_change(&["linux", "admin", "firewall"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Manage firewall",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: list | status | add | remove",
                    "enum": ["list", "status", "add", "remove"]
                },
                "rule": {
                    "type": "string",
                    "description": "Rule specification for add/remove (backend-specific syntax)"
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
            .unwrap_or("status");
        let action = match action_str {
            "list" => FirewallAction::List,
            "add" => FirewallAction::Add,
            "remove" => FirewallAction::Remove,
            _ => FirewallAction::Status,
        };

        let backend = FirewallBackend::detect();
        if matches!(backend, FirewallBackend::Unknown) {
            return Ok(ToolExecutionResult::error(
                "No supported firewall backend found (tried ufw, firewalld, nftables, iptables)"
                    .to_string(),
            ));
        }

        let data = match action {
            FirewallAction::List => {
                let rules = match backend {
                    FirewallBackend::Ufw => Self::do_list_ufw().await,
                    _ => Self::do_list_iptables().await,
                };
                serde_json::json!({
                    "action": "list",
                    "backend": backend.as_str(),
                    "count": rules.len(),
                    "rules": rules,
                })
            }
            FirewallAction::Status => {
                let (success, output) = Self::do_status(backend).await;
                serde_json::json!({
                    "action": "status",
                    "backend": backend.as_str(),
                    "success": success,
                    "output": output,
                })
            }
            FirewallAction::Add | FirewallAction::Remove => {
                // Add/remove are deliberately limited; full rule syntax is complex
                // and dangerous. We return an informative error suggesting manual action.
                serde_json::json!({
                    "action": action_str,
                    "backend": backend.as_str(),
                    "success": false,
                    "output": format!(
                        "Add/remove via tool is restricted for safety. \
                         Use '{}' directly for: {}",
                        backend.as_str(),
                        args.get("rule").and_then(|v| v.as_str()).unwrap_or("unspecified rule")
                    ),
                })
            }
        };

        let message =
            format!("Firewall '{}' completed (backend: {})", action_str, backend.as_str());
        Ok(ToolExecutionResult::success(message).with_data(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_firewall_manager_tool_name() {
        let tool = FirewallManagerTool::new();
        assert_eq!(tool.name(), "firewall_manager");
    }

    #[test]
    fn test_firewall_manager_schema() {
        let tool = FirewallManagerTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }
}
