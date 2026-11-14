//! Network diagnostic tool — ping, traceroute, port scan, DNS lookup.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::warn;

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Action types for network diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAction {
    Ping,
    Traceroute,
    Ss,
    Dig,
    Curl,
}

/// A single listening port entry.
#[derive(Debug, Clone, Serialize)]
pub struct SocketEntry {
    pub protocol: String,
    pub local_address: String,
    pub state: String,
    pub process: String,
}

/// Tool for network diagnostics on Linux.
#[derive(Debug)]
pub struct NetworkDiagTool;

impl Default for NetworkDiagTool {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkDiagTool {
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

    async fn do_ping(target: &str, count: u8) -> (bool, String) {
        match Self::run_cmd("ping", &["-c", &count.to_string(), target], 30).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute ping".to_string()),
        }
    }

    async fn do_traceroute(target: &str) -> (bool, String) {
        // Try traceroute first, then tracepath as fallback
        if let Some((success, output)) = Self::run_cmd("traceroute", &[target], 60).await {
            return (success, output);
        }
        if let Some((success, output)) = Self::run_cmd("tracepath", &[target], 60).await {
            return (success, output);
        }
        (false, "Neither traceroute nor tracepath is available".to_string())
    }

    async fn do_ss(port: Option<u16>) -> Vec<SocketEntry> {
        let port_str;
        let mut args: Vec<&str> = vec!["-tulnp", "--no-header"];
        if let Some(p) = port {
            port_str = format!(":{}", p);
            args.push("--sport");
            args.push(&port_str);
        }

        match Self::run_cmd("ss", &args, 15).await {
            Some((true, output)) => output
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 6 {
                        Some(SocketEntry {
                            protocol: parts[0].to_string(),
                            local_address: parts[4].to_string(),
                            state: parts[1].to_string(),
                            process: parts.last().unwrap_or(&"").to_string(),
                        })
                    } else {
                        None
                    }
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    async fn do_dig(domain: &str, record_type: &str) -> (bool, String) {
        match Self::run_cmd("dig", &["+short", record_type, domain], 15).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute dig".to_string()),
        }
    }

    async fn do_curl(url: &str) -> (bool, String) {
        match Self::run_cmd(
            "curl",
            &[
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code} %{time_total}",
                url,
            ],
            30,
        )
        .await
        {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute curl".to_string()),
        }
    }
}

#[async_trait]
impl Tool for NetworkDiagTool {
    fn name(&self) -> &str {
        "network_diag"
    }

    fn description(&self) -> &str {
        "Network diagnostic tools for Linux: ping, traceroute, port listing (ss), DNS lookup \
         (dig), and HTTP check (curl). Use to diagnose connectivity, DNS, routing, and port issues."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::observe(&["linux", "admin", "network"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Run a network diagnostic command",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: ping | traceroute | ss | dig | curl",
                    "enum": ["ping", "traceroute", "ss", "dig", "curl"]
                },
                "target": {
                    "type": "string",
                    "description": "Target host, IP, domain, or URL (depends on action)"
                },
                "port": {
                    "type": "integer",
                    "description": "Filter by port for 'ss' action"
                },
                "count": {
                    "type": "integer",
                    "description": "Ping packet count (default 4)",
                    "default": 4
                },
                "record_type": {
                    "type": "string",
                    "description": "DNS record type for 'dig' (default A)",
                    "default": "A"
                }
            }),
            vec!["action", "target"],
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
            .unwrap_or("ping");
        let action = match action_str {
            "traceroute" => NetworkAction::Traceroute,
            "ss" => NetworkAction::Ss,
            "dig" => NetworkAction::Dig,
            "curl" => NetworkAction::Curl,
            _ => NetworkAction::Ping,
        };

        let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
        if target.is_empty() {
            return Ok(ToolExecutionResult::error("'target' is required".to_string()));
        }

        let data = match action {
            NetworkAction::Ping => {
                let count = args.get("count").and_then(|v| v.as_i64()).unwrap_or(4) as u8;
                let (success, output) = Self::do_ping(target, count).await;
                serde_json::json!({
                    "action": "ping",
                    "target": target,
                    "success": success,
                    "output": output,
                })
            }
            NetworkAction::Traceroute => {
                let (success, output) = Self::do_traceroute(target).await;
                serde_json::json!({
                    "action": "traceroute",
                    "target": target,
                    "success": success,
                    "output": output,
                })
            }
            NetworkAction::Ss => {
                let port = args.get("port").and_then(|v| v.as_i64()).map(|v| v as u16);
                let sockets = Self::do_ss(port).await;
                serde_json::json!({
                    "action": "ss",
                    "sockets": sockets,
                    "count": sockets.len(),
                })
            }
            NetworkAction::Dig => {
                let record_type = args
                    .get("record_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("A");
                let (success, output) = Self::do_dig(target, record_type).await;
                serde_json::json!({
                    "action": "dig",
                    "domain": target,
                    "record_type": record_type,
                    "success": success,
                    "output": output,
                })
            }
            NetworkAction::Curl => {
                let (success, output) = Self::do_curl(target).await;
                serde_json::json!({
                    "action": "curl",
                    "url": target,
                    "success": success,
                    "output": output,
                })
            }
        };

        let success = data
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let message = format!("Network '{}' completed", action_str);

        if success {
            Ok(ToolExecutionResult::success(message).with_data(data))
        } else {
            Ok(ToolExecutionResult::error(
                data.get("output")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Network diagnostic failed")
                    .to_string(),
            )
            .with_data(data))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_diag_tool_name() {
        let tool = NetworkDiagTool::new();
        assert_eq!(tool.name(), "network_diag");
    }

    #[test]
    fn test_network_diag_schema() {
        let tool = NetworkDiagTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }
}
