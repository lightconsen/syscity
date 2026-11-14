//! Log analyzer tool — read and search system and application logs.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Action types for log analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogAction {
    Read,
    Search,
    Follow,
}

/// A single log line.
#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub timestamp: Option<String>,
    pub unit: Option<String>,
    pub level: Option<String>,
    pub message: String,
}

/// Tool for reading and searching system logs.
#[derive(Debug)]
pub struct LogAnalyzerTool;

impl Default for LogAnalyzerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl LogAnalyzerTool {
    pub fn new() -> Self {
        Self
    }

    async fn read_journalctl(
        service: Option<&str>,
        lines: usize,
        since: Option<&str>,
    ) -> Vec<LogLine> {
        let lines_str = lines.to_string();
        let mut args: Vec<&str> = vec!["--no-pager", "-n", &lines_str];
        if let Some(svc) = service {
            args.push("-u");
            args.push(svc);
        }
        if let Some(s) = since {
            args.push("--since");
            args.push(s);
        }

        let result =
            timeout(Duration::from_secs(15), Command::new("journalctl").args(&args).output()).await;

        match result {
            Ok(Ok(output)) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .lines()
                    .map(|line| {
                        // Try to parse: "Jan 15 10:23:00 hostname service[level]: message"
                        // or systemd-style: "Jan 15 10:23:00 app[1234]: message"
                        if line.len() < 20 {
                            return LogLine {
                                timestamp: None,
                                unit: None,
                                level: None,
                                message: line.to_string(),
                            };
                        }
                        let ts = &line[..15]; // "Jan 15 10:23:00"
                        let rest = &line[16..];
                        // Find first ':' to separate prefix from message
                        if let Some(pos) = rest.find(':') {
                            let prefix = &rest[..pos];
                            let message = rest[pos + 1..].trim().to_string();
                            // Extract unit and level from prefix like "nginx[1234]" or "app[1234]:"
                            let unit = prefix.split('[').next().map(|s| s.to_string());
                            let level = None; // journalctl doesn't always include level inline
                            LogLine {
                                timestamp: Some(ts.to_string()),
                                unit,
                                level,
                                message,
                            }
                        } else {
                            LogLine {
                                timestamp: Some(ts.to_string()),
                                unit: None,
                                level: None,
                                message: rest.to_string(),
                            }
                        }
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    async fn grep_logs(pattern: &str, path: Option<&str>, lines: usize) -> Vec<LogLine> {
        let cmd = if let Some(p) = path {
            format!("grep -i '{}' {} | tail -n {}", pattern, p, lines)
        } else {
            format!("journalctl --no-pager | grep -i '{}' | tail -n {}", pattern, lines)
        };

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let result =
            timeout(Duration::from_secs(15), Command::new(&shell).arg("-c").arg(&cmd).output())
                .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .lines()
                    .map(|line| LogLine {
                        timestamp: None,
                        unit: None,
                        level: None,
                        message: line.to_string(),
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }
}

#[async_trait]
impl Tool for LogAnalyzerTool {
    fn name(&self) -> &str {
        "log_analyzer"
    }

    fn description(&self) -> &str {
        "Read and search system logs using journalctl or grep. Supports reading recent logs, \
         filtering by service, time range, and pattern search."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::observe(&["linux", "admin", "logs"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Analyze system logs",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: read | search | follow",
                    "enum": ["read", "search", "follow"]
                },
                "service": {
                    "type": "string",
                    "description": "Service name filter (e.g. nginx, sshd). Only for 'read' action with journalctl."
                },
                "lines": {
                    "type": "integer",
                    "description": "Maximum lines to return",
                    "default": 50
                },
                "since": {
                    "type": "string",
                    "description": "Time range for 'read' (e.g. '1 hour ago', '2024-01-01'). journalctl syntax."
                },
                "pattern": {
                    "type": "string",
                    "description": "Search pattern for 'search' action (case-insensitive grep)."
                },
                "path": {
                    "type": "string",
                    "description": "Log file path for 'search' (e.g. /var/log/nginx/error.log). If omitted, searches journalctl."
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
            .unwrap_or("read");
        let action = match action_str {
            "search" => LogAction::Search,
            "follow" => LogAction::Follow,
            _ => LogAction::Read,
        };

        let lines = args.get("lines").and_then(|v| v.as_i64()).unwrap_or(50) as usize;

        let logs = match action {
            LogAction::Read => {
                let service = args.get("service").and_then(|v| v.as_str());
                let since = args.get("since").and_then(|v| v.as_str());
                Self::read_journalctl(service, lines, since).await
            }
            LogAction::Search => {
                let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                let path = args.get("path").and_then(|v| v.as_str());
                if pattern.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "'pattern' is required for search action".to_string(),
                    ));
                }
                Self::grep_logs(pattern, path, lines).await
            }
            LogAction::Follow => {
                // Follow is read-only; return a limited read instead
                let service = args.get("service").and_then(|v| v.as_str());
                Self::read_journalctl(service, lines, None).await
            }
        };

        if logs.is_empty() {
            return Ok(ToolExecutionResult::success("No logs found".to_string()));
        }

        let data = serde_json::json!({
            "lines": logs.len(),
            "logs": logs,
        });

        Ok(ToolExecutionResult::success(format!("Retrieved {} log lines", logs.len()))
            .with_data(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_analyzer_tool_name() {
        let tool = LogAnalyzerTool::new();
        assert_eq!(tool.name(), "log_analyzer");
    }

    #[test]
    fn test_log_analyzer_schema() {
        let tool = LogAnalyzerTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }
}
