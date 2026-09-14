//! Windows PowerShell script execution tool.

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Execute PowerShell scripts on Windows.
///
/// This is a more powerful alternative to the generic shell tool,
/// leveraging Windows-native APIs and cmdlets.
#[derive(Debug)]
pub struct PowerShellTool;

impl Default for PowerShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl PowerShellTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for PowerShellTool {
    fn name(&self) -> &str {
        "windows_powershell"
    }

    fn description(&self) -> &str {
        "Execute PowerShell scripts on Windows. Provides full access to .NET APIs, WMI, COM \
         objects, and Windows management cmdlets."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Execute a PowerShell script",
            serde_json::json!({
                "script": {
                    "type": "string",
                    "description": "PowerShell script to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 30)",
                    "default": 30
                }
            }),
            vec!["script"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // One pointer and one focus: two conversations must not drive this
        // display at once. See `tools::target_lock`.
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::DESKTOP)
            .await;
        let script = args
            .get("script")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if script.trim().is_empty() {
            return Ok(ToolExecutionResult::error("No script provided".to_string()));
        }

        let timeout_secs = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);

        info!("Executing PowerShell script ({} chars)", script.len());

        let output = timeout(
            Duration::from_secs(timeout_secs),
            Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    &script,
                ])
                .output(),
        )
        .await;

        match output {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);

                if !stderr.trim().is_empty() {
                    warn!("PowerShell stderr: {}", stderr);
                }

                let mut result = if out.status.success() {
                    ToolExecutionResult::success(format!("Exit code: 0\n\n{}", stdout.trim()))
                } else {
                    let code = out.status.code().unwrap_or(-1);
                    ToolExecutionResult::error(format!(
                        "Exit code: {}\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
                        code,
                        stdout.trim(),
                        stderr.trim()
                    ))
                };

                result.data = Some(serde_json::json!({
                    "exit_code": out.status.code(),
                    "stdout": stdout.as_ref(),
                    "stderr": stderr.as_ref(),
                }));

                Ok(result)
            }
            Ok(Err(e)) => {
                Ok(ToolExecutionResult::error(format!("Failed to run PowerShell: {}", e)))
            }
            Err(_) => Ok(ToolExecutionResult::error("PowerShell script timed out".to_string())),
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        cfg!(target_os = "windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_powershell_tool_creation() {
        let tool = PowerShellTool::new();
        assert_eq!(tool.name(), "windows_powershell");
    }
}
