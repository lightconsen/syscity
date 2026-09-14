//! AppleScript execution tool for macOS automation.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Result of an AppleScript execution.
#[derive(Debug, Clone, Serialize)]
pub struct AppleScriptResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

/// Execute AppleScript on macOS using `osascript`.
#[derive(Debug)]
pub struct AppleScriptTool;

impl Default for AppleScriptTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleScriptTool {
    pub fn new() -> Self {
        Self
    }

    /// Execute an AppleScript string and return the result.
    pub async fn execute_script(script: &str, timeout_secs: u64) -> AppleScriptResult {
        info!("Executing AppleScript ({} chars)", script.len());

        let script_owned = script.to_string();

        // Spawn child first so we can kill it if the timeout fires.
        let mut child = match Command::new("osascript")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to spawn osascript: {}", e);
                return AppleScriptResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to spawn osascript: {}", e)),
                };
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(script_owned.as_bytes()).await {
                warn!("Failed to write to osascript stdin: {}", e);
            }
        }

        let mut child_opt = Some(child);
        let wait_result = timeout(Duration::from_secs(timeout_secs), async {
            let Some(c) = child_opt.take() else {
                return Err(std::io::Error::other("child handle missing"));
            };
            c.wait_with_output().await
        })
        .await;

        match wait_result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                AppleScriptResult {
                    success: output.status.success(),
                    output: stdout,
                    error: if output.status.success() || stderr.is_empty() {
                        None
                    } else {
                        Some(stderr)
                    },
                }
            }
            Ok(Err(e)) => {
                warn!("Failed to collect osascript output: {}", e);
                AppleScriptResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to collect osascript output: {}", e)),
                }
            }
            Err(_) => {
                warn!("osascript timed out after {}s, killing process", timeout_secs);
                if let Some(mut c) = child_opt {
                    let _ = c.kill().await;
                }
                AppleScriptResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("osascript timed out after {} seconds", timeout_secs)),
                }
            }
        }
    }
}

#[async_trait]
impl Tool for AppleScriptTool {
    fn name(&self) -> &str {
        "applescript"
    }

    fn description(&self) -> &str {
        "Execute AppleScript on macOS using `osascript`. Use to automate macOS applications, query \
         UI state, control system settings, or interact with app-specific APIs."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Execute AppleScript",
            serde_json::json!({
                "script": {
                    "type": "string",
                    "description": "The AppleScript source code to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds",
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
        let script = args["script"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'script' argument".to_string())
        })?;

        let timeout_secs = args["timeout"].as_u64().unwrap_or(30);

        let result = Self::execute_script(script, timeout_secs).await;

        let json = serde_json::to_string_pretty(&result)
            .map_err(crate::error::SyscityError::Serialization)?;

        if result.success {
            Ok(ToolExecutionResult::success(json).with_data(serde_json::to_value(result)?))
        } else {
            Ok(ToolExecutionResult::error(result.error.clone().unwrap_or_default())
                .with_data(serde_json::to_value(result)?))
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        cfg!(target_os = "macos")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_applescript_tool_creation() {
        let tool = AppleScriptTool::new();
        assert_eq!(tool.name(), "applescript");
        assert!(tool.description().contains("AppleScript"));
    }
}
